//! Autonomous application runtime: commands, presence/retries, UI notifications
//! and delayed status refreshes all run on Rust threads, without Qt timers.
use std::ffi::{c_void, CString};
use std::os::raw::c_char;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use serde::Deserialize;

use crate::core::Core;
use crate::cpukeepalive::CpuKeepAlive;
use crate::notification::{Notification, PhoneKeyLink};
use crate::phone_key::PresenceEvent;

pub type UiCallback = unsafe extern "C" fn(*mut c_void, *const c_char);

struct Observer {
    callback: UiCallback,
    context: usize,
}

#[derive(Default)]
struct ObserverState {
    observer: Option<Observer>,
    in_flight: usize,
}

#[derive(Default)]
struct Notifications {
    observer: Mutex<ObserverState>,
    idle: Condvar,
    relay: Option<async_channel::Sender<Completion>>,
}

enum Completion {
    Notification(Notification),
    RefreshAt(Instant),
}

impl Notifications {
    fn send(&self, value: &Notification) {
        if let Some(relay) = &self.relay {
            // One serial executor produces completions; the control loop
            // consumes this bounded channel independently of BLE admission.
            let _ = relay.send_blocking(Completion::Notification(value.clone()));
            return;
        }
        // Clone the observer under the lock, then invoke the callback with
        // no lock held: the callback must never re-enter us (detach during
        // delivery would deadlock on the same mutex).
        let mut state = self
            .observer
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let observer = state.observer.as_ref().map(|o| (o.callback, o.context));
        if observer.is_some() {
            state.in_flight += 1;
        }
        drop(state);
        if let Some((callback, context)) = observer {
            let Some(data) = serde_json::to_string(value)
                .ok()
                .and_then(|json| CString::new(json).ok())
            else {
                self.finish_delivery();
                return;
            };
            // SAFETY: detachment waits for in-flight callbacks before returning.
            unsafe { callback(context as *mut c_void, data.as_ptr()) };
            self.finish_delivery();
        }
    }

    fn finish_delivery(&self) {
        let mut state = self
            .observer
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.in_flight -= 1;
        self.idle.notify_all();
    }

    fn observe(&self, observer: Option<Observer>) {
        let mut state = self
            .observer
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.observer = None;
        while state.in_flight != 0 {
            state = self
                .idle
                .wait(state)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
        }
        state.observer = observer;
    }
}

#[derive(Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum Request {
    Run {
        request_id: String,
        cmd: String,
        args: Vec<String>,
        #[serde(default)]
        refresh_status: bool,
    },
    GenerateKey {
        force: bool,
    },
    Pair,
    SetConfig {
        vin: String,
        model: String,
        key_name: String,
        connect_timeout_sec: i32,
        command_timeout_sec: i32,
        cover_left_action: Option<crate::cover_action::CoverAction>,
        cover_right_action: Option<crate::cover_action::CoverAction>,
    },
    /// Applies the status-notification settings on their own (Settings
    /// switches take effect immediately, without the page's Save).
    SetNotification {
        enabled: bool,
        actions: Vec<crate::cover_action::CoverAction>,
    },
    GetConfig,
    ExportAutomagic {
        documents_dir: String,
    },
    PreviewDestination {
        request_id: String,
        text: String,
    },
    ShareDestination {
        request_id: String,
        text: String,
    },
    ApplicationState {
        state: AppState,
    },
    LogUi {
        message: String,
    },
    Snapshot,
    Shutdown,
}

/// `Qt::ApplicationState`, as named by the Qt client.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, strum::Display)]
#[serde(rename_all = "lowercase")]
#[strum(serialize_all = "lowercase")]
pub(crate) enum AppState {
    Active,
    Inactive,
    Hidden,
    Suspended,
    #[serde(other)]
    Unknown,
}

/// Messages for the serial BLE executor. `Wake` asks it to re-evaluate
/// lifecycle state (resume, presence stopped, shutdown) without a request.
enum BleMessage {
    Request(Request),
    Wake,
}

/// Longest BLE-executor sleep; every lifecycle change also wakes it.
const BLE_IDLE_WAIT: Duration = Duration::from_secs(30);

pub struct Runtime {
    requests: async_channel::Sender<Request>,
    ble_requests: mpsc::SyncSender<BleMessage>,
    ble_worker: Option<JoinHandle<()>>,
    shutdown: Arc<AtomicBool>,
    core: Arc<Core>,
    notifications: Arc<Notifications>,
    worker: Option<JoinHandle<()>>,
    keepalive: Option<CpuKeepAlive>,
}

impl Runtime {
    pub(crate) fn new(core: Core) -> Self {
        Self::launch(core, true)
    }

    fn launch(core: Core, enable_lease: bool) -> Self {
        let core = Arc::new(core);
        let keepalive = enable_lease.then(|| CpuKeepAlive::new(Arc::clone(&core)));
        let notifications = Arc::new(Notifications::default());
        let sink = Arc::clone(&notifications);
        let (requests, receiver) = async_channel::bounded(64);
        let (ble_requests, ble_receiver) = mpsc::sync_channel(64);
        let (completed, completions) = async_channel::bounded(64);
        let shutdown = Arc::new(AtomicBool::new(false));
        let resume = Arc::new(AtomicBool::new(false));
        let stopped = Arc::new(AtomicU64::new(0));
        let ble_worker = {
            let core = Arc::clone(&core);
            let shutdown = Arc::clone(&shutdown);
            let resume = Arc::clone(&resume);
            let stopped = Arc::clone(&stopped);
            thread::spawn(move || {
                run_ble(
                    &core,
                    &ble_receiver,
                    &completed,
                    &shutdown,
                    &resume,
                    &stopped,
                );
            })
        };
        let worker = {
            let core = Arc::clone(&core);
            let shutdown = Arc::clone(&shutdown);
            let resume = Arc::clone(&resume);
            let stopped = Arc::clone(&stopped);
            let lifecycle = Lifecycle {
                resume,
                stopped,
                wake: ble_requests.clone(),
            };
            thread::spawn(move || {
                async_io::block_on(run(
                    &core,
                    receiver,
                    completions,
                    &sink,
                    &shutdown,
                    &lifecycle,
                ));
            })
        };
        Self {
            requests,
            ble_requests,
            ble_worker: Some(ble_worker),
            shutdown,
            core,
            notifications,
            worker: Some(worker),
            keepalive,
        }
    }

