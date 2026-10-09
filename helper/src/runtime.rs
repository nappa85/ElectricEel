//! Autonomous application runtime: commands, presence/retries, UI notifications
//! and delayed status refreshes all run on Rust threads, without Qt timers.
use std::ffi::{c_void, CString};
use std::os::raw::c_char;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::core::Core;
use crate::cpukeepalive::CpuKeepAlive;

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
    Notification(Value),
    RefreshAt(Instant),
}

impl Notifications {
    fn send(&self, value: &Value) {
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
            let Ok(data) = CString::new(value.to_string()) else {
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
    },
    GetConfig,
    PreviewDestination {
        request_id: String,
        text: String,
    },
    ShareDestination {
        request_id: String,
        text: String,
    },
    ApplicationState {
        state: String,
    },
    LogUi {
        message: String,
    },
    Snapshot,
    Shutdown,
}

pub struct Runtime {
    requests: async_channel::Sender<Request>,
    ble_requests: mpsc::SyncSender<Request>,
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
            thread::spawn(move || {
                async_io::block_on(run(
                    &core,
                    receiver,
                    completions,
                    &sink,
                    &shutdown,
                    &resume,
                    &stopped,
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
                self.shutdown.store(true, Ordering::SeqCst);
                self.core.cancel_session();
                self.requests.close();
                true
            }
            Request::Run { .. }
            | Request::Pair
            | Request::GenerateKey { .. }
            | Request::SetConfig { .. }
            | Request::ShareDestination { .. } => self.ble_requests.try_send(request).is_ok(),
            _ => self.requests.try_send(request).is_ok(),
        })
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
                    .send(&json!({"type":"initialized", "ok":true}));
            }
        }
    }
}

impl Drop for Runtime {
    fn drop(&mut self) {
        // Detach before joining so shutdown cannot call into a destroyed UI.
        self.notifications.observe(None);
        self.shutdown.store(true, Ordering::SeqCst);
        self.core.cancel_session();
        self.requests.close();
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

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
enum PhoneKeyLink {
    Unpaired,
    BluetoothOff,
    Scanning,
    Connected,
    Authorized,
    Stopped,
    Error,
}

struct PhoneState {
    active: bool,
    status: String,
    link: PhoneKeyLink,
    generation: u64,
}

impl PhoneState {
    fn publish(&self, sink: &Notifications) {
        sink.send(&json!({"type":"phone_key_state", "active":self.active, "link":self.link, "status":self.status, "generation":self.generation}));
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
                self.link = if matches!(error, crate::error::OperationError::NotPaired) {
                    PhoneKeyLink::Unpaired
                } else {
                    PhoneKeyLink::Error
                };
            }
        }
        self.publish(sink);
    }

    fn poll(&mut self, core: &Core, sink: &Notifications, stopped: &AtomicU64) {
        while let Some(mut event) = core.drain_phone_key_event() {
            if event.generation < core.session_generation() {
                continue;
            }
            self.generation = event.generation;
            if event.vin.is_empty() {
                event.vin = core.get_config().0;
            }
            if event.kind == "presence_stopped" {
                stopped.fetch_max(event.generation, Ordering::SeqCst);
            }
            match event.kind.as_str() {
                "presence_stopped" => self.active = false,
                "presence_restarted" | "presence_near" => self.active = true,
                _ => (),
            }
            self.link = match event.kind.as_str() {
                "presence_near" => PhoneKeyLink::Connected,
                "presence_auth_ok" => PhoneKeyLink::Authorized,
                "presence_far" | "presence_restarted" | "presence_disconnected" => {
                    PhoneKeyLink::Scanning
                }
                "presence_stopped" => PhoneKeyLink::Stopped,
                "presence_error" | "presence_auth_failed" => PhoneKeyLink::Error,
                _ => self.link,
            };
            if event.error_code == "bluetooth-off" {
                self.link = PhoneKeyLink::BluetoothOff;
            }
            let status = match event.kind.as_str() {
                "presence_near" => Some("Phone key connected".to_string()),
                "presence_far" | "presence_restarted" | "presence_disconnected" => {
                    Some("Phone key scanning".to_string())
                }
                "presence_auth_ok" => Some("Phone key authorized".to_string()),
                "presence_stopped" => Some(with_error("Phone key stopped", &event.error)),
                "presence_error" | "presence_auth_failed" => {
                    Some(with_error("Phone key error", &event.error))
                }
                _ => None,
            };
            if let Some(status) = status {
                self.status = status;
            }
            let time = if event.time.is_empty() {
                crate::keylog::utc_stamp()
            } else {
                event.time
            };
            sink.send(
                &json!({"type":"phone_key_event", "kind":event.kind, "vin":event.vin,
                "time":time, "error":event.error}),
            );
            self.publish(sink);
        }
    }
}

