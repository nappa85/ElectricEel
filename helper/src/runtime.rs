//! Autonomous application runtime: commands, presence/retries, UI notifications
//! and delayed status refreshes all run on Rust threads, without Qt timers.
use std::ffi::{c_void, CString};
use std::os::raw::c_char;
use std::sync::{mpsc, Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use serde::Deserialize;
use serde_json::{json, Value};

use crate::core::Core;
use crate::cpukeepalive::CpuKeepAlive;

pub type UiCallback = unsafe extern "C" fn(*mut c_void, *const c_char);

struct Observer {
    callback: UiCallback,
    context: usize,
}

#[derive(Default)]
struct Notifications(Mutex<Option<Observer>>);

impl Notifications {
    fn send(&self, value: &Value) {
        let guard = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(observer) = &*guard {
            let data = CString::new(value.to_string()).expect("JSON escapes NULs");
            // SAFETY: observer registration requires a live context until
            // detachment returns. The mutex makes detachment a callback barrier.
            unsafe { (observer.callback)(observer.context as *mut c_void, data.as_ptr()) };
        }
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
    requests: mpsc::SyncSender<Request>,
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
        let (requests, receiver) = mpsc::sync_channel(64);
        let worker = thread::spawn(move || run(&core, &receiver, &sink));
        Self {
            requests,
            notifications,
            worker: Some(worker),
            keepalive,
        }
    }

    pub(crate) fn submit(&self, json: &str) -> bool {
        serde_json::from_str(json).is_ok_and(|request| self.requests.try_send(request).is_ok())
    }

    /// # Safety
    /// The context must remain live until detachment returns. Callbacks must
    /// copy the borrowed JSON and enqueue UI work; never block or re-enter us.
    pub(crate) unsafe fn observe(&self, callback: Option<UiCallback>, context: *mut c_void) {
        *self
            .notifications
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) =
            callback.map(|callback| Observer {
                callback,
                context: context as usize,
            });
        if callback.is_some() {
            let _ = self.requests.try_send(Request::Snapshot);
        }
    }
}

impl Drop for Runtime {
    fn drop(&mut self) {
        // Detach before joining so shutdown cannot call into a destroyed UI.
        *self
            .notifications
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
        let _ = self.requests.send(Request::Shutdown);
        if let Some(worker) = self.worker.take() {
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
}

impl PhoneState {
    fn publish(&self, sink: &Notifications) {
        sink.send(&json!({"type":"phone_key_state", "active":self.active, "status":self.status}));
    }

    fn start(&mut self, core: &Core, sink: &Notifications) {
        match core.start_phone_key() {
            Ok(()) => {
                self.active = true;
                self.status = "Phone key scanning".into();
            }
            Err(error) => {
                self.active = false;
                self.status = error.to_string();
            }
        }
        self.publish(sink);
    }

    fn poll(&mut self, core: &Core, sink: &Notifications) {
        while let Some(event) = core.poll_phone_key_event() {
            match event.kind.as_str() {
                "presence_stopped" => self.active = false,
                "presence_restarted" | "presence_near" => self.active = true,
                _ => (),
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

fn run(core: &Core, receiver: &mpsc::Receiver<Request>, sink: &Notifications) {
    let mut phone = PhoneState {
        active: false,
        status: "Phone key inactive".into(),
    };
    phone.start(core, sink);
    let mut poll_at = Instant::now();
    let mut refresh_at = None;
    let mut suspended = false;
    loop {
        let now = Instant::now();
        if now >= poll_at {
            phone.poll(core, sink);
            poll_at = Instant::now() + Duration::from_secs(1);
        }
        if refresh_at.is_some_and(|deadline| now >= deadline) {
            refresh_at = None;
            sink.send(&json!({"type":"status_refresh_requested"}));
        }
        let deadline = refresh_at.map_or(poll_at, |refresh| poll_at.min(refresh));
        match receiver.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
            Ok(Request::Shutdown) | Err(mpsc::RecvTimeoutError::Disconnected) => break,
            Err(mpsc::RecvTimeoutError::Timeout) => (),
            Ok(Request::ApplicationState { state }) => {
                crate::keylog::log("ui", &format!("applicationState={state}"));
                if state == "suspended" {
                    suspended = true;
                } else if state == "active" && suspended {
                    suspended = false;
                    core.handle_resume();
                    phone.start(core, sink);
                }
            }
            Ok(Request::LogUi { message }) => crate::keylog::log("ui", &message),
            Ok(Request::Snapshot) => {
                sink.send(&json!({"type":"initialized", "ok":true}));
                phone.publish(sink);
            }
            Ok(request) => {
                let refresh = matches!(
                    &request,
                    Request::Run {
                        refresh_status: true,
                        ..
                    }
                );
                dispatch(core, request, sink, &mut phone);
                // Vehicle settle timing belongs to command completion, never
                // to whether the UI has processed the result notification.
                if refresh {
                    refresh_at = Some(Instant::now() + Duration::from_millis(2500));
                }
            }
        }
    }
    core.stop_phone_key();
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
        let mut cfg = crate::config::Config::default();
        cfg.vin = "5YJ3E1EA0PF000000".into();
        cfg.vin_state = crate::config::VinState::Paired;
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