    pub(crate) fn submit(&self, json: &str) -> bool {
        if self.shutdown.load(Ordering::SeqCst) {
            return false;
        }
        serde_json::from_str(json).is_ok_and(|request| match request {
            Request::Shutdown => {
                self.begin_shutdown();
                true
            }
            Request::Run { .. }
            | Request::Pair
            | Request::GenerateKey { .. }
            | Request::SetConfig { .. }
            | Request::ShareDestination { .. } => self
                .ble_requests
                .try_send(BleMessage::Request(request))
                .is_ok(),
            _ => self.requests.try_send(request).is_ok(),
        })
    }

    /// Stops admission and cancels protocol work without blocking the caller.
    fn begin_shutdown(&self) {
        self.shutdown.store(true, Ordering::SeqCst);
        self.core.cancel_session();
        self.requests.close();
        // A full queue already guarantees the executor wakes up.
        let _ = self.ble_requests.try_send(BleMessage::Wake);
    }

    /// # Safety
    /// The context must remain live until detachment returns. Callbacks must
    /// copy the borrowed JSON and enqueue UI work; never block or re-enter us.
    pub(crate) unsafe fn observe(&self, callback: Option<UiCallback>, context: *mut c_void) {
        self.notifications
            .observe(callback.map(|callback| Observer {
                callback,
                context: context as usize,
            }));
        if callback.is_some() {
            // Snapshot must never be silently dropped: without "initialized"
            // the UI spinners never stop. try_send first (never block the UI
            // thread on a full control queue); on Full, deliver "initialized"
            // directly. The next state change publishes phone-key state.
            if self.requests.try_send(Request::Snapshot).is_err() {
                self.notifications
                    .send(&Notification::Initialized { ok: true });
            }
        }
    }
}

impl Drop for Runtime {
    fn drop(&mut self) {
        // Detach before joining so shutdown cannot call into a destroyed UI.
        self.notifications.observe(None);
        self.begin_shutdown();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
        if let Some(worker) = self.ble_worker.take() {
            let _ = worker.join();
        }
        // The worker stopped mode before this releases the lease and drops
        // the final core reference (which reaps Go).
        self.keepalive.take();
    }
}

struct PhoneState {
    active: bool,
    status: String,
    link: PhoneKeyLink,
    generation: u64,
    away: crate::phone_key::AwayDeadline,
}

impl PhoneState {
    fn publish(&self, sink: &Notifications) {
        sink.send(&Notification::PhoneKeyState {
            active: self.active,
            link: self.link,
            status: self.status.clone(),
            generation: self.generation,
        });
    }

    fn publish_due_away(&mut self, core: &Core, sink: &Notifications, now: Instant) {
        if let Some(vin) = self
            .away
            .take_due(now, &core.vin(), core.session_generation())
        {
            let kind = PresenceEvent::AwayConfirmed;
            crate::keylog::log("presence", &format!("{kind} vin={vin}"));
            sink.send(&Notification::PhoneKeyEvent {
                kind,
                vin,
                time: crate::keylog::utc_stamp(),
                error: String::new(),
                generation: core.session_generation(),
            });
        }
    }

    fn accept_completion(&mut self, core: &Core, event: &Notification) -> bool {
        if let Notification::PhoneKeyEvent {
            kind,
            vin,
            generation,
            ..
        } = event
        {
            if *generation < core.session_generation() {
                return false;
            }
            self.away.observe(*kind, vin, *generation, Instant::now());
        }
        if let Notification::PhoneKeyState {
            active,
            link,
            status,
            generation,
        } = event
        {
            if *generation < core.session_generation()
                || *generation < self.generation
                || (*generation != 0
                    && *generation == self.generation
                    && *link == PhoneKeyLink::Scanning)
            {
                return false;
            }
            self.generation = *generation;
            self.active = *active;
            self.status.clone_from(status);
            self.link = *link;
        }
        true
    }

    fn start(&mut self, core: &Core, sink: &Notifications) {
        let result = core.start_phone_key();
        self.generation = core.session_generation();
        self.started(result, sink);
    }

    fn started(&mut self, result: Result<(), crate::error::OperationError>, sink: &Notifications) {
        match result {
            Ok(()) => {
                self.active = true;
                self.status = "Phone key scanning".into();
                self.link = PhoneKeyLink::Scanning;
            }
            Err(error) => {
                self.active = false;
                self.status = error.to_string();
                self.link = match error {
                    crate::error::OperationError::NotPaired => PhoneKeyLink::Unpaired,
                    crate::error::OperationError::BluetoothOff(_) => PhoneKeyLink::BluetoothOff,
                    _ => PhoneKeyLink::Error,
                };
            }
        }
        self.publish(sink);
    }