fn with_error(status: &str, error: &str) -> String {
    if error.is_empty() {
        status.into()
    } else {
        format!("{status}: {error}")
    }
}

#[allow(clippy::too_many_arguments)]
async fn run(
    core: &Core,
    receiver: async_channel::Receiver<Request>,
    completions: async_channel::Receiver<Completion>,
    sink: &Notifications,
    shutdown: &AtomicBool,
    resume: &AtomicBool,
    stopped: &AtomicU64,
) {
    enum Wake {
        Request(Result<Request, async_channel::RecvError>),
        Completion(Result<Completion, async_channel::RecvError>),
        Timer,
        Presence,
    }
    let mut phone = PhoneState {
        active: false,
        status: "Phone key inactive".into(),
        link: PhoneKeyLink::Stopped,
        generation: 0,
    };
    let mut poll_at = Instant::now();
    let mut refresh_at = None;
    let mut suspended = false;
    loop {
        if shutdown.load(Ordering::SeqCst) {
            break;
        }
        let now = Instant::now();
        if now >= poll_at {
            phone.poll(core, sink, stopped);
            poll_at = Instant::now() + Duration::from_secs(1);
        }
        if refresh_at.is_some_and(|deadline| now >= deadline) {
            refresh_at = None;
            sink.send(&json!({"type":"status_refresh_requested"}));
        }
        let deadline = refresh_at.map_or(poll_at, |refresh| poll_at.min(refresh));
        let wake = futures_lite::future::race(
            async { Wake::Request(receiver.recv().await) },
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
        .await;
        if shutdown.load(Ordering::SeqCst) {
            break;
        }
        match wake {
            Wake::Request(Ok(Request::Shutdown) | Err(_)) | Wake::Completion(Err(_)) => break,
            Wake::Timer => (),
            Wake::Presence => phone.poll(core, sink, stopped),
            Wake::Completion(Ok(Completion::RefreshAt(deadline))) => refresh_at = Some(deadline),
            Wake::Completion(Ok(Completion::Notification(event))) => {
                if event["type"] == "phone_key_state" {
                    let generation = event["generation"].as_u64().unwrap_or(0);
                    let scanning = event["link"] == "scanning";
                    if generation < core.session_generation()
                        || generation < phone.generation
                        || (generation != 0 && generation == phone.generation && scanning)
                    {
                        continue;
                    }
                    phone.generation = generation;
                    phone.active = event["active"].as_bool().unwrap_or(false);
                    phone.status = event["status"].as_str().unwrap_or_default().into();
                    phone.link = serde_json::from_value(event["link"].clone())
                        .unwrap_or(PhoneKeyLink::Error);
                }
                sink.send(&event);
            }
            Wake::Request(Ok(Request::ApplicationState { state })) => {
                crate::keylog::log("ui", &format!("applicationState={state}"));
                if state == "suspended" {
                    suspended = true;
                } else if state == "active" && suspended {
                    suspended = false;
                    resume.store(true, Ordering::SeqCst);
                }
            }
            Wake::Request(Ok(Request::LogUi { message })) => crate::keylog::log("ui", &message),
            Wake::Request(Ok(Request::Snapshot)) => {
                sink.send(&json!({"type":"initialized", "ok":true}));
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
    receiver: &mpsc::Receiver<Request>,
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
            sink.send(&json!({"type":"phone_key_event", "kind":if result.is_ok() { "presence_restarted" } else { "presence_stopped" },
                "vin":core.get_config().0, "time":crate::keylog::utc_stamp(),
                "error":result.as_ref().err().map(ToString::to_string).unwrap_or_default()}));
            phone.started(result, &sink);
        }
        match receiver.recv_timeout(Duration::from_millis(100)) {
            Ok(request) => {
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
            Err(mpsc::RecvTimeoutError::Timeout) => (),
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
    core.stop_phone_key();
    core.reap_session();
}

#[allow(clippy::too_many_lines, clippy::needless_pass_by_value)]
fn dispatch(core: &Core, request: Request, sink: &Notifications, phone: &mut PhoneState) {
    match request {
        Request::Run { request_id, cmd, args, .. } => match core.run(&cmd, &args) {
            Ok((ok, stdout, stderr, exit_code)) => sink.send(&json!({"type":"command_finished",
                "request_id":request_id, "ok":ok, "stdout":stdout, "stderr":stderr, "exit_code":exit_code})),
            Err(e) => sink.send(&json!({"type":"command_error", "request_id":request_id, "error":e.to_string()})),
        },
        Request::GenerateKey { force } => {
            let (ok, pem, error) = match core.generate_key(force) {
                Ok(pem) => (true, pem, String::new()),
                Err(e) => (false, String::new(), e.to_string()),
            };
            sink.send(&json!({"type":"key_generated", "ok":ok, "pem":pem, "error":error}));
            phone.start(core, sink);
        }
        Request::Pair => {
            let (ok, output, error) = core.pair().unwrap_or_else(|e| (false, String::new(), e.to_string()));
            sink.send(&json!({"type":"paired", "ok":ok, "output":output, "error":error}));
            phone.start(core, sink);
        }
        Request::SetConfig { vin, model, key_name, connect_timeout_sec, command_timeout_sec } => {
            let result = core.set_config(&vin, &model, &key_name, connect_timeout_sec, command_timeout_sec);
            let ok = result.is_ok();
            let error = result.err().map_or_else(String::new, |e| e.to_string());
            sink.send(&json!({"type":"config_saved", "ok":ok, "error":error}));
            if ok { phone.start(core, sink); }
        }
        Request::GetConfig => {
            let (vin, model, key_name, connect_timeout_sec, command_timeout_sec, has_key, pem) = core.get_config();
            sink.send(&json!({"type":"config_loaded", "vin":vin, "model":model, "key_name":key_name,
                "connect_timeout_sec":connect_timeout_sec, "command_timeout_sec":command_timeout_sec,
                "has_key":has_key, "pem":pem}));
        }
        Request::PreviewDestination { request_id, text } => {
            let (ok, kind, value1, value2, error) = match Core::preview_destination(&text) {
                Ok((kind, value1, value2)) => (true, kind, value1, value2, String::new()),
                Err(e) => (false, String::new(), String::new(), String::new(), e.to_string()),
            };
            sink.send(&json!({"type":"destination_previewed", "request_id":request_id, "ok":ok,
                "kind":kind, "value1":value1, "value2":value2, "error":error}));
        }
        Request::ShareDestination { request_id, text } => {
            let (ok, output, error) = core.share_destination(&text).unwrap_or_else(|e| (false, String::new(), e.to_string()));
            sink.send(&json!({"type":"share_finished", "request_id":request_id, "ok":ok, "output":output, "error":error}));
        }
        _ => unreachable!("runtime control requests are handled by run"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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

    #[test]
    fn blocked_ble_keeps_control_and_presence_responsive_and_shutdown_skips_queue() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = crate::config::Config {
            vin: "5YJ3E1EA0PF000000".into(),
            ..crate::config::Config::default()
        };
        cfg.save(&dir.path().join("config.json")).unwrap();
        std::fs::write(dir.path().join("private_key.pem"), "private").unwrap();
        std::fs::write(dir.path().join("public_key.pem"), "public").unwrap();
        let (session, got, peer) = crate::session_client::tests::accept_blocked_mock(dir.path());
        let state = dir.path().to_string_lossy().into_owned();
        let runtime = Runtime::launch(
            Core::new(state.clone(), state, Some(session)).unwrap(),
            false,
        );
        let (sender, receiver) = mpsc::channel::<Value>();
        let mut sender = Box::new(sender);
        unsafe { runtime.observe(Some(notification), std::ptr::addr_of_mut!(*sender).cast()) };
        receive(&receiver, "initialized", Duration::from_secs(2));
        while receive(&receiver, "phone_key_state", Duration::from_secs(2))["link"] != "unpaired" {}
        assert!(runtime.submit(r#"{"op":"run","request_id":"slow","cmd":"lock","args":[]}"#));
        got.recv_timeout(Duration::from_secs(2)).unwrap();
        let start = Instant::now();
        assert!(runtime.submit(r#"{"op":"get_config"}"#));
        let mut presence = None;
        let config = loop {
            let event = receiver
                .recv_timeout(Duration::from_millis(100).saturating_sub(start.elapsed()))
                .unwrap();
            if event["type"] == "phone_key_event" {
                presence = Some(event);
            } else if event["type"] == "config_loaded" {
                break event;
            }
        };
        assert_eq!(config["vin"], "5YJ3E1EA0PF000000");
        assert!(start.elapsed() < Duration::from_millis(100));
        let event = presence
            .unwrap_or_else(|| receive(&receiver, "phone_key_event", Duration::from_secs(1)));
        assert_eq!(event["kind"], "presence_near");
        assert!(runtime.submit(r#"{"op":"snapshot"}"#));
        receive(&receiver, "initialized", Duration::from_millis(100));
        let snapshot = receive(&receiver, "phone_key_state", Duration::from_millis(100));
        assert_eq!(snapshot["link"], "connected");
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
        peer.join().unwrap();
    }

    #[test]
    fn phone_key_machine_state_tracks_events_independently_of_diagnostics() {
        let dir = tempfile::tempdir().unwrap();
        let events = [
            ("presence_near", "", "connected", true),
            ("presence_auth_ok", "", "authorized", true),
            ("presence_inside", "", "authorized", true),
            ("presence_error", "bluetooth-off", "bluetooth-off", true),
            ("presence_auth_failed", "", "error", true),
            ("presence_disconnected", "", "scanning", true),
            ("presence_stopped", "", "stopped", false),
        ];
        let mut greet = vec![r#"{"type":"hello","v":1}"#.into()];
        for (kind, code, _, _) in events {
            greet.push(json!({"type":"event", "kind":kind, "error_code":code, "error":"diagnostic wording unrelated to state"}).to_string());
        }
        let (session, _, peer) = crate::session_client::tests::accept_mock(dir.path(), greet,
            vec![r#"{"type":"response","id":"{ID}","ok":true,"stdout":"","stderr":"","exit_code":0}"#.into()]);
        // The response barrier proves every preceding event was demultiplexed.
        session
            .run("ping", &[], "VIN", "/key", 1, 1, Duration::from_secs(2))
            .unwrap();
        let state = dir.path().to_string_lossy().into_owned();
        let core = Core::new(state.clone(), state, Some(session)).unwrap();
        let (sender, receiver) = mpsc::channel();
        let sink = Notifications {
            relay: None,
            ..Notifications::default()
        };
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
        };
        phone.poll(&core, &sink, &AtomicU64::new(0));
        for (_, _, link, active) in events {
            let state = receive(&receiver, "phone_key_state", Duration::from_secs(1));
            assert_eq!(state["link"], link);
            assert_eq!(state["active"], active);
        }
        sink.observe(None);
        drop(core);
        peer.join().unwrap();
    }

    #[test]
    fn blocked_key_generation_does_not_hold_the_configuration_lock() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("private_key.pem"), "private").unwrap();
        std::fs::write(dir.path().join("public_key.pem"), "public").unwrap();
        let (session, got, peer) = crate::session_client::tests::accept_blocked_mock(dir.path());
        let state = dir.path().to_string_lossy().into_owned();
        let runtime = Runtime::launch(
            Core::new(state.clone(), state, Some(session)).unwrap(),
            false,
        );
        let (sender, receiver) = mpsc::channel::<Value>();
        let mut sender = Box::new(sender);
        unsafe { runtime.observe(Some(notification), std::ptr::addr_of_mut!(*sender).cast()) };
        receive(&receiver, "initialized", Duration::from_secs(2));
        assert!(runtime.submit(r#"{"op":"generate_key","force":false}"#));
        let request = got.recv_timeout(Duration::from_secs(2)).unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(&request).unwrap()["cmd"],
            "keygen"
        );
        assert!(runtime.submit(r#"{"op":"get_config"}"#));
        assert_eq!(
            receive(&receiver, "config_loaded", Duration::from_millis(100))["has_key"],
            true
        );
        drop(runtime);
        peer.join().unwrap();
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
            move || sink.send(&json!({"type":"test"}))
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
        sink.send(&json!({"type":"after-detach"}));
    }

    #[test]
    fn commands_and_delayed_refresh_are_rust_driven_and_callbacks_detach() {
        let dir = tempfile::tempdir().unwrap();
        let state = dir.path().to_string_lossy().into_owned();
        let runtime = Runtime::new(Core::new(state.clone(), state, None).unwrap());
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
    fn failed_start_is_retried_without_a_ui_poll() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = crate::config::Config {
            vin: "5YJ3E1EA0PF000000".into(),
            vin_state: crate::config::VinState::Paired,
            ..crate::config::Config::default()
        };
        cfg.save(&dir.path().join("config.json")).unwrap();
        std::fs::write(dir.path().join("private_key.pem"), "private").unwrap();
        let state = dir.path().to_string_lossy().into_owned();
        let session = crate::session_client::SessionClient::new(
            dir.path().join("missing"),
            "bluez",
            dir.path().into(),
        );
        let runtime = Runtime::launch(
            Core::new(state.clone(), state, Some(session)).unwrap(),
            false,
        );
        let (sender, receiver) = mpsc::channel::<Value>();
        let mut sender = Box::new(sender);
        unsafe { runtime.observe(Some(notification), std::ptr::addr_of_mut!(*sender).cast()) };
        receive(&receiver, "initialized", Duration::from_secs(2));
        // The real five-second retry deadline must fire autonomously.
        let event = receive(&receiver, "phone_key_event", Duration::from_secs(8));
        assert_eq!(event["kind"], "presence_stopped");
        assert_eq!(event["vin"], "5YJ3E1EA0PF000000");
        assert!(!event["error"].as_str().unwrap().is_empty());
        assert!(event["time"].as_str().unwrap().ends_with('Z'));
        unsafe { runtime.observe(None, std::ptr::null_mut()) };
        drop(runtime);
    }
}
