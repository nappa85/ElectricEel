//! Rust-owned phone-key controller and operation orchestration on top of the
//! in-process Go protocol library (`tesla_ble`).
use crate::{
    commands::Command,
    phone_key::{PresenceEvent, Proximity},
    tesla_ble::{self, Domain, ObservationKind, Operation, Session, Status, Value},
};
use p256::elliptic_curve::sec1::ToEncodedPoint;
use serde::Deserialize;
use std::{
    collections::VecDeque,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc, Mutex,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

/// A phone-key lifecycle/state event for the runtime.
#[derive(Clone, Debug)]
pub(crate) struct SessionEvent {
    pub kind: PresenceEvent,
    pub vin: String,
    pub time: String,
    pub error: String,
    /// The event was caused by the Bluetooth adapter being powered off.
    pub bluetooth_off: bool,
    pub generation: u64,
}

/// Result of one vehicle command.
#[derive(Debug)]
pub(crate) struct RunOutcome {
    pub ok: bool,
    pub stdout: String,
    pub stderr: String,
    pub exit_code: i32,
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum SessionError {
    #[error("{0}")]
    Library(String),
    /// The Bluetooth adapter is powered off (`EE_BLUETOOTH_OFF`).
    #[error("{0}")]
    BluetoothOff(String),
    #[error("operation cancelled")]
    Cancelled,
}

#[derive(Deserialize)]
struct BodyControllerState {
    #[serde(default, rename = "userPresence")]
    user_presence: UserPresence,
}

#[derive(Debug, Default, PartialEq, Eq, Deserialize)]
enum UserPresence {
    #[default]
    #[serde(rename = "VEHICLE_USER_PRESENCE_UNKNOWN")]
    Unknown,
    #[serde(rename = "VEHICLE_USER_PRESENCE_NOT_PRESENT")]
    NotPresent,
    #[serde(rename = "VEHICLE_USER_PRESENCE_PRESENT")]
    Present,
}

fn user_present(payload: &[u8]) -> bool {
    serde_json::from_slice::<BodyControllerState>(payload)
        .is_ok_and(|state| state.user_presence == UserPresence::Present)
}

/// Idle foreground links are closed after this long without a command.
const IDLE_DISCONNECT: Duration = Duration::from_secs(90);
/// Upper bound on a maintenance sleep; every relevant change also wakes it.
const MAINTENANCE_SAFETY_WAKE: Duration = Duration::from_secs(60);

fn seconds(value: i32) -> Duration {
    Duration::from_secs(u64::try_from(value.max(1)).unwrap_or(1))
}
fn failure(error: impl std::fmt::Display) -> SessionError {
    SessionError::Library(error.to_string())
}
/// A non-`Ok` completion as an error, keeping the Bluetooth-off cause typed.
fn rejected(result: tesla_ble::Outcome) -> SessionError {
    if result.status == Status::BluetoothOff {
        SessionError::BluetoothOff(result.error)
    } else {
        SessionError::Library(result.error)
    }
}
fn outcome(result: tesla_ble::Outcome) -> RunOutcome {
    RunOutcome {
        ok: result.status == Status::Ok,
        stdout: String::from_utf8_lossy(&result.payload).into_owned(),
        stderr: result.error,
        exit_code: i32::from(result.status != Status::Ok),
    }
}
fn execute(
    session: &Session,
    operation: Operation,
    timeout: Duration,
) -> Result<tesla_ble::Outcome, SessionError> {
    futures_lite::future::block_on(session.execute(operation, timeout)).map_err(failure)
}
/// The public key an add-key request enrolls (its first, byte-string value).
fn enrollment_key(request: &Operation) -> Option<Vec<u8>> {
    match request {
        Operation::Command { values, .. } => match values.first() {
            Some(Value::Bytes(key)) => Some(key.clone()),
            _ => None,
        },
        _ => None,
    }
}

type Slot = Option<(String, String, Arc<Session>)>;

struct Inner {
    path: PathBuf,
    library: Mutex<Option<Arc<tesla_ble::Library>>>,
    /// Only ever held briefly: blocking teardown happens after the session
    /// was taken out, so `cancel` (GUI thread) never waits for Go.
    session: Mutex<Slot>,
    operation: async_lock::Mutex<()>,
    generation: AtomicU64,
    mode: AtomicBool,
    cancelled: AtomicBool,
    connected: AtomicBool,
    proximity: Mutex<Proximity>,
    last_command: Mutex<Option<Instant>>,
    events: Mutex<VecDeque<SessionEvent>>,
    terminal: Mutex<Option<SessionEvent>>,
    ready: async_channel::Sender<()>,
    wait: async_channel::Receiver<()>,
    /// Wakes the maintenance thread to recompute its deadline.
    maintenance_wake: async_channel::Sender<()>,
    maintenance_wait: async_channel::Receiver<()>,
}

impl Inner {
    fn wake_maintenance(&self) {
        let _ = self.maintenance_wake.try_send(());
    }
    fn current(&self) -> Option<Arc<Session>> {
        self.session
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
            .map(|(_, _, session)| Arc::clone(session))
    }
}

pub(crate) struct ProtocolClient {
    inner: Arc<Inner>,
    controller: Mutex<Option<JoinHandle<()>>>,
    maintenance: Mutex<Option<JoinHandle<()>>>,
}

impl ProtocolClient {
    pub fn new(path: PathBuf) -> Self {
        let (ready, wait) = async_channel::bounded(1);
        let (maintenance_wake, maintenance_wait) = async_channel::bounded(1);
        Self {
            inner: Arc::new(Inner {
                path,
                library: Mutex::new(None),
                session: Mutex::new(None),
                operation: async_lock::Mutex::new(()),
                generation: AtomicU64::new(1),
                mode: AtomicBool::new(false),
                cancelled: AtomicBool::new(false),
                connected: AtomicBool::new(false),
                proximity: Mutex::new(Proximity::default()),
                last_command: Mutex::new(None),
                events: Mutex::new(VecDeque::new()),
                terminal: Mutex::new(None),
                ready,
                wait,
                maintenance_wake,
                maintenance_wait,
            }),
            controller: Mutex::new(None),
            maintenance: Mutex::new(None),
        }
    }
    fn ensure(&self, vin: &str, key: &str) -> Result<Arc<Session>, SessionError> {
        if self.inner.cancelled.load(Ordering::SeqCst) {
            return Err(SessionError::Cancelled);
        }
        // Decide under the slot lock, but stop/join/drop the old session only
        // after releasing it: destroy blocks until Go's teardown completes.
        let old = {
            let mut slot = self
                .inner
                .session
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some((old_vin, old_key, session)) = &*slot {
                if old_vin == vin && old_key == key && !session.stopped() && !session.faulted() {
                    return Ok(Arc::clone(session));
                }
                session.stop();
            }
            slot.take()
        };
        let mut worker = self
            .maintenance
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(old) = worker.take() {
            let _ = old.join();
        }
        drop(old);
        let library = {
            let mut library = self
                .inner
                .library
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if library.is_none() {
                // SAFETY: app-owned fixed library path, never supplied through UI.
                *library =
                    Some(unsafe { tesla_ble::Library::load(&self.inner.path) }.map_err(failure)?);
            }
            Arc::clone(library.as_ref().ok_or_else(|| failure("missing library"))?)
        };
        let signing = crate::keys::load(Path::new(key)).map_err(failure)?;
        let scalar = zeroize::Zeroizing::new(<[u8; 32]>::from(signing.to_bytes()));
        let session = Arc::new(Session::new(library, vin, "", &scalar).map_err(failure)?);
        *self
            .inner
            .session
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) =
            Some((vin.into(), key.into(), Arc::clone(&session)));
        self.inner.connected.store(false, Ordering::SeqCst);
        let inner = Arc::clone(&self.inner);
        let session_for_worker = Arc::clone(&session);
        *worker = Some(thread::spawn(move || {
            maintenance(&inner, &session_for_worker);
        }));
        Ok(session)
    }
    pub fn generation(&self) -> u64 {
        self.inner.generation.load(Ordering::SeqCst)
    }
    pub fn is_alive(&self) -> bool {
        self.inner
            .session
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
            .is_some_and(|(_, _, session)| !session.stopped() && !session.faulted())
    }
    pub fn is_presence_active(&self) -> bool {
        self.inner.mode.load(Ordering::SeqCst) && self.is_alive()
    }
    /// Permanent shutdown. Non-blocking: safe on the GUI thread.
    pub fn cancel(&self) {
        self.inner.cancelled.store(true, Ordering::SeqCst);
        self.inner.mode.store(false, Ordering::SeqCst);
        if let Some((_, _, session)) = &*self
            .inner
            .session
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
        {
            // Stop is non-blocking; the slot lock is never held across teardown.
            session.stop();
        }
        let _ = self.inner.ready.try_send(());
        self.inner.wake_maintenance();
    }
    pub fn invalidate(&self) {
        self.inner.mode.store(false, Ordering::SeqCst);
        if let Some((_, _, session)) = &*self
            .inner
            .session
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
        {
            session.stop();
        }
        self.join_controller();
        if let Some(worker) = self
            .maintenance
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
        {
            let _ = worker.join();
        }
        let _operation = self.inner.operation.lock_blocking();
        let old = self
            .inner
            .session
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        // Destroy (blocking) runs here, with the slot lock already released.
        drop(old);
        self.inner.generation.fetch_add(1, Ordering::SeqCst);
        self.inner.connected.store(false, Ordering::SeqCst);
        self.clear_events();
        *self
            .inner
            .proximity
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Proximity::default();
    }
    fn join_controller(&self) {
        if let Some(worker) = self
            .controller
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
        {
            let _ = worker.join();
        }
    }
    pub fn start_presence(
        &self,
        vin: &str,
        key: &str,
        connect: i32,
        command: i32,
    ) -> Result<(), SessionError> {
        if self.is_presence_active() {
            return Ok(());
        }
        self.join_controller();
        let session = self.ensure(vin, key)?;
        let _operation = self.inner.operation.lock_blocking();
        let result = execute(&session, Operation::WatchStart, seconds(connect))?;
        if result.status != Status::Ok {
            return Err(rejected(result));
        }
        let result = execute(&session, Operation::PassiveEntry(true), seconds(command))?;
        if result.status != Status::Ok {
            return Err(rejected(result));
        }
        self.inner.mode.store(true, Ordering::SeqCst);
        let inner = Arc::clone(&self.inner);
        let vin = vin.to_string();
        let generation = self.generation();
        *self
            .controller
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(thread::spawn(move || {
            controller(
                &inner,
                &session,
                &vin,
                generation,
                seconds(connect),
                seconds(command),
            );
        }));
        Ok(())
    }
    pub fn stop_presence(&self) -> Result<(), SessionError> {
        self.inner.mode.store(false, Ordering::SeqCst);
        self.inner.wake_maintenance();
        let session = self.inner.current();
        if let Some(session) = &session {
            session.cancel_operations();
        }
        self.join_controller();
        let _operation = self.inner.operation.lock_blocking();
        if let Some(session) = session.filter(|session| !session.stopped()) {
            for operation in [
                Operation::PassiveEntry(false),
                Operation::WatchStop,
                Operation::Disconnect,
            ] {
                let result = execute(&session, operation, Duration::from_secs(5))?;
                if result.status != Status::Ok {
                    return Err(failure(result.error));
                }
            }
        }
        self.inner.connected.store(false, Ordering::SeqCst);
        *self
            .inner
            .proximity
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Proximity::default();
        Ok(())
    }
    /// Executes one parsed command. `cmd` selects the link's domain; an
    /// `add-key-request` additionally waits for NFC approval of that key.
    pub fn run(
        &self,
        cmd: Command,
        operation: Operation,
        vin: &str,
        key: &str,
        connect: i32,
        command: i32,
    ) -> Result<RunOutcome, SessionError> {
        if cmd == Command::AddKeyRequest {
            let public = enrollment_key(&operation)
                .ok_or_else(|| failure("add-key-request requires a public key"))?;
            let restart = self.is_presence_active();
            let result = self.enroll(operation, &public, vin, key, connect, command);
            if restart && !self.inner.cancelled.load(Ordering::SeqCst) {
                match self.start_presence(vin, key, connect, command) {
                    Ok(()) => emit(
                        &self.inner,
                        vin,
                        self.generation(),
                        PresenceEvent::Restarted,
                        "",
                        Status::Ok,
                    ),
                    Err(error) => emit(
                        &self.inner,
                        vin,
                        self.generation(),
                        PresenceEvent::Stopped,
                        &error.to_string(),
                        if matches!(error, SessionError::BluetoothOff(_)) {
                            Status::BluetoothOff
                        } else {
                            Status::Failed
                        },
                    ),
                }
            }
            return result;
        }
        self.execute_in(
            crate::protocol_commands::domain(cmd),
            operation,
            vin,
            key,
            connect,
            command,
        )
    }
    /// Executes one operation on a link authenticated for `domain`.
    pub fn execute_in(
        &self,
        domain: Domain,
        operation: Operation,
        vin: &str,
        key: &str,
        connect: i32,
        command: i32,
    ) -> Result<RunOutcome, SessionError> {
        let _operation = self.inner.operation.lock_blocking();
        let session = self.ensure(vin, key)?;
        if self.inner.mode.load(Ordering::SeqCst)
            && !self.inner.connected.load(Ordering::SeqCst)
            && !self
                .inner
                .proximity
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .can_connect(Instant::now())
        {
            return Err(failure(
                "vehicle not nearby; wait for phone key to connect and retry",
            ));
        }
        let result = execute(&session, Operation::Connect(domain), seconds(connect))?;
        if result.status != Status::Ok {
            self.inner.connected.store(false, Ordering::SeqCst);
            return Ok(outcome(result));
        }
        self.inner.connected.store(true, Ordering::SeqCst);
        let result = execute(&session, operation, seconds(command))?;
        if [
            Status::Uncertain,
            Status::Cancelled,
            Status::TimedOut,
            Status::NotConnected,
        ]
        .contains(&result.status)
        {
            self.inner.connected.store(false, Ordering::SeqCst);
        }
        *self
            .inner
            .last_command
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(Instant::now());
        self.inner.wake_maintenance();
        Ok(outcome(result))
    }
    /// Enrolls this phone's own key as an owner phone key. The public file must
    /// belong to `key`, so an interrupted key rotation can never enroll a stale
    /// public half.
    pub fn pair(
        &self,
        public_key: &Path,
        vin: &str,
        key: &str,
        connect: i32,
        command: i32,
    ) -> Result<RunOutcome, SessionError> {
        let public = crate::keys::public_bytes(public_key).map_err(failure)?;
        if public
            != crate::keys::load(Path::new(key))
                .map_err(failure)?
                .public_key()
                .to_encoded_point(false)
                .as_bytes()
        {
            return Err(failure("pairing public key does not match private key"));
        }
        let args = [
            public_key.to_string_lossy().into_owned(),
            "owner".into(),
            "android_device".into(),
        ];
        let request =
            crate::protocol_commands::parse(Command::AddKeyRequest, &args).map_err(failure)?;
        self.enroll(request, &public, vin, key, connect, command)
    }
    fn enroll(
        &self,
        request: Operation,
        public: &[u8],
        vin: &str,
        key: &str,
        connect: i32,
        command: i32,
    ) -> Result<RunOutcome, SessionError> {
        self.stop_presence()?;
        let _operation = self.inner.operation.lock_blocking();
        let session = self.ensure(vin, key)?;
        // Never retain a connection established for unauthenticated enrollment,
        // regardless of rejection, deadline, cancellation or verification result.
        let _disconnect = DisconnectOnDrop {
            session: Arc::clone(&session),
        };
        let result = execute(
            &session,
            Operation::Connect(Domain::Unauthenticated),
            seconds(connect),
        )?;
        if result.status != Status::Ok {
            return Ok(outcome(result));
        }
        let sent = execute(&session, request, seconds(command))?;
        if sent.status != Status::Ok {
            return Ok(outcome(sent));
        }
        let deadline = Instant::now() + Duration::from_secs(90);
        loop {
            if self.inner.cancelled.load(Ordering::SeqCst) {
                return Err(SessionError::Cancelled);
            }
            let result = execute(
                &session,
                Operation::EnrollmentStatus(public.to_vec()),
                Duration::from_secs(3).min(deadline.saturating_duration_since(Instant::now())),
            )?;
            if result.status == Status::Ok {
                let _ = execute(&session, Operation::Disconnect, Duration::from_secs(5));
                return Ok(outcome(result));
            }
            if Instant::now() >= deadline {
                let _ = execute(&session, Operation::Disconnect, Duration::from_secs(5));
                return Err(failure("NFC enrollment was not confirmed"));
            }
            for _ in 0..10 {
                if self.inner.cancelled.load(Ordering::SeqCst) {
                    return Err(SessionError::Cancelled);
                }
                thread::sleep(Duration::from_millis(100));
            }
        }
    }
    pub fn poll_event(&self) -> Option<SessionEvent> {
        if let Some(event) = self
            .inner
            .terminal
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
        {
            return Some(event);
        }
        self.inner
            .events
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .pop_front()
    }
    pub async fn wait_event(&self) {
        let _ = self.inner.wait.recv().await;
    }
    pub fn clear_events(&self) {
        self.inner
            .terminal
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        self.inner
            .events
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clear();
        while self.inner.wait.try_recv().is_ok() {}
    }
}
struct DisconnectOnDrop {
    session: Arc<Session>,
}
impl Drop for DisconnectOnDrop {
    fn drop(&mut self) {
        if !self.session.stopped() {
            let _ = execute(&self.session, Operation::Disconnect, Duration::from_secs(5));
        }
    }
}
impl Drop for ProtocolClient {
    fn drop(&mut self) {
        self.cancel();
        self.invalidate();
    }
}
fn emit(
    inner: &Inner,
    vin: &str,
    generation: u64,
    kind: PresenceEvent,
    error: &str,
    status: Status,
) {
    let event = SessionEvent {
        kind,
        vin: vin.into(),
        time: crate::keylog::utc_stamp(),
        error: error.into(),
        bluetooth_off: status == Status::BluetoothOff,
        generation,
    };
    if kind == PresenceEvent::Stopped {
        *inner
            .terminal
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(event);
        inner
            .events
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clear();
        let _ = inner.ready.try_send(());
        return;
    }
    let mut events = inner
        .events
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    // Coalesce derived state snapshots, never command completions. A terminal
    // lifecycle event has its own priority slot and cannot be evicted.
    events.retain(|old| old.kind != event.kind);
    events.push_back(event);
    drop(events);
    let _ = inner.ready.try_send(());
}

#[allow(clippy::too_many_lines)] // Single owner for the phone-key transition loop.
fn controller(
    inner: &Arc<Inner>,
    session: &Arc<Session>,
    vin: &str,
    generation: u64,
    connect: Duration,
    command: Duration,
) {
    let mut connected_at = None;
    let mut status_at = None;
    let mut present = false;
    let mut inside = false;
    while inner.mode.load(Ordering::SeqCst)
        && !inner.cancelled.load(Ordering::SeqCst)
        && generation == inner.generation.load(Ordering::SeqCst)
    {
        if session.faulted() || session.stopped() {
            session.stop();
            emit(
                inner,
                vin,
                generation,
                PresenceEvent::Stopped,
                "protocol session faulted",
                Status::InternalBug,
            );
            break;
        }
        while let Some(event) = session.poll_observation() {
            match event.kind {
                ObservationKind::Advertisement => {
                    let mut proximity = inner
                        .proximity
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    let was_near = proximity.near;
                    proximity.advertisement(Instant::now(), event.target, event.rssi);
                    if proximity.near && !was_near {
                        crate::keylog::log("presence", &format!("near rssi={}", event.rssi));
                    }
                }
                ObservationKind::Disconnected => {
                    inner.connected.store(false, Ordering::SeqCst);
                    connected_at = None;
                    status_at = None;
                    inside = false;
                    emit(
                        inner,
                        vin,
                        generation,
                        PresenceEvent::Disconnected,
                        &event.error,
                        event.status,
                    );
                }
                ObservationKind::Authenticated => emit(
                    inner,
                    vin,
                    generation,
                    PresenceEvent::AuthOk,
                    "",
                    event.status,
                ),
                ObservationKind::AuthFailed => emit(
                    inner,
                    vin,
                    generation,
                    PresenceEvent::AuthFailed,
                    &event.error,
                    event.status,
                ),
                ObservationKind::WatchFailed => {
                    emit(
                        inner,
                        vin,
                        generation,
                        PresenceEvent::Error,
                        &event.error,
                        event.status,
                    );
                    session.stop();
                    emit(
                        inner,
                        vin,
                        generation,
                        PresenceEvent::Stopped,
                        &event.error,
                        event.status,
                    );
                    inner.mode.store(false, Ordering::SeqCst);
                    return;
                }
                // Log observations are routed to diagnostics by the callback.
                ObservationKind::Log => (),
            }
        }
        let connected = inner.connected.load(Ordering::SeqCst);
        let now = Instant::now();
        let (depart, can_connect, target) = {
            let mut proximity = inner
                .proximity
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            (
                proximity.tick(now, connected),
                proximity.can_connect(now),
                proximity.target,
            )
        };
        if let Some(_operation) = inner.operation.try_lock() {
            if depart {
                let _ = execute(session, Operation::Disconnect, command);
                emit(inner, vin, generation, PresenceEvent::Far, "", Status::Ok);
            }
            if !connected && can_connect {
                match execute(
                    session,
                    Operation::ConnectTarget {
                        domain: Domain::Vcsec,
                        target,
                    },
                    connect,
                ) {
                    Ok(result) if result.status == Status::Ok => {
                        inner.connected.store(true, Ordering::SeqCst);
                        inner
                            .proximity
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .connected();
                        connected_at = Some(Instant::now());
                        status_at = None;
                        inside = false;
                        emit(inner, vin, generation, PresenceEvent::Near, "", Status::Ok);
                    }
                    result => {
                        inner
                            .proximity
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .failed(Instant::now());
                        let (status, error) = match result {
                            Ok(result) => (result.status, result.error),
                            Err(error) => (Status::Failed, error.to_string()),
                        };
                        emit(inner, vin, generation, PresenceEvent::Error, &error, status);
                    }
                }
            }
            if inner.connected.load(Ordering::SeqCst)
                && status_at.is_none_or(|at: Instant| at.elapsed() >= Duration::from_secs(15))
            {
                match execute(session, Operation::BodyControllerState, command) {
                    Ok(result) if result.status == Status::Ok => {
                        present = user_present(&result.payload);
                        status_at = Some(Instant::now());
                        if connected_at.is_none() {
                            connected_at = Some(Instant::now());
                        }
                    }
                    result => {
                        present = false;
                        status_at = Some(Instant::now());
                        if let Ok(result) = result {
                            if [
                                Status::Uncertain,
                                Status::Cancelled,
                                Status::TimedOut,
                                Status::NotConnected,
                            ]
                            .contains(&result.status)
                            {
                                inner.connected.store(false, Ordering::SeqCst);
                                connected_at = None;
                                inside = false;
                                emit(
                                    inner,
                                    vin,
                                    generation,
                                    PresenceEvent::Disconnected,
                                    &result.error,
                                    result.status,
                                );
                            }
                        }
                    }
                }
            }
        }
        if !inside
            && connected_at
                .zip(status_at)
                .is_some_and(|(connected, status)| {
                    crate::phone_key::inside(connected, status, Instant::now(), present)
                })
        {
            inside = true;
            emit(
                inner,
                vin,
                generation,
                PresenceEvent::Inside,
                "",
                Status::Ok,
            );
        }
        thread::sleep(Duration::from_millis(100));
    }
    inner.mode.store(false, Ordering::SeqCst);
}

enum MaintenanceWake {
    Diagnostic(String),
    Other,
}

/// Per-session housekeeping: writes Go diagnostics to the daily log, discards
/// raw observations while presence is off (only the controller consumes them,
/// and an unread backlog would otherwise overflow and fault the session) and
/// closes idle foreground links. Sleeps until a diagnostic, an observation
/// (presence off), an explicit wake or the idle deadline.
fn maintenance(inner: &Arc<Inner>, session: &Arc<Session>) {
    let mut lost = 0;
    while !inner.cancelled.load(Ordering::SeqCst) && !session.stopped() {
        while let Some(message) = session.poll_diagnostic() {
            crate::keylog::log("go", &message);
        }
        let current = session.lost_diagnostics();
        if current != lost {
            crate::keylog::log(
                "go",
                &format!(
                    "diagnostic queue overflow: {} records dropped",
                    current - lost
                ),
            );
            lost = current;
        }
        let presence = inner.mode.load(Ordering::SeqCst);
        if !presence {
            while session.poll_observation().is_some() {}
        }
        let last_command = *inner
            .last_command
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let idle_at = last_command
            .filter(|_| !presence)
            .map(|at| at + IDLE_DISCONNECT);
        if idle_at.is_some_and(|at| Instant::now() >= at) {
            if let Some(_operation) = inner.operation.try_lock() {
                let _ = execute(session, Operation::Disconnect, Duration::from_secs(5));
                inner.connected.store(false, Ordering::SeqCst);
                *inner
                    .last_command
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
                continue;
            }
        }
        let now = Instant::now();
        let deadline = idle_at
            .filter(|at| *at > now)
            .map_or(now + MAINTENANCE_SAFETY_WAKE, |at| {
                // A busy foreground operation defers the idle check briefly.
                at.min(now + MAINTENANCE_SAFETY_WAKE)
            })
            .max(now + Duration::from_millis(100));
        let wake = async_io::block_on(futures_lite::future::race(
            async {
                match session.diagnostic().await {
                    Ok(message) => MaintenanceWake::Diagnostic(message),
                    Err(_) => MaintenanceWake::Other,
                }
            },
            futures_lite::future::race(
                async {
                    let _ = inner.maintenance_wait.recv().await;
                    MaintenanceWake::Other
                },
                futures_lite::future::race(
                    async {
                        if presence {
                            std::future::pending::<()>().await;
                        }
                        session.wait_observation().await;
                        MaintenanceWake::Other
                    },
                    async {
                        async_io::Timer::at(deadline).await;
                        MaintenanceWake::Other
                    },
                ),
            ),
        ));
        if let MaintenanceWake::Diagnostic(message) = wake {
            crate::keylog::log("go", &message);
        }
    }
    while let Some(message) = session.poll_diagnostic() {
        crate::keylog::log("go", &message);
    }
}

#[cfg(test)]
impl ProtocolClient {
    /// Test-only: a client whose protocol library is already loaded (e.g. the
    /// in-process `tesla_ble::fake`), so no shared object is opened.
    pub(crate) fn with_library(library: Arc<tesla_ble::Library>) -> Self {
        let client = Self::new(PathBuf::from("/test-library-is-preloaded"));
        *client.inner.library.lock().unwrap() = Some(library);
        client
    }

    /// Test-only: publish a phone-key event as the controller would.
    pub(crate) fn emit_for_test(
        &self,
        vin: &str,
        kind: PresenceEvent,
        error: &str,
        bluetooth_off: bool,
    ) {
        emit(
            &self.inner,
            vin,
            self.generation(),
            kind,
            error,
            if bluetooth_off {
                Status::BluetoothOff
            } else {
                Status::Ok
            },
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn presence_payload_validates_protocol_enum_and_decodes_escapes() {
        let plain =
            br#"{"userPresence":"VEHICLE_USER_PRESENCE_PRESENT","ignored":{"nested":[1,true]}}"#;
        assert!(user_present(plain));

        let escaped = br#"{"userPresence":"VEHICLE_USER_PRESENCE_\u0050RESENT"}"#;
        assert!(user_present(escaped));

        for (name, expected) in [
            ("VEHICLE_USER_PRESENCE_UNKNOWN", UserPresence::Unknown),
            (
                "VEHICLE_USER_PRESENCE_NOT_PRESENT",
                UserPresence::NotPresent,
            ),
            ("VEHICLE_USER_PRESENCE_PRESENT", UserPresence::Present),
        ] {
            let payload = format!(r#"{{"userPresence":"{name}"}}"#);
            let state: BodyControllerState = serde_json::from_str(&payload).unwrap();
            assert_eq!(state.user_presence, expected);
        }
        assert!(
            serde_json::from_str::<BodyControllerState>(r#"{"userPresence":"UNKNOWN"}"#).is_err()
        );
    }

    #[test]
    fn absent_or_invalid_presence_is_not_present() {
        for payload in [
            "{}",
            r#"{"userPresence":null}"#,
            r#"{"userPresence":42}"#,
            r#"{"userPresence":{"unexpected":true}}"#,
            r#"{"userPresence":"VEHICLE_USER_PRESENCE_NOT_PRESENT"}"#,
            r#"{"userPresence":"VEHICLE_USER_PRESENCE_UNKNOWN"}"#,
            r#"{"userPresence":"UNKNOWN"}"#,
            r#"{"userPresence":"VEHICLE_USER_PRESENCE_PRESENT","broken":}"#,
            "null",
            "[]",
        ] {
            assert!(!user_present(payload.as_bytes()), "{payload}");
        }
    }

    #[test]
    fn terminal_state_is_not_lost_in_an_observation_storm() {
        let client = ProtocolClient::new(PathBuf::from("/unused"));
        for _ in 0..200 {
            emit(&client.inner, "V", 1, PresenceEvent::AuthOk, "", Status::Ok);
        }
        emit(
            &client.inner,
            "V",
            1,
            PresenceEvent::Stopped,
            "bug",
            Status::InternalBug,
        );
        for _ in 0..200 {
            emit(
                &client.inner,
                "V",
                1,
                PresenceEvent::Error,
                "radio",
                Status::Failed,
            );
        }
        assert_eq!(client.poll_event().unwrap().kind, PresenceEvent::Stopped);
        assert_eq!(client.inner.events.lock().unwrap().len(), 1);
    }

    #[test]
    fn pairing_rejects_a_stale_public_half_before_loading_go_or_touching_ble() {
        let directory = tempfile::tempdir().unwrap();
        let private = directory.path().join("private_key.pem");
        let public = directory.path().join("public_key.pem");
        crate::keys::generate(&private, &public, false).unwrap();
        let other_private = directory.path().join("other_private.pem");
        let other_public = directory.path().join("other_public.pem");
        crate::keys::generate(&other_private, &other_public, false).unwrap();
        let client = ProtocolClient::new(PathBuf::from("/must-not-load"));
        let error = client
            .pair(
                &other_public,
                "5YJ3E1EA7KF000001",
                private.to_str().unwrap(),
                20,
                5,
            )
            .err()
            .unwrap();
        assert!(error.to_string().contains("does not match"));
        assert!(client.inner.library.lock().unwrap().is_none());
    }

    #[test]
    #[ignore = "requires the built Go shared library"]
    fn real_library_uses_rust_persisted_keys_and_invalidates_without_children() {
        let path = std::env::var_os("TESLA_BLE_TEST_LIBRARY").expect("TESLA_BLE_TEST_LIBRARY");
        let directory = tempfile::tempdir().unwrap();
        let client = ProtocolClient::new(path.into());
        let private = directory.path().join("private_key.pem");
        let public = directory.path().join("public_key.pem");
        crate::keys::generate(&private, &public, false).unwrap();
        let session = client
            .ensure("5YJ3E1EA7KF000001", private.to_str().unwrap())
            .unwrap();
        let result = execute(&session, Operation::PublicKey, Duration::from_secs(1)).unwrap();
        assert_eq!(result.status, Status::Ok);
        assert_eq!(result.payload, crate::keys::public_bytes(&public).unwrap());
        let generation = client.generation();
        client.invalidate();
        assert!(session.stopped());
        assert!(client.generation() > generation);
        assert!(!client.is_alive());
        drop(session);
        drop(client);
    }
}