    /// Publishes queued presence events. Returns whether a stop was recorded
    /// in `stopped`, i.e. whether the BLE executor must be woken.
    fn poll(&mut self, core: &Core, sink: &Notifications, stopped: &AtomicU64) -> bool {
        let mut stop_seen = false;
        while let Some(mut event) = core.drain_phone_key_event() {
            if event.generation < core.session_generation() {
                continue;
            }
            self.generation = event.generation;
            if event.vin.is_empty() {
                event.vin = core.vin();
            }
            if event.kind == PresenceEvent::Stopped {
                stopped.fetch_max(event.generation, Ordering::SeqCst);
                stop_seen = true;
            }
            crate::keylog::log(
                "presence",
                &if event.error.is_empty() {
                    event.kind.to_string()
                } else {
                    format!("{} error={}", event.kind, event.error)
                },
            );
            self.away
                .observe(event.kind, &event.vin, event.generation, Instant::now());
            let (active, link, status) = match event.kind {
                PresenceEvent::Near => (
                    Some(true),
                    PhoneKeyLink::Connected,
                    Some("Phone key connected".to_string()),
                ),
                PresenceEvent::AuthOk => (
                    None,
                    PhoneKeyLink::Authorized,
                    Some("Phone key authorized".to_string()),
                ),
                PresenceEvent::Restarted => (
                    Some(true),
                    PhoneKeyLink::Scanning,
                    Some("Phone key scanning".to_string()),
                ),
                PresenceEvent::Far | PresenceEvent::Disconnected => (
                    None,
                    PhoneKeyLink::Scanning,
                    Some("Phone key scanning".to_string()),
                ),
                PresenceEvent::Stopped => (
                    Some(false),
                    PhoneKeyLink::Stopped,
                    Some(with_error("Phone key stopped", &event.error)),
                ),
                PresenceEvent::Error | PresenceEvent::AuthFailed => (
                    None,
                    PhoneKeyLink::Error,
                    Some(with_error("Phone key error", &event.error)),
                ),
                PresenceEvent::Inside | PresenceEvent::AwayConfirmed => (None, self.link, None),
            };
            if let Some(active) = active {
                self.active = active;
            }
            self.link = if event.bluetooth_off {
                PhoneKeyLink::BluetoothOff
            } else {
                link
            };
            if let Some(status) = status {
                self.status = status;
            }
            let time = if event.time.is_empty() {
                crate::keylog::utc_stamp()
            } else {
                event.time
            };
            sink.send(&Notification::PhoneKeyEvent {
                kind: event.kind,
                vin: event.vin,
                time,
                error: event.error,
                generation: event.generation,
            });
            self.publish(sink);
        }
        stop_seen
    }
}

/// State the control loop shares with the BLE executor.
struct Lifecycle {
    resume: Arc<AtomicBool>,
    stopped: Arc<AtomicU64>,
    wake: mpsc::SyncSender<BleMessage>,
}

impl Lifecycle {
    fn wake(&self) {
        // A full queue already guarantees the executor wakes up.
        let _ = self.wake.try_send(BleMessage::Wake);
    }
}

fn with_error(status: &str, error: &str) -> String {
    if error.is_empty() {
        status.into()
    } else {
        format!("{status}: {error}")
    }
}

enum Wake {
    Request(Result<Request, async_channel::RecvError>),
    Completion(Result<Completion, async_channel::RecvError>),
    Timer,
    Presence,
    Export(Notification),
}

async fn wait_wake(
    core: &Core,
    receiver: &async_channel::Receiver<Request>,
    completions: &async_channel::Receiver<Completion>,
    export: Option<&async_channel::Receiver<Notification>>,
    deadline: Instant,
) -> Wake {
    futures_lite::future::race(
        futures_lite::future::race(async { Wake::Request(receiver.recv().await) }, async {
            match export {
                Some(finished) => Wake::Export(finished.recv().await.unwrap_or_else(|_| {
                    Notification::AutomagicExportFinished {
                        ok: false,
                        path: String::new(),
                        error: "Automagic export failed".into(),
                    }
                })),
                None => std::future::pending().await,
            }
        }),
        futures_lite::future::race(
            async { Wake::Completion(completions.recv().await) },
            futures_lite::future::race(
                async {
                    async_io::Timer::at(deadline).await;
                    Wake::Timer
                },
                async {
                    core.wait_phone_key_event().await;
                    Wake::Presence
                },
            ),
        ),
    )
    .await
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
async fn run(
    core: &Core,
    receiver: async_channel::Receiver<Request>,
    completions: async_channel::Receiver<Completion>,
    sink: &Notifications,
    shutdown: &AtomicBool,
    lifecycle: &Lifecycle,
) {
    let mut phone = PhoneState {
        active: false,
        status: "Phone key inactive".into(),
        link: PhoneKeyLink::Stopped,
        generation: 0,
        away: crate::phone_key::AwayDeadline::default(),
    };
    let mut poll_at = Instant::now();
    let mut refresh_at = None;
    let mut suspended = false;
    let mut export: Option<async_channel::Receiver<Notification>> = None;
    loop {
        if shutdown.load(Ordering::SeqCst) {
            break;
        }
        // Drain already queued presence events before expiring a deadline, so
        // an inside event at the boundary wins over delayed-away publication.
        if phone.poll(core, sink, &lifecycle.stopped) {
            lifecycle.wake();
        }
        let now = Instant::now();
        if now >= poll_at {
            poll_at = Instant::now() + Duration::from_secs(1);
        }
        phone.publish_due_away(core, sink, now);
        if refresh_at.is_some_and(|deadline| now >= deadline) {
            refresh_at = None;
            sink.send(&Notification::StatusRefreshRequested);
        }
        let deadline = refresh_at.map_or(poll_at, |refresh| poll_at.min(refresh));
        let deadline = phone
            .away
            .deadline()
            .map_or(deadline, |away| deadline.min(away));
        let wake = wait_wake(core, &receiver, &completions, export.as_ref(), deadline).await;
        if shutdown.load(Ordering::SeqCst) {
            break;
        }
        match wake {
            Wake::Request(Ok(Request::Shutdown) | Err(_)) | Wake::Completion(Err(_)) => break,
            Wake::Timer => (),
            Wake::Presence => {
                if phone.poll(core, sink, &lifecycle.stopped) {
                    lifecycle.wake();
                }
            }
            Wake::Export(event) => {
                export = None;
                sink.send(&event);
            }
            Wake::Request(Ok(Request::ExportAutomagic { documents_dir })) => {
                if export.is_some() {
                    sink.send(&Notification::AutomagicExportFinished {
                        ok: false,
                        path: String::new(),
                        error: "An Automagic export is already in progress".into(),
                    });
                    continue;
                }
                // Filesystem I/O must not stall either the serial control loop
                // or BLE maintenance. There is at most one export task.
                let (finished, result) = async_channel::bounded(1);
                export = Some(result);
                thread::spawn(move || {
                    let _ = finished.send_blocking(
                        match crate::automagic::export(std::path::Path::new(&documents_dir)) {
                            Ok(path) => Notification::AutomagicExportFinished {
                                ok: true,
                                path: path.to_string_lossy().into_owned(),
                                error: String::new(),
                            },
                            Err(error) => Notification::AutomagicExportFinished {
                                ok: false,
                                path: String::new(),
                                error: error.to_string(),
                            },
                        },
                    );
                });
            }
            Wake::Completion(Ok(Completion::RefreshAt(deadline))) => refresh_at = Some(deadline),
            Wake::Completion(Ok(Completion::Notification(event))) => {
                if phone.accept_completion(core, &event) {
                    sink.send(&event);
                }
            }
            Wake::Request(Ok(Request::ApplicationState { state })) => {
                crate::keylog::log("ui", &format!("applicationState={state}"));
                if state == AppState::Suspended {
                    suspended = true;
                } else if state == AppState::Active && suspended {
                    suspended = false;
                    lifecycle.resume.store(true, Ordering::SeqCst);
                    lifecycle.wake();
                }
            }
            Wake::Request(Ok(Request::LogUi { message })) => crate::keylog::log("ui", &message),
            Wake::Request(Ok(Request::Snapshot)) => {
                sink.send(&Notification::Initialized { ok: true });
                phone.publish(sink);
            }
            Wake::Request(Ok(request)) => {
                dispatch(core, request, sink, &mut phone);
            }
        }
    }
    // Wake a blocked completion producer before joining the executor.
    completions.close();
}

fn run_ble(
    core: &Core,
    receiver: &mpsc::Receiver<BleMessage>,
    completed: &async_channel::Sender<Completion>,
    shutdown: &AtomicBool,
    resume: &AtomicBool,
    stopped: &AtomicU64,
) {
    let sink = Notifications {
        relay: Some(completed.clone()),
        ..Notifications::default()
    };
    let mut phone = PhoneState {
        active: false,
        status: "Phone key inactive".into(),
        link: PhoneKeyLink::Stopped,
        generation: 0,
        away: crate::phone_key::AwayDeadline::default(),
    };
    if !shutdown.load(Ordering::SeqCst) {
        phone.start(core, &sink);
    }
    loop {
        if shutdown.load(Ordering::SeqCst) {
            break;
        }
        if resume.swap(false, Ordering::SeqCst) {
            core.handle_resume();
            phone.start(core, &sink);
        }
        if let Some(result) = core.maintain_phone_key(stopped.swap(0, Ordering::SeqCst)) {
            phone.generation = core.session_generation();
            sink.send(&Notification::PhoneKeyEvent {
                kind: if result.is_ok() {
                    PresenceEvent::Restarted
                } else {
                    PresenceEvent::Stopped
                },
                generation: phone.generation,
                vin: core.vin(),
                time: crate::keylog::utc_stamp(),
                error: result
                    .as_ref()
                    .err()
                    .map(ToString::to_string)
                    .unwrap_or_default(),
            });
            phone.started(result, &sink);
        }
        // Sleep until a request, an explicit wake (resume, presence stopped,
        // shutdown) or the scheduled presence retry.
        let wait = core
            .phone_key_retry_deadline()
            .map_or(BLE_IDLE_WAIT, |deadline| {
                deadline.saturating_duration_since(Instant::now())
            })
            .min(BLE_IDLE_WAIT);
        match receiver.recv_timeout(wait) {
            Ok(BleMessage::Request(request)) => {
                if shutdown.load(Ordering::SeqCst) {
                    break;
                }
                let refresh = matches!(
                    &request,
                    Request::Run {
                        refresh_status: true,
                        ..
                    }
                );
                dispatch(core, request, &sink, &mut phone);
                if refresh {
                    let _ = completed.send_blocking(Completion::RefreshAt(
                        Instant::now() + Duration::from_millis(2500),
                    ));
                }
            }
            Ok(BleMessage::Wake) | Err(mpsc::RecvTimeoutError::Timeout) => (),
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
    core.stop_phone_key();
    core.reap_session();
}

#[allow(clippy::too_many_lines, clippy::needless_pass_by_value)]
fn dispatch(core: &Core, request: Request, sink: &Notifications, phone: &mut PhoneState) {
    match request {
        Request::Run {
            request_id,
            cmd,
            args,
            ..
        } => match core.run(&cmd, &args) {
            Ok(outcome) => {
                let renamed = outcome.ok
                    && matches!(
                        cmd.parse(),
                        Ok(crate::commands::Command::VehicleName
                            | crate::commands::Command::SetVehicleName)
                    );
                sink.send(&Notification::CommandFinished {
                    request_id,
                    ok: outcome.ok,
                    stdout: outcome.stdout,
                    stderr: outcome.stderr,
                    exit_code: outcome.exit_code,
                });
                // The cached car name changed: push it to every view.
                if renamed {
                    dispatch(core, Request::GetConfig, sink, phone);
                }
            }
            Err(e) => sink.send(&Notification::CommandError {
                request_id,
                error: e.to_string(),
            }),
        },
        Request::GenerateKey { force } => {
            let (ok, pem, error) = match core.generate_key(force) {
                Ok(pem) => (true, pem, String::new()),
                Err(e) => (false, String::new(), e.to_string()),
            };
            sink.send(&Notification::KeyGenerated { ok, pem, error });
            phone.start(core, sink);
        }
        Request::Pair => {
            let (ok, output, error) = core
                .pair()
                .unwrap_or_else(|e| (false, String::new(), e.to_string()));
            sink.send(&Notification::Paired { ok, output, error });
            phone.start(core, sink);
        }
        Request::SetConfig {
            vin,
            model,
            key_name,
            connect_timeout_sec,
            command_timeout_sec,
            cover_left_action,
            cover_right_action,
        } => {
            let result = core.set_config_with_cover_actions(
                &vin,
                &model,
                &key_name,
                connect_timeout_sec,
                command_timeout_sec,
                Some((cover_left_action, cover_right_action)),
            );
            let ok = result.is_ok();
            let error = result.err().map_or_else(String::new, |e| e.to_string());
            if ok {
                dispatch(core, Request::GetConfig, sink, phone);
            }
            sink.send(&Notification::ConfigSaved { ok, error });
            if ok {
                phone.start(core, sink);
            }
        }
        Request::SetNotification { enabled, actions } => {
            if let Err(error) = core.set_notification(enabled, &actions) {
                crate::keylog::log("core", &format!("notification settings not saved: {error}"));
            }
            // Success or not, show what is actually saved.
            dispatch(core, Request::GetConfig, sink, phone);
        }
        Request::GetConfig => {
            let (vin, model, key_name, connect_timeout_sec, command_timeout_sec, has_key, pem) =
                core.get_config();
            let (cover_left_action, cover_right_action) = core.cover_actions();
            let (notification_enabled, notification_actions) = core.notification_settings();
            sink.send(&Notification::ConfigLoaded {
                vin,
                model,
                key_name,
                connect_timeout_sec,
                command_timeout_sec,
                has_key,
                pem,
                cover_left_action,
                cover_right_action,
                vehicle_name: core.vehicle_name(),
                notification_enabled,
                notification_actions,
            });
        }
        Request::PreviewDestination { request_id, text } => {
            let (ok, kind, value1, value2, error) = match Core::preview_destination(&text) {
                Ok((kind, value1, value2)) => (true, Some(kind), value1, value2, String::new()),
                Err(e) => (false, None, String::new(), String::new(), e.to_string()),
            };
            sink.send(&Notification::DestinationPreviewed {
                request_id,
                ok,
                kind,
                value1,
                value2,
                error,
            });
        }
        Request::ShareDestination { request_id, text } => {
            let (ok, output, error) = core
                .share_destination(&text)
                .unwrap_or_else(|e| (false, String::new(), e.to_string()));
            sink.send(&Notification::ShareFinished {
                request_id,
                ok,
                output,
                error,
            });
        }
        _ => crate::keylog::log("runtime", "unexpected control request in dispatch"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tesla_ble::fake;
    use serde_json::{json, Value};
    use std::ffi::CStr;

    unsafe extern "C" fn notification(context: *mut c_void, data: *const c_char) {
        // SAFETY: the test retains its boxed sender until after detachment.
        let sender = unsafe { &*context.cast::<mpsc::Sender<Value>>() };
        let json = unsafe { CStr::from_ptr(data) }.to_str().unwrap();
        sender.send(serde_json::from_str(json).unwrap()).unwrap();
    }

    fn receive(receiver: &mpsc::Receiver<Value>, kind: &str, timeout: Duration) -> Value {
        let deadline = Instant::now() + timeout;
        loop {
            let event = receiver
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                .unwrap();
            if event["type"] == kind {
                return event;
            }
        }
    }

    /// A core on the production protocol client over the fake Go library.
    fn fake_core(dir: &std::path::Path, vin: &str, state: crate::config::VinState) -> Core {
        crate::config::Config {
            vin: vin.into(),
            vin_state: state,
            ..crate::config::Config::default()
        }
        .save(&dir.join("config.json"))
        .unwrap();
        crate::keys::generate(
            &dir.join("private_key.pem"),
            &dir.join("public_key.pem"),
            false,
        )
        .unwrap();
        Core::new(
            dir,
            Some(crate::protocol_client::ProtocolClient::with_library(
                fake::library(),
            )),
        )
        .unwrap()
    }

    fn wait_until(condition: impl Fn() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(3);
        while !condition() {
            assert!(Instant::now() < deadline, "condition not reached");
            thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn set_notification_accepts_the_qt_client_payload_and_persists() {
        // Exactly what TeslaClient::setNotificationSettings submits.
        let request: Request = serde_json::from_str(
            r#"{"op":"set_notification","enabled":true,"actions":["honk","lock","honk"]}"#,
        )
        .unwrap();
        let Request::SetNotification { enabled, actions } = request else {
            panic!("set_notification")
        };
        let dir = tempfile::tempdir().unwrap();
        let core = Core::new(dir.path(), None).unwrap();
        core.set_notification(enabled, &actions).unwrap();
        let reloaded = Core::new(dir.path(), None).unwrap();
        assert_eq!(
            reloaded.notification_settings(),
            (
                true,
                vec![
                    crate::cover_action::CoverAction::Honk,
                    crate::cover_action::CoverAction::Lock
                ]
            )
        );
    }

    #[test]
    fn application_state_names_match_qt_client_and_tolerate_unknown_ones() {
        for (name, state) in [
            ("active", AppState::Active),
            ("inactive", AppState::Inactive),
            ("hidden", AppState::Hidden),
            ("suspended", AppState::Suspended),
            ("unknown", AppState::Unknown),
            ("something-new", AppState::Unknown),
        ] {
            let request = format!(r#"{{"op":"application_state","state":"{name}"}}"#);
            let Ok(Request::ApplicationState { state: parsed }) = serde_json::from_str(&request)
            else {
                panic!("{name}")
            };
            assert_eq!(parsed, state, "{name}");
        }
    }

    #[test]
    fn blocked_ble_keeps_control_and_presence_responsive_and_shutdown_skips_queue() {
        let dir = tempfile::tempdir().unwrap();
        let vin = "5YJ3E1EA0PF200001";
        // Vehicle commands never complete on their own: only shutdown ends them.
        fake::script(
            vin,
            fake::Script {
                hold: vec![fake::VEHICLE_COMMAND],
                ..fake::Script::default()
            },
        );
        let runtime = Runtime::launch(
            fake_core(dir.path(), vin, crate::config::VinState::Paired),
            false,
        );
        let (sender, receiver) = mpsc::channel::<Value>();
        let mut sender = Box::new(sender);
        unsafe { runtime.observe(Some(notification), std::ptr::addr_of_mut!(*sender).cast()) };
        receive(&receiver, "initialized", Duration::from_secs(2));
        // The car advertises nearby: presence connects.
        let near = loop {
            fake::observe_target(vin, fake::ObservationKind::Advertisement, 0, 7, -60);
            if let Ok(event) = receiver.recv_timeout(Duration::from_millis(50)) {
                if event["type"] == "phone_key_event" && event["kind"] == "presence_near" {
                    break event;
                }
            }
        };
        assert_eq!(near["vin"], vin);
        assert!(runtime.submit(r#"{"op":"run","request_id":"slow","cmd":"lock","args":[]}"#));
        wait_until(|| fake::held(vin) == 1);

        let start = Instant::now();
        assert!(runtime.submit(r#"{"op":"get_config"}"#));
        let config = receive(&receiver, "config_loaded", Duration::from_millis(100));
        assert_eq!(config["vin"], vin);
        assert!(start.elapsed() < Duration::from_millis(100));
        // Presence observations are still processed while BLE work is blocked.
        fake::observe(vin, fake::ObservationKind::Authenticated, 0);
        let event = receive(&receiver, "phone_key_event", Duration::from_secs(1));
        assert_eq!(event["kind"], "presence_auth_ok");
        assert!(runtime.submit(r#"{"op":"snapshot"}"#));
        receive(&receiver, "initialized", Duration::from_millis(100));
        let snapshot = receive(&receiver, "phone_key_state", Duration::from_millis(100));
        assert_eq!(snapshot["link"], "authorized");
        assert!(runtime
            .submit(&json!({"op":"export_automagic", "documents_dir":dir.path()}).to_string()));
        let exported = receive(
            &receiver,
            "automagic_export_finished",
            Duration::from_secs(2),
        );
        assert_eq!(exported["ok"], true);
        assert!(std::path::Path::new(exported["path"].as_str().unwrap())
            .join("flows.json")
            .exists());
        for _ in 0..64 {
            assert!(runtime.submit(r#"{"op":"run","request_id":"queued","cmd":"lock","args":[]}"#));
        }
        assert!(!runtime.submit(r#"{"op":"run","request_id":"full","cmd":"lock","args":[]}"#));
        assert!(
            runtime.submit(r#"{"op":"preview_destination","request_id":"nav","text":"geo:45,9"}"#)
        );
        receive(
            &receiver,
            "destination_previewed",
            Duration::from_millis(100),
        );
        let start = Instant::now();
        assert!(runtime.submit(r#"{"op":"shutdown"}"#));
        assert!(!runtime.submit(r#"{"op":"get_config"}"#));
        drop(runtime);
        assert!(start.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn phone_key_machine_state_tracks_events_independently_of_diagnostics() {
        let dir = tempfile::tempdir().unwrap();
        let vin = "5YJ3E1EA0PF200002";
        let core = fake_core(dir.path(), vin, crate::config::VinState::Unpaired);
        let client = core.protocol();
        let sink = Notifications::default();
        let (sender, receiver) = mpsc::channel();
        let mut sender = Box::new(sender);
        sink.observe(Some(Observer {
            callback: notification,
            context: std::ptr::addr_of_mut!(*sender) as usize,
        }));
        let mut phone = PhoneState {
            active: false,
            status: String::new(),
            link: PhoneKeyLink::Stopped,
            generation: 0,
            away: crate::phone_key::AwayDeadline::default(),
        };
        let stopped = AtomicU64::new(0);
        for (kind, bluetooth_off, link, active) in [
            (PresenceEvent::Near, false, "connected", true),
            (PresenceEvent::AuthOk, false, "authorized", true),
            (PresenceEvent::Inside, false, "authorized", true),
            (PresenceEvent::Error, true, "bluetooth-off", true),
            (PresenceEvent::AuthFailed, false, "error", true),
            (PresenceEvent::Disconnected, false, "scanning", true),
            (PresenceEvent::Stopped, false, "stopped", false),
        ] {
            client.emit_for_test(
                vin,
                kind,
                "diagnostic wording unrelated to state",
                bluetooth_off,
            );
            let stop_seen = phone.poll(&core, &sink, &stopped);
            assert_eq!(stop_seen, kind == PresenceEvent::Stopped);
            let state = receive(&receiver, "phone_key_state", Duration::from_secs(1));
            assert_eq!(state["link"], link, "{kind}");
            assert_eq!(state["active"], active, "{kind}");
        }
        assert_eq!(stopped.load(Ordering::SeqCst), core.session_generation());
        sink.observe(None);
    }

    #[test]
    fn delayed_away_publication_preserves_raw_events_and_uses_standard_event_shape() {
        let dir = tempfile::tempdir().unwrap();
        let core = Core::new(
            dir.path(),
            Some(crate::protocol_client::ProtocolClient::with_library(
                fake::library(),
            )),
        )
        .unwrap();
        core.protocol()
            .emit_for_test("", PresenceEvent::Far, "", false);
        let (sender, receiver) = mpsc::channel();
        let sink = Notifications::default();
        let mut sender = Box::new(sender);
        sink.observe(Some(Observer {
            callback: notification,
            context: std::ptr::addr_of_mut!(*sender) as usize,
        }));
        let mut phone = PhoneState {
            active: true,
            status: String::new(),
            link: PhoneKeyLink::Scanning,
            generation: 0,
            away: crate::phone_key::AwayDeadline::default(),
        };
        phone.poll(&core, &sink, &AtomicU64::new(0));
        assert_eq!(
            receive(&receiver, "phone_key_event", Duration::from_secs(1))["kind"],
            "presence_far"
        );
        receive(&receiver, "phone_key_state", Duration::from_secs(1));
        phone.publish_due_away(&core, &sink, phone.away.deadline().unwrap());
        let event = receive(&receiver, "phone_key_event", Duration::from_secs(1));
        assert_eq!(event["kind"], "presence_away_confirmed");
        assert_eq!(event["vin"], "");
        assert_eq!(event["error"], "");
        assert!(event["time"].as_str().unwrap().ends_with('Z'));
        phone.publish_due_away(&core, &sink, Instant::now() + Duration::from_secs(360));
        assert!(receiver.try_recv().is_err());
        sink.observe(None);
    }

    #[test]
    fn blocked_key_generation_does_not_hold_the_configuration_lock() {
        let dir = tempfile::tempdir().unwrap();
        let vin = "5YJ3E1EA0PF200003";
        // Replacing the key stops presence first; that radio work never ends.
        fake::script(
            vin,
            fake::Script {
                hold: vec![fake::WATCH_STOP],
                ..fake::Script::default()
            },
        );
        let runtime = Runtime::launch(
            fake_core(dir.path(), vin, crate::config::VinState::Paired),
            false,
        );
        let (sender, receiver) = mpsc::channel::<Value>();
        let mut sender = Box::new(sender);
        unsafe { runtime.observe(Some(notification), std::ptr::addr_of_mut!(*sender).cast()) };
        receive(&receiver, "initialized", Duration::from_secs(2));
        wait_until(|| runtime.core.phone_key_enabled());
        assert!(runtime.submit(r#"{"op":"generate_key","force":true}"#));
        wait_until(|| fake::held(vin) == 1);
        assert!(runtime.submit(r#"{"op":"get_config"}"#));
        assert_eq!(
            receive(&receiver, "config_loaded", Duration::from_millis(100))["has_key"],
            true
        );
        drop(runtime);
    }

    #[test]
    fn detachment_waits_for_callbacks_already_in_flight() {
        struct Context {
            entered: mpsc::Sender<()>,
            release: Mutex<mpsc::Receiver<()>>,
        }
        unsafe extern "C" fn blocked(context: *mut c_void, _: *const c_char) {
            let context = unsafe { &*context.cast::<Context>() };
            context.entered.send(()).unwrap();
            context.release.lock().unwrap().recv().unwrap();
        }
        let sink = Arc::new(Notifications::default());
        let (entered, entry) = mpsc::channel();
        let (release, wait) = mpsc::channel();
        let mut context = Box::new(Context {
            entered,
            release: Mutex::new(wait),
        });
        sink.observe(Some(Observer {
            callback: blocked,
            context: std::ptr::addr_of_mut!(*context) as usize,
        }));
        let delivery = thread::spawn({
            let sink = Arc::clone(&sink);
            move || sink.send(&Notification::Initialized { ok: true })
        });
        entry.recv_timeout(Duration::from_secs(1)).unwrap();
        let (done, detached) = mpsc::channel();
        let detach = thread::spawn({
            let sink = Arc::clone(&sink);
            move || {
                sink.observe(None);
                done.send(()).unwrap();
            }
        });
        assert!(matches!(
            detached.recv_timeout(Duration::from_millis(20)),
            Err(mpsc::RecvTimeoutError::Timeout)
        ));
        release.send(()).unwrap();
        detached.recv_timeout(Duration::from_secs(1)).unwrap();
        delivery.join().unwrap();
        detach.join().unwrap();
        drop(context);
        sink.send(&Notification::Initialized { ok: true });
    }

    #[test]
    fn cover_preferences_default_save_publish_and_survive_restart() {
        let dir = tempfile::tempdir().unwrap();
        let state = dir.path().to_string_lossy().into_owned();
        for first_start in [true, false] {
            let runtime = Runtime::new(Core::new(state.clone(), None).unwrap());
            let (sender, receiver) = mpsc::channel::<Value>();
            let mut sender = Box::new(sender);
            unsafe { runtime.observe(Some(notification), std::ptr::addr_of_mut!(*sender).cast()) };
            assert!(runtime.submit(r#"{"op":"get_config"}"#));
            let loaded = receive(&receiver, "config_loaded", Duration::from_secs(2));
            assert_eq!(
                loaded["cover_left_action"],
                if first_start {
                    "frunk-open"
                } else {
                    "flash-lights"
                }
            );
            assert_eq!(
                loaded["cover_right_action"],
                if first_start { "trunk-open" } else { "honk" }
            );
            if first_start {
                assert!(runtime.submit(r#"{"op":"set_config","vin":"","model":"","key_name":"phone","connect_timeout_sec":20,"command_timeout_sec":5,"cover_left_action":"flash-lights","cover_right_action":"honk"}"#));
                // Saving pushes the new config to the cover without a UI poll.
                let updated = receive(&receiver, "config_loaded", Duration::from_secs(2));
                assert_eq!(updated["cover_left_action"], "flash-lights");
                assert_eq!(updated["cover_right_action"], "honk");
                assert_eq!(
                    receive(&receiver, "config_saved", Duration::from_secs(2))["ok"],
                    true
                );
            }
            drop(runtime);
        }
    }

    #[test]
    fn commands_and_delayed_refresh_are_rust_driven_and_callbacks_detach() {
        let dir = tempfile::tempdir().unwrap();
        let state = dir.path().to_string_lossy().into_owned();
        let runtime = Runtime::new(Core::new(state, None).unwrap());
        let (sender, receiver) = mpsc::channel::<Value>();
        let mut sender = Box::new(sender);
        unsafe { runtime.observe(Some(notification), std::ptr::addr_of_mut!(*sender).cast()) };
        assert!(
            receive(&receiver, "initialized", Duration::from_secs(2))["ok"]
                .as_bool()
                .unwrap()
        );
        assert!(
            !receive(&receiver, "phone_key_state", Duration::from_secs(2))["active"]
                .as_bool()
                .unwrap()
        );
        assert!(runtime.submit(r#"{"op":"get_config"}"#));
        assert!(
            !receive(&receiver, "config_loaded", Duration::from_secs(2))["has_key"]
                .as_bool()
                .unwrap()
        );
        assert!(
            runtime.submit(r#"{"op":"run","request_id":"command-7","cmd":"unknown","args":[]}"#)
        );
        assert_eq!(
            receive(&receiver, "command_error", Duration::from_secs(2))["request_id"],
            "command-7"
        );
        assert!(
            runtime.submit(r#"{"op":"preview_destination","request_id":"nav","text":"geo:45,9"}"#)
        );
        let preview = receive(&receiver, "destination_previewed", Duration::from_secs(2));
        assert_eq!(preview["request_id"], "nav");
        assert_eq!(preview["kind"], "gps");
        assert!(preview["ok"].as_bool().unwrap());
        assert!(!runtime.submit(r#"{"op":"unknown_operation"}"#));
        assert!(!runtime.submit(r#"{"op":"run","request_id":"bad","cmd":"ping","args":[7]}"#));
        assert!(runtime.submit(
            r#"{"op":"run","request_id":"toggle","cmd":"unknown","args":[],"refresh_status":true}"#
        ));
        // No event loop or UI poll: Rust expires the deadline while we sleep.
        thread::sleep(Duration::from_millis(2700));
        receive(
            &receiver,
            "status_refresh_requested",
            Duration::from_secs(1),
        );
        unsafe { runtime.observe(None, std::ptr::null_mut()) };
        while receiver.try_recv().is_ok() {}
        drop(sender);
        assert!(runtime.submit(r#"{"op":"get_config"}"#));
        drop(runtime); // Joins the worker; no stale callback may touch sender.
        assert!(receiver.try_recv().is_err());
    }

    #[test]
    fn preview_and_share_failures_publish_empty_results() {
        let dir = tempfile::tempdir().unwrap();
        let runtime = Runtime::launch(Core::new(dir.path(), None).unwrap(), false);
        let (sender, receiver) = mpsc::channel::<Value>();
        let mut sender = Box::new(sender);
        unsafe { runtime.observe(Some(notification), std::ptr::addr_of_mut!(*sender).cast()) };
        assert!(
            runtime.submit(r#"{"op":"preview_destination","request_id":"ok","text":"geo:45,9"}"#)
        );
        assert_eq!(
            receive(&receiver, "destination_previewed", Duration::from_secs(2))["kind"],
            "gps"
        );
        assert!(
            runtime.submit(r#"{"op":"preview_destination","request_id":"bad","text":"999,999"}"#)
        );
        let failed = receive(&receiver, "destination_previewed", Duration::from_secs(2));
        assert_eq!(failed["request_id"], "bad");
        assert_eq!(failed["ok"], false);
        for field in ["kind", "value1", "value2"] {
            assert_eq!(
                failed[field], "",
                "{field} must not carry an earlier preview"
            );
        }
        assert_ne!(failed["error"], "");
        assert!(
            runtime.submit(r#"{"op":"share_destination","request_id":"share","text":"geo:45,9"}"#)
        );
        let shared = receive(&receiver, "share_finished", Duration::from_secs(2));
        assert_eq!(shared["ok"], false);
        assert_eq!(shared["output"], "");
        assert_ne!(shared["error"], "");
        unsafe { runtime.observe(None, std::ptr::null_mut()) };
        drop(runtime);
    }

    #[test]
    fn failed_start_is_retried_without_a_ui_poll() {
        let dir = tempfile::tempdir().unwrap();
        let vin = "5YJ3E1EA0PF200004";
        fake::script(
            vin,
            fake::Script {
                statuses: [(fake::WATCH_START, (fake::FAILED, "radio failure".into()))].into(),
                ..fake::Script::default()
            },
        );
        let runtime = Runtime::launch(
            fake_core(dir.path(), vin, crate::config::VinState::Paired),
            false,
        );
        let (sender, receiver) = mpsc::channel::<Value>();
        let mut sender = Box::new(sender);
        unsafe { runtime.observe(Some(notification), std::ptr::addr_of_mut!(*sender).cast()) };
        receive(&receiver, "initialized", Duration::from_secs(2));
        // The real five-second retry deadline must fire autonomously.
        let event = receive(&receiver, "phone_key_event", Duration::from_secs(8));
        assert_eq!(event["kind"], "presence_stopped");
        assert_eq!(event["vin"], vin);
        assert_ne!(event["error"].as_str().unwrap(), "");
        assert!(event["time"].as_str().unwrap().ends_with('Z'));
        unsafe { runtime.observe(None, std::ptr::null_mut()) };
        drop(runtime);
    }

    // REVIEW ROUND 3 BUG (protocol_client.rs `outcome`/`start_presence`,
    // error.rs `OperationError::PresenceFailed(String)`, runtime.rs
    // `PhoneState::started`): when presence cannot start because Bluetooth is
    // off, Go reports EE_BLUETOOTH_OFF, but `RunOutcome` keeps only the error
    // text, `Core` wraps it as a generic `PresenceFailed`, and the runtime
    // publishes link `error`. The dedicated `bluetooth-off` UI state is thus
    // unreachable at startup, and when Bluetooth is switched off while
    // presence runs it is shown only until the immediate restart attempt
    // (within ~100 ms) overwrites it with `error`, re-published every 5 s.
    // Desired: a start failure classified as Bluetooth-off publishes
    // `link: "bluetooth-off"`.
    #[test]
    fn review_round3_bluetooth_off_start_failure_publishes_bluetooth_off_link() {
        let dir = tempfile::tempdir().unwrap();
        let vin = "5YJ3E1EA0PF300005";
        fake::script(
            vin,
            fake::Script {
                statuses: [(
                    fake::WATCH_START,
                    (
                        fake::BLUETOOTH_OFF,
                        "Bluetooth adapter is powered off".into(),
                    ),
                )]
                .into(),
                ..fake::Script::default()
            },
        );
        let runtime = Runtime::launch(
            fake_core(dir.path(), vin, crate::config::VinState::Paired),
            false,
        );
        let (sender, receiver) = mpsc::channel::<Value>();
        let mut sender = Box::new(sender);
        unsafe { runtime.observe(Some(notification), std::ptr::addr_of_mut!(*sender).cast()) };
        let state = loop {
            let state = receive(&receiver, "phone_key_state", Duration::from_secs(3));
            if state["link"] != "stopped" {
                break state;
            }
        };
        unsafe { runtime.observe(None, std::ptr::null_mut()) };
        drop(runtime);
        assert_eq!(state["active"], false);
        assert_eq!(
            state["link"], "bluetooth-off",
            "Bluetooth-off start failure published as {state}"
        );
    }
}
