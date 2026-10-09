//! Client for the persistent tesla-session companion process (see
//! helper/session/), which keeps one *vehicle.Vehicle BLE session alive
//! across many commands instead of paying a full connect+StartSession
//! handshake per command, the way spawning a fresh `tesla-control` does.
//!
//! Transport is a private Unix-domain socket, not stdio: the parent binds
//! a uniquely-named path under the state dir, spawns the child with
//! `--socket-path`, accepts exactly one connection, then unlinks the path
//! so no other same-UID process can dial in later and spoof responses.
//! Frames are tagged newline-delimited JSON (`{"type":...}`), versioned by
//! a `hello` handshake — never shape-sniffed. This removes the old stdio
//! transport's failure modes: response/event ambiguity, interleaving with
//! command output on stdout, and silent version skew.
//!
//! The child heartbeats every 10s (even mid-command); any frame resets the
//! reader's 30s deadline, so a wedged-but-silent child is killed and
//! reported instead of hanging the next command. On any failure at this
//! layer the whole child is dropped and an error reported — `Core`
//! surfaces it (no silent fallback), so a bug here degrades to a visible
//! error rather than to a wrong reply.
//!
//! tesla-session's presence-maintenance loop (presence-start/presence-stop)
//! also writes unsolicited `event` frames outside any request/response
//! pairing. The socket reader demultiplexes those into a bounded side
//! queue (capacity 64, oldest dropped) so a slow consumer cannot wedge the
//! child. Rust's autonomous runtime drains the queue and pushes state to QML;
//! the UI never needs to poll to keep the session healthy.
//!
//! Socket accept, handshake, reads, writes and response deadlines are async.
//! An async mutex enforces single-flight requests; synchronous Core/FFI callers
//! drive the same futures with `block_on`. Shutdown closes a cancellation channel
//! independent of that mutex. Cancelling a request destroys its session instead
//! of allowing a late response to be paired with a subsequent command.

use async_io::{Async, Timer};
use futures_lite::{
    future,
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader as AsyncBufReader},
};
use std::collections::VecDeque;
#[cfg(test)]
use std::io::{BufRead, BufReader, Write};
use std::net::Shutdown;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
#[cfg(test)]
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
#[cfg(test)]
use std::thread;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::child::KillOnDrop;

/// How long a BLE session may sit idle before tesla-session tears it down
/// and lets the vehicle/adapter go back to sleep. Not user-configurable
/// (see `docs/limitations.md`: shipped as an invisible optimization, not a
/// Settings toggle).
pub(crate) const IDLE_TIMEOUT_SEC: u32 = 90;

/// Framing contract version. Must match tesla-session's `protocolVersion`
/// (see helper/session/serve.go); a mismatch kills the child with a
/// version error instead of parsing frames it doesn't understand.
pub(crate) const PROTOCOL_VERSION: u32 = 1;

/// Includes the terminating newline; matches Go's 1 MiB request ceiling.
const MAX_FRAME_BYTES: usize = 1024 * 1024;

async fn read_frame(
    reader: &mut (impl futures_lite::io::AsyncBufRead + Unpin),
    line: &mut String,
) -> std::io::Result<usize> {
    // Read at most one excess byte so an unterminated stream cannot allocate
    // beyond the ceiling or wait for a newline before rejecting oversize input.
    let count = reader
        .take((MAX_FRAME_BYTES + 1) as u64)
        .read_line(line)
        .await?;
    if count > MAX_FRAME_BYTES || (count != 0 && !line.ends_with('\n')) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "oversized or unterminated session frame",
        ));
    }
    Ok(count)
}

#[derive(Serialize)]
struct Request<'a> {
    #[serde(rename = "type")]
    kind: &'a str,
    id: &'a str,
    cmd: &'a str,
    args: &'a [String],
}

#[derive(Debug, Deserialize)]
struct Response {
    id: String,
    ok: bool,
    stdout: String,
    stderr: String,
    exit_code: i32,
}

/// An unsolicited phone-key event emitted by tesla-session.
#[derive(Clone, Debug, Deserialize)]
pub(crate) struct SessionEvent {
    pub kind: String,
    #[serde(default)]
    pub vin: String,
    #[serde(default)]
    pub time: String,
    #[serde(default)]
    pub error: String,
    #[serde(default)]
    pub error_code: String,
    #[serde(skip)]
    pub generation: u64,
}

/// Either shape a frame on tesla-session's socket can take, discriminated
/// by the sender-set `"type"` field — never inferred from the payload
/// shape. `run()` waits specifically for a `Response`, diverting `Event`
/// frames to the side queue and treating `Heartbeat` as liveness.
#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
enum Frame {
    #[serde(rename = "response")]
    Response(Response),
    #[serde(rename = "event")]
    Event(SessionEvent),
    #[serde(rename = "heartbeat")]
    Heartbeat {
        // Payload unused: any successfully-read frame (including this
        // one) is the liveness signal. Kept for protocol compatibility.
        #[allow(dead_code)]
        unix: i64,
    },
    #[serde(rename = "hello")]
    Hello { v: u32 },
}

#[derive(Debug)]
pub(crate) struct RunOutcome {
    pub ok: bool,
    pub stdout: String,
    pub stderr: String,
    pub exit_code: i32,
}

#[derive(Debug)]
pub(crate) enum SessionError {
    Spawn(std::io::Error),
    /// Accept, hello, or version-handshake failure (holds the reason).
    /// The child (if any) is reaped; nothing half-connected survives.
    Handshake(String),
    BrokenPipe,
    Timeout,
    Decode(serde_json::Error),
    /// The response's id didn't match the request that was just sent - a
    /// protocol violation (stray/duplicate line, or a bug in
    /// tesla-session), treated as fatal for this child rather than
    /// silently pairing a request with the wrong reply.
    IdMismatch,
    Cancelled,
}

impl std::fmt::Display for SessionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SessionError::Spawn(e) => write!(f, "failed to spawn tesla-session: {e}"),
            SessionError::Handshake(e) => write!(f, "tesla-session handshake failed: {e}"),
            SessionError::BrokenPipe => write!(f, "tesla-session connection closed"),
            SessionError::Timeout => write!(f, "tesla-session did not respond in time"),
            SessionError::Decode(e) => write!(f, "malformed tesla-session frame: {e}"),
            SessionError::IdMismatch => write!(f, "tesla-session response id mismatch"),
            SessionError::Cancelled => write!(f, "tesla-session operation cancelled"),
        }
    }
}

pub(crate) struct ChildHandle {
    /// None only in tests driving a mock peer (nothing to reap).
    /// Production always holds the spawned tesla-session here, wrapped so
    /// any drop path (including panic unwind) still kills and reaps it —
    /// see `crate::child::KillOnDrop`.
    child: Option<KillOnDrop>,
    /// Write half of the session socket. Reads live on a cloned handle in
    /// the reader task; writes happen only under the client's `child`
    /// lock (single-flight by contract), so no write mutex is needed.
    stream: Async<UnixStream>,
    rx: async_channel::Receiver<String>,
    /// Bound socket path, removed on kill. Unlinked right after accept so
    /// the accept window is the only time the path exists.
    sock_path: PathBuf,
    alive: Arc<AtomicBool>,
    cancelled: Arc<AtomicBool>,
    reader_task: Option<async_global_executor::Task<()>>,
}

impl Drop for ChildHandle {
    /// Backstop for the socket path: `KillOnDrop` already reaps the
    /// process above; unlinking here guarantees no stale path survives
    /// any drop path either. Both halves ignore errors (nothing sensible
    /// to do in Drop, and double unlink/kill is harmless).
    fn drop(&mut self) {
        self.cancelled.store(true, Ordering::SeqCst);
        let _ = self.stream.get_ref().shutdown(Shutdown::Both);
        if let Some(reader) = self.reader_task.take() {
            // The shared reactor keeps running while the compatibility caller
            // waits. Await termination before clearing this generation's events.
            async_global_executor::block_on(reader);
        }
        drop(self.child.take());
        let _ = std::fs::remove_file(&self.sock_path);
    }
}

struct SocketPathGuard(PathBuf);

impl Drop for SocketPathGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// Spawns an async task that owns the socket's read half for the
/// child lifetime, forwarding complete response lines onto a channel and
/// diverting events/heartbeats. Async timers enforce the frame deadline: any
/// gap longer than `frame_timeout` (no response, event, or heartbeat)
/// marks the child dead rather than hanging the caller. EOF/read errors also
/// publish a stopped event so the core can reap and restart it while idle.
#[allow(clippy::too_many_arguments)]
fn spawn_reader(
    reader: AsyncBufReader<Async<UnixStream>>,
    events: Arc<Mutex<VecDeque<SessionEvent>>>,
    alive: Arc<AtomicBool>,
    cancelled: Arc<AtomicBool>,
    frame_timeout: Duration,
    generation: u64,
    event_ready: async_channel::Sender<()>,
    shutdown: async_channel::Receiver<()>,
) -> (
    async_channel::Receiver<String>,
    async_global_executor::Task<()>,
) {
    // Bounded to 1: exactly one response is consumed per run(). An unbounded
    // channel would let a buggy child flooding responses grow memory without
    // bound. A full channel is a protocol violation: kill the child.
    let (tx, rx) = async_channel::bounded(1);
    let task = async_global_executor::spawn(async move {
        let mut reader = reader;
        let mut line = String::new();
        loop {
            line.clear();
            match future::race(
                read_frame(&mut reader, &mut line),
                future::race(
                    async {
                        Timer::after(frame_timeout).await;
                        Err(std::io::Error::new(
                            std::io::ErrorKind::TimedOut,
                            "frame deadline",
                        ))
                    },
                    async {
                        let _ = shutdown.recv().await;
                        Err(std::io::Error::new(
                            std::io::ErrorKind::Interrupted,
                            "shutdown",
                        ))
                    },
                ),
            )
            .await
            {
                Ok(0) => break, // EOF: child exited or connection reset
                Ok(_) => {
                    let raw = line.trim_end().to_string();
                    match serde_json::from_str::<Frame>(&raw) {
                        Ok(Frame::Response(_)) => {
                            if tx.try_send(raw).is_err() {
                                break;
                            }
                        }
                        Ok(Frame::Event(mut event)) => {
                            event.generation = generation;
                            let mut queue = events
                                .lock()
                                .unwrap_or_else(std::sync::PoisonError::into_inner);
                            if queue.len() == 64 {
                                queue.pop_front();
                            }
                            queue.push_back(event);
                            let _ = event_ready.try_send(());
                        }
                        Ok(Frame::Heartbeat { .. }) => {
                            // Liveness only; the successful read itself is
                            // what holds the frame deadline open.
                        }
                        Ok(Frame::Hello { .. }) => {
                            // Only legal as the first frame (consumed by the
                            // handshake); a later one is a protocol violation.
                            break;
                        }
                        Err(_) => {
                            // Unparseable frame: fatal for this child, same
                            // as the old transport — never skip-and-continue
                            // past data we can't classify.
                            break;
                        }
                    }
                }
                // No frame (not even a heartbeat) within the deadline: the
                // child is wedged. Break so run() fails fast instead of
                // waiting out the full command deadline; run() owns the
                // Child and reaps it.
                Err(e)
                    if e.kind() == std::io::ErrorKind::WouldBlock
                        || e.kind() == std::io::ErrorKind::TimedOut =>
                {
                    break;
                }
                Err(_) => break,
            }
        }
        alive.store(false, Ordering::SeqCst);
        if !cancelled.load(Ordering::SeqCst) {
            let mut queue = events
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            queue.clear();
            queue.push_back(SessionEvent {
                kind: "presence_stopped".to_string(),
                vin: String::new(),
                time: String::new(),
                error: "tesla-session transport closed".to_string(),
                error_code: String::new(),
                generation,
            });
            let _ = event_ready.try_send(());
        }
    });
    (rx, task)
}

pub struct SessionClient {
    bin_path: PathBuf,
    ble_backend: String,
    state_dir: PathBuf,
    child: async_lock::Mutex<Option<ChildHandle>>,
    shutdown_tx: async_channel::Sender<()>,
    shutdown_rx: async_channel::Receiver<()>,
    transport_alive: Arc<AtomicBool>,
    generation: AtomicU64,
    event_ready: async_channel::Sender<()>,
    event_wait: async_channel::Receiver<()>,
    next_id: AtomicU64,
    /// Separate nonce for socket paths: request IDs and socket suffixes must
    /// not share a counter (a failed spawn without run would otherwise reuse
    /// a request id as a path suffix, confusing diagnostics).
    sock_nonce: AtomicU64,
    events: Arc<Mutex<VecDeque<SessionEvent>>>,
    presence_active: AtomicBool,
    /// Bound on socket accept + hello handshake.
    handshake_timeout: Duration,
    /// Deadline per frame on an established connection. Must exceed the
    /// child's heartbeat interval by several multiples.
    frame_timeout: Duration,
}

/// Dropping an in-flight future is cancellation too. A possibly transmitted
/// command must never leave a session reusable with an outstanding response.
struct InFlight<'a> {
    slot: async_lock::MutexGuard<'a, Option<ChildHandle>>,
    client: &'a SessionClient,
    complete: bool,
}

impl std::ops::Deref for InFlight<'_> {
    type Target = Option<ChildHandle>;
    fn deref(&self) -> &Self::Target {
        &self.slot
    }
}
impl std::ops::DerefMut for InFlight<'_> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.slot
    }
}
impl Drop for InFlight<'_> {
    fn drop(&mut self) {
        if !self.complete {
            if let Some(handle) = self.slot.take() {
                drop(handle);
                self.client.transport_failed();
            }
        }
    }
}

impl SessionClient {
    #[must_use]
    pub fn new(bin_path: PathBuf, ble_backend: &str, state_dir: PathBuf) -> Self {
        let (shutdown_tx, shutdown_rx) = async_channel::bounded(1);
        let (event_ready, event_wait) = async_channel::bounded(1);
        SessionClient {
            bin_path,
            ble_backend: ble_backend.to_string(),
            state_dir,
            child: async_lock::Mutex::new(None),
            shutdown_tx,
            shutdown_rx,
            transport_alive: Arc::new(AtomicBool::new(false)),
            generation: AtomicU64::new(0),
            event_ready,
            event_wait,
            next_id: AtomicU64::new(1),
            sock_nonce: AtomicU64::new(1),
            events: Arc::new(Mutex::new(VecDeque::new())),
            presence_active: AtomicBool::new(false),
            handshake_timeout: Duration::from_secs(5),
            frame_timeout: Duration::from_secs(30),
        }
    }

    /// Kills and forgets any live tesla-session child. Called when
    /// `SetConfig` changes the VIN/key/timeouts a running session was
    /// spawned with - those are only read once, at spawn time (see `run`),
    /// so a stale session must not survive a config change.
    ///
    /// `run()` holds `child`'s lock for the full duration of a BLE op (up
    /// to the connect+command+10s envelope, so potentially minutes) - a
    /// `SetConfig`/`GenerateKey` call invoking this concurrently blocks on
    /// that same lock until the in-flight command finishes, not just until
    /// the child is idle.
    pub(crate) fn invalidate(&self) {
        let mut guard = self.child.lock_blocking();
        self.presence_active.store(false, Ordering::SeqCst);
        if let Some(handle) = guard.take() {
            Self::kill(handle);
        }
        self.generation.fetch_add(1, Ordering::SeqCst);
        // Drop joins the old reader, so it cannot enqueue stale events later.
        self.clear_events();
    }

    pub(crate) fn is_alive(&self) -> bool {
        self.transport_alive.load(Ordering::SeqCst)
    }

    /// Permanent shutdown. Closing this channel wakes every waiter, including
    /// accept, hello, write and response waits, without acquiring `child`.
    pub(crate) fn cancel(&self) {
        self.shutdown_tx.close();
        self.presence_active.store(false, Ordering::SeqCst);
    }

    pub(crate) fn generation(&self) -> u64 {
        self.generation.load(Ordering::SeqCst)
    }

    async fn cancellable<T>(
        &self,
        work: impl std::future::Future<Output = Result<T, SessionError>>,
        timeout: Duration,
    ) -> Result<T, SessionError> {
        if self.shutdown_rx.is_closed() {
            return Err(SessionError::Cancelled);
        }
        future::race(
            work,
            future::race(
                async {
                    let _ = self.shutdown_rx.recv().await;
                    Err(SessionError::Cancelled)
                },
                async {
                    Timer::after(timeout).await;
                    Err(SessionError::Timeout)
                },
            ),
        )
        .await
    }

    pub(crate) fn is_presence_active(&self) -> bool {
        self.presence_active.load(Ordering::SeqCst) && self.is_alive()
    }

    /// The BLE transport backend this client was constructed with ("hci" or
    /// "bluez"). `Core` consults it so the hci-only one-shot fallback
    /// (`run_binary("tesla-control", ...)`) is suppressed while a bluez
    /// session is in use - spawning raw HCI code would bring down the very
    /// adapter connections (e.g. a soundbar) that bluez mode exists to keep.
    pub(crate) fn ble_backend(&self) -> &str {
        &self.ble_backend
    }

    fn sock_path(&self) -> PathBuf {
        self.state_dir.join(format!(
            "tesla-session-{}-{}.sock",
            std::process::id(),
            self.sock_nonce.fetch_add(1, Ordering::Relaxed)
        ))
    }

    /// Binds the private parent socket. Split out of `spawn` so tests can
    /// drive the accept/handshake half against a scripted mock peer.
    pub(crate) fn bind_listener(&self) -> Result<(UnixListener, PathBuf), SessionError> {
        let sock_path = self.sock_path();
        // Best-effort stale cleanup (previous crash between bind and
        // unlink). The name is unique per spawn, so a leftover can only be
        // ours.
        let _ = std::fs::remove_file(&sock_path);
        // Restrict the state dir so only our UID can reach the socket during
        // the bind→accept→unlink window (a same-UID process could otherwise
        // dial in and spoof hello/responses).
        let _ = std::fs::create_dir_all(&self.state_dir);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ =
                std::fs::set_permissions(&self.state_dir, std::fs::Permissions::from_mode(0o700));
        }
        let listener = UnixListener::bind(&sock_path).map_err(SessionError::Spawn)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&sock_path, std::fs::Permissions::from_mode(0o600));
        }
        Ok((listener, sock_path))
    }

    async fn spawn(
        &self,
        vin: &str,
        key_file: &str,
        connect_timeout_sec: i32,
        command_timeout_sec: i32,
    ) -> Result<ChildHandle, SessionError> {
        let (listener, sock_path) = self.bind_listener()?;
        let _cleanup = SocketPathGuard(sock_path.clone());
        // this is made unsafe by libc::prctl
        // Wrapped at birth: every later drop path (including `?`
        // early-outs and panics) reaps the child, not just the explicit
        // kills below.
        let child = KillOnDrop(
            Command::new(&self.bin_path)
                .arg("-vin")
                .arg(vin)
                .arg("-key-file")
                .arg(key_file)
                .arg("-ble-backend")
                .arg(&self.ble_backend)
                .arg("-connect-timeout")
                .arg(format!("{connect_timeout_sec}s"))
                .arg("-command-timeout")
                .arg(format!("{command_timeout_sec}s"))
                .arg("-idle-timeout")
                .arg(format!("{IDLE_TIMEOUT_SEC}s"))
                .arg("-socket-path")
                .arg(&sock_path)
                // Inherited, not discarded: tesla-session's own startup
                // failures (bad flags, an unexpected panic) should land in
                // the app's own journal tag.
                .stderr(Stdio::inherit())
                .spawn()
                .map_err(SessionError::Spawn)?,
        );
        self.accept_and_handshake_async(&listener, Some(child), sock_path)
            .await
    }

    /// Accepts the single child connection and runs the hello handshake.
    /// `child` is `None` only in tests driving a mock peer (nothing to
    /// reap on failure); production always passes `Some`.
    #[cfg(test)]
    pub(crate) fn accept_and_handshake(
        &self,
        listener: &UnixListener,
        child: Option<KillOnDrop>,
        sock_path: PathBuf,
    ) -> Result<ChildHandle, SessionError> {
        async_io::block_on(self.accept_and_handshake_async(listener, child, sock_path))
    }

    async fn accept_and_handshake_async(
        &self,
        listener: &UnixListener,
        child: Option<KillOnDrop>,
        sock_path: PathBuf,
    ) -> Result<ChildHandle, SessionError> {
        let _cleanup = SocketPathGuard(sock_path.clone());
        // Non-blocking accept loop with a deadline: a child that never dials
        // can't hang spawn past the handshake deadline, and unlike the old
        // thread+recv_timeout helper this leaves no blocked accept thread
        // behind on timeout.
        let listener = Async::new(listener.try_clone().map_err(SessionError::Spawn)?)
            .map_err(SessionError::Spawn)?;
        let (stream, _) = self
            .cancellable(
                async { listener.accept().await.map_err(SessionError::Spawn) },
                self.handshake_timeout,
            )
            .await?;
        // Unlink right after accept: from here on no new peer can dial in,
        // so frames can only come from our child.
        let _ = std::fs::remove_file(&sock_path);

        // Hello handshake: the first frame must be a
        // versioned hello, otherwise this child speaks a different
        // protocol and must not survive. The handshake reads through the
        // SAME BufReader the reader task will own afterwards: a fresh
        // reader per phase would buffer ahead and silently drop frames
        // already read (e.g. presence events sent right after hello).
        // Accept and hello each have an interruptible handshake deadline.
        let write = Async::new(stream.get_ref().try_clone().map_err(SessionError::Spawn)?)
            .map_err(SessionError::Spawn)?;
        let mut reader = AsyncBufReader::new(stream);
        let mut line = String::new();
        let hello_ok = match self
            .cancellable(
                async {
                    read_frame(&mut reader, &mut line)
                        .await
                        .map_err(SessionError::Spawn)
                },
                self.handshake_timeout,
            )
            .await
        {
            Ok(_) => matches!(
                serde_json::from_str::<Frame>(line.trim_end()),
                Ok(Frame::Hello { v }) if v == PROTOCOL_VERSION
            ),
            Err(SessionError::Cancelled) => return Err(SessionError::Cancelled),
            Err(_) => false,
        };
        // The frame deadline governs the established connection from here.
        if !hello_ok {
            return Err(SessionError::Handshake(format!(
                "bad hello (want {{\"type\":\"hello\",\"v\":{PROTOCOL_VERSION}}}): {}",
                line.trim_end()
            )));
        }
        // `child` is None only in tests driving a mock peer (nothing to
        // reap); production always passes the spawned process. Either way
        // it moves into the handle untouched.
        let events = Arc::clone(&self.events);
        let alive = Arc::clone(&self.transport_alive);
        alive.store(true, Ordering::SeqCst);
        let cancelled = Arc::new(AtomicBool::new(false));
        let generation = self.generation.fetch_add(1, Ordering::SeqCst) + 1;
        let (rx, reader_task) = spawn_reader(
            reader,
            events,
            Arc::clone(&alive),
            Arc::clone(&cancelled),
            self.frame_timeout,
            generation,
            self.event_ready.clone(),
            self.shutdown_rx.clone(),
        );
        Ok(ChildHandle {
            child,
            stream: write,
            rx,
            sock_path,
            alive,
            cancelled,
            reader_task: Some(reader_task),
        })
    }

    /// Kills the given handle outright rather than dropping it - a live
    /// Child's Drop impl does not send a signal, so an abandoned handle
    /// would otherwise leak a running tesla-session (and its BLE session)
    /// for every timeout/error, not just close our end of the socket. The
    /// socket path was already unlinked at accept; remove it again in
    /// case accept never got that far (spawn failure paths).
    fn kill(handle: ChildHandle) {
        // Drop first shuts down/joins the reader, then reaps the process.
        drop(handle);
    }

    fn transport_failed(&self) {
        self.presence_active.store(false, Ordering::SeqCst);
        let mut queue = self
            .events
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        queue.clear();
        queue.push_back(SessionEvent {
            kind: "presence_stopped".to_string(),
            vin: String::new(),
            time: String::new(),
            error: "tesla-session transport failed".to_string(),
            error_code: String::new(),
            generation: self.generation(),
        });
        let _ = self.event_ready.try_send(());
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn run(
        &self,
        cmd: &str,
        args: &[String],
        vin: &str,
        key_file: &str,
        connect_timeout_sec: i32,
        command_timeout_sec: i32,
        timeout: Duration,
    ) -> Result<RunOutcome, SessionError> {
        async_io::block_on(self.run_async(
            cmd,
            args,
            vin,
            key_file,
            connect_timeout_sec,
            command_timeout_sec,
            timeout,
        ))
    }

    #[allow(clippy::too_many_arguments, clippy::too_many_lines)]
    pub(crate) async fn run_async(
        &self,
        cmd: &str,
        args: &[String],
        vin: &str,
        key_file: &str,
        connect_timeout_sec: i32,
        command_timeout_sec: i32,
        timeout: Duration,
    ) -> Result<RunOutcome, SessionError> {
        let slot = self
            .cancellable(async { Ok(self.child.lock().await) }, timeout)
            .await?;
        let mut guard = InFlight {
            slot,
            client: self,
            complete: false,
        };
        if guard
            .as_ref()
            .is_some_and(|handle| !handle.alive.load(Ordering::SeqCst))
        {
            Self::kill(guard.take().unwrap());
            self.transport_failed();
        }
        if guard.is_none() {
            *guard = Some(
                self.spawn(vin, key_file, connect_timeout_sec, command_timeout_sec)
                    .await?,
            );
        }
        let handle = guard.as_mut().unwrap();

        let id = self.next_id.fetch_add(1, Ordering::Relaxed).to_string();
        let mut line = serde_json::to_string(&Request {
            kind: "request",
            id: &id,
            cmd,
            args,
        })
        .expect("Request only contains strings; cannot fail to encode");
        line.push('\n');

        // Writes and response waits share a command deadline and can both be
        // interrupted without acquiring the single-flight mutex.
        let deadline = std::time::Instant::now() + timeout;
        if let Err(error) = self
            .cancellable(
                async {
                    handle
                        .stream
                        .write_all(line.as_bytes())
                        .await
                        .map_err(|_| SessionError::BrokenPipe)
                },
                timeout,
            )
            .await
        {
            let handle = guard.take().unwrap();
            Self::kill(handle);
            // The dead child's queued presence events must not replay as if
            // fresh once a new child is spawned.
            self.transport_failed();
            return Err(error);
        }

        // The reader task has already diverted Event/Heartbeat frames
        // into `events`/oblivion; this channel contains only responses (or
        // malformed lines that must fail the child), so a single timed
        // recv is enough.
        match self
            .cancellable(
                async { handle.rx.recv().await.map_err(|_| SessionError::BrokenPipe) },
                deadline.saturating_duration_since(std::time::Instant::now()),
            )
            .await
        {
            Ok(raw) => {
                let frame: Frame = match serde_json::from_str(&raw) {
                    Ok(l) => l,
                    Err(e) => {
                        let handle = guard.take().unwrap();
                        Self::kill(handle);
                        self.transport_failed();
                        return Err(SessionError::Decode(e));
                    }
                };
                let resp = match frame {
                    Frame::Event(_) | Frame::Heartbeat { .. } | Frame::Hello { .. } => {
                        // Reader diverts non-response frames; reaching here
                        // means a reader regression. No unreachable!(): this
                        // is reachable from C across FFI where a panic is UB.
                        // Treat as a fatal protocol violation like IdMismatch.
                        let handle = guard.take().unwrap();
                        Self::kill(handle);
                        self.transport_failed();
                        return Err(SessionError::IdMismatch);
                    }
                    Frame::Response(resp) => resp,
                };
                if resp.id != id {
                    let handle = guard.take().unwrap();
                    Self::kill(handle);
                    self.transport_failed();
                    return Err(SessionError::IdMismatch);
                }
                if resp.ok && (cmd == "presence-start" || cmd == "presence-stop") {
                    self.presence_active
                        .store(cmd == "presence-start", Ordering::SeqCst);
                }
                guard.complete = true;
                Ok(RunOutcome {
                    ok: resp.ok,
                    stdout: resp.stdout,
                    stderr: resp.stderr,
                    exit_code: resp.exit_code,
                })
            }
            Err(error) => {
                // Reader died (EOF, malformed frame, wedged child): the
                // transport is broken, not slow. Report BrokenPipe so the UI
                // can distinguish "car slow" (Timeout, retry) from "session
                // dead" (reconnect).
                let handle = guard.take().unwrap();
                Self::kill(handle);
                self.transport_failed();
                Err(error)
            }
        }
    }

    /// Generates (or, without `force`, re-prints the existing - see
    /// dispatchKeygen in tesla-session) a P256 keypair through the session's
    /// `keygen` request. No BLE is involved. The public key comes back PEM-
    /// encoded on `RunOutcome::stdout`; the private key is written to
    /// `key_file` by tesla-session itself. Pure-crypto, so there's no radio
    /// touching here and no backend concerns - the point is to stop exec'ing
    /// the privileged tesla-keygen binary.
    pub(crate) fn keygen(
        &self,
        force: bool,
        key_file: &str,
        vin: &str,
        connect_timeout_sec: i32,
        command_timeout_sec: i32,
        timeout: Duration,
    ) -> Result<RunOutcome, SessionError> {
        let args: Vec<String> = if force {
            vec!["-f".to_string()]
        } else {
            Vec::new()
        };
        self.run(
            "keygen",
            &args,
            vin,
            key_file,
            connect_timeout_sec,
            command_timeout_sec,
            timeout,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn start_presence(
        &self,
        vin: &str,
        key_file: &str,
        connect_timeout_sec: i32,
        command_timeout_sec: i32,
        timeout: Duration,
    ) -> Result<RunOutcome, SessionError> {
        self.run(
            "presence-start",
            &[],
            vin,
            key_file,
            connect_timeout_sec,
            command_timeout_sec,
            timeout,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn stop_presence(
        &self,
        vin: &str,
        key_file: &str,
        connect_timeout_sec: i32,
        command_timeout_sec: i32,
        timeout: Duration,
    ) -> Result<RunOutcome, SessionError> {
        self.run(
            "presence-stop",
            &[],
            vin,
            key_file,
            connect_timeout_sec,
            command_timeout_sec,
            timeout,
        )
    }

    pub(crate) fn poll_event(&self) -> Option<SessionEvent> {
        self.events
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .pop_front()
    }

    pub(crate) async fn wait_event(&self) {
        let _ = self.event_wait.recv().await;
    }

    pub(crate) fn clear_events(&self) {
        self.events
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clear();
        while self.event_wait.try_recv().is_ok() {}
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    #[test]
    fn frames_are_bounded_even_without_a_newline() {
        for payload in [vec![b'x'; MAX_FRAME_BYTES + 1], b"truncated".to_vec()] {
            let mut reader = futures_lite::io::Cursor::new(payload);
            let mut line = String::new();
            let error = async_io::block_on(read_frame(&mut reader, &mut line)).unwrap_err();
            assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
            assert!(line.len() <= MAX_FRAME_BYTES + 1);
        }
    }

    #[test]
    fn frame_limit_preserves_following_buffered_frames() {
        let mut payload = vec![b'x'; MAX_FRAME_BYTES - 1];
        payload.extend_from_slice(b"\nnext\n");
        let mut reader = futures_lite::io::Cursor::new(payload);
        let mut line = String::new();
        assert_eq!(
            async_io::block_on(read_frame(&mut reader, &mut line)).unwrap(),
            MAX_FRAME_BYTES
        );
        line.clear();
        assert_eq!(
            async_io::block_on(read_frame(&mut reader, &mut line)).unwrap(),
            5
        );
        assert_eq!(line, "next\n");
    }

    #[test]
    fn oversized_hello_rejects_the_peer() {
        let dir = tempfile::tempdir().unwrap();
        let client = test_client(dir.path());
        let (listener, path) = client.bind_listener().unwrap();
        let peer = thread::spawn({
            let path = path.clone();
            move || {
                let mut stream = UnixStream::connect(path).unwrap();
                let _ = stream.write_all(&vec![b'x'; MAX_FRAME_BYTES + 1]);
            }
        });
        assert!(matches!(
            client.accept_and_handshake(&listener, None, path.clone()),
            Err(SessionError::Handshake(_))
        ));
        peer.join().unwrap();
        assert!(!path.exists());
    }

    #[test]
    fn oversized_established_frame_stops_the_session() {
        let dir = tempfile::tempdir().unwrap();
        let mut greeting = hello_ok();
        greeting.push(format!(
            "{{\"type\":\"event\",\"kind\":\"{}\"}}",
            "x".repeat(MAX_FRAME_BYTES)
        ));
        let (client, _, peer) = accept_mock(dir.path(), greeting, vec![]);
        let ready = async_io::block_on(future::race(
            async {
                client.wait_event().await;
                true
            },
            async {
                Timer::after(Duration::from_secs(2)).await;
                false
            },
        ));
        let alive = client.is_alive();
        let event = client.poll_event();
        client.invalidate();
        peer.join().unwrap();
        assert!(ready, "reader did not report oversized established frame");
        assert!(!alive);
        assert_eq!(event.unwrap().kind, "presence_stopped");
    }

    fn test_client(dir: &std::path::Path) -> SessionClient {
        SessionClient::new(
            PathBuf::from("/nonexistent/tesla-session"),
            "bluez",
            dir.to_path_buf(),
        )
    }

    #[test]
    fn completed_work_survives_simultaneous_shutdown() {
        let dir = tempfile::tempdir().unwrap();
        let client = test_client(dir.path());
        let result = future::block_on(client.cancellable(
            async {
                // Completion wins the race, even if shutdown becomes visible
                // during this final poll before the result is returned.
                client.cancel();
                Ok(42)
            },
            Duration::from_secs(1),
        ));
        assert!(matches!(result, Ok(42)));
    }

    fn tmp_state_dir(tag: &str) -> tempfile::TempDir {
        tempfile::Builder::new()
            .prefix(&format!("electric-eel-sessiontest-{tag}-"))
            .tempdir()
            .unwrap()
    }

    /// Scripted mock peer: dials `sock_path`, sends `greet` lines, then
    /// answers each request line with the next `replies` entry (`{ID}` is
    /// replaced with the request's id). Closes when the script is
    /// exhausted or the socket breaks. Returns received request lines.
    fn mock_peer(
        sock_path: PathBuf,
        greet: Vec<String>,
        replies: Vec<String>,
    ) -> (mpsc::Receiver<String>, thread::JoinHandle<()>) {
        let (tx, rx) = mpsc::channel();
        let handle = thread::spawn(move || {
            let stream = UnixStream::connect(&sock_path).expect("mock peer dial");
            let mut w = stream.try_clone().expect("mock clone");
            for line in greet {
                writeln!(w, "{line}").expect("mock greet");
            }
            let mut reader = BufReader::new(stream);
            let mut replies = replies.into_iter();
            let mut line = String::new();
            loop {
                line.clear();
                match reader.read_line(&mut line) {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {
                        let raw = line.trim_end().to_string();
                        let id = serde_json::from_str::<serde_json::Value>(&raw)
                            .ok()
                            .and_then(|v| v.get("id")?.as_str().map(str::to_string))
                            .unwrap_or_default();
                        let _ = tx.send(raw);
                        match replies.next() {
                            Some(r) => {
                                let _ = writeln!(w, "{}", r.replace("{ID}", &id));
                            }
                            None => break,
                        }
                    }
                }
            }
        });
        (rx, handle)
    }

    fn hello_ok() -> Vec<String> {
        vec![format!(
            "{{\"type\":\"hello\",\"v\":{},\"ble_backend\":\"bluez\"}}",
            PROTOCOL_VERSION
        )]
    }

    fn ok_response() -> String {
        "{\"type\":\"response\",\"id\":\"{ID}\",\"ok\":true,\"stdout\":\"done\",\"stderr\":\"\",\"exit_code\":0}"
            .to_string()
    }

    /// Accepts a mock peer into a fresh client, bypassing process spawn:
    /// pre-binds the listener, starts the peer, and runs the real
    /// accept+handshake. Returns the client (with live child slot) and the
    /// peer's received-request channel.
    pub(crate) fn accept_mock(
        dir: &std::path::Path,
        greet: Vec<String>,
        replies: Vec<String>,
    ) -> (
        SessionClient,
        mpsc::Receiver<String>,
        thread::JoinHandle<()>,
    ) {
        let client = test_client(dir);
        let (got, peer) = attach_mock(&client, greet, replies);
        (client, got, peer)
    }

    pub(crate) fn attach_mock(
        client: &SessionClient,
        greet: Vec<String>,
        replies: Vec<String>,
    ) -> (mpsc::Receiver<String>, thread::JoinHandle<()>) {
        let (listener, path) = client.bind_listener().expect("bind");
        let (got, peer) = mock_peer(path.clone(), greet, replies);
        let handle = client
            .accept_and_handshake(&listener, None, path)
            .expect("handshake");
        client.child.lock_blocking().replace(handle);
        (got, peer)
    }

    /// A peer that emits presence during a command but withholds its reply.
    /// EOF is the shutdown acknowledgement; no sleeps or radio hardware.
    pub(crate) fn accept_blocked_mock(
        dir: &std::path::Path,
    ) -> (
        SessionClient,
        mpsc::Receiver<String>,
        thread::JoinHandle<()>,
    ) {
        let client = test_client(dir);
        let (listener, path) = client.bind_listener().unwrap();
        let (tx, rx) = mpsc::channel();
        let peer = thread::spawn({
            let path = path.clone();
            move || {
                let stream = UnixStream::connect(path).unwrap();
                let mut writer = stream.try_clone().unwrap();
                writeln!(writer, "{}", hello_ok()[0]).unwrap();
                let mut reader = BufReader::new(stream);
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                tx.send(line).unwrap();
                let _ = writer.write_all(b"{\"type\":\"event\",\"kind\":\"presence_near\",\"vin\":\"V\",\"time\":\"t\"}\n");
                let mut next = String::new();
                assert_eq!(
                    reader.read_line(&mut next).unwrap_or(0),
                    0,
                    "queued work must not execute after cancellation"
                );
            }
        });
        let handle = client.accept_and_handshake(&listener, None, path).unwrap();
        client.child.lock_blocking().replace(handle);
        (client, rx, peer)
    }

    #[test]
    fn cancellation_interrupts_response_wait_without_the_operation_lock() {
        let dir = tmp_state_dir("cancel-response");
        let (client, got, peer) = accept_blocked_mock(dir.path());
        let client = Arc::new(client);
        let (tx, rx) = mpsc::channel();
        let worker = thread::spawn({
            let client = Arc::clone(&client);
            move || {
                tx.send(client.run("lock", &[], "VIN", "/key", 60, 60, Duration::from_secs(130)))
                    .unwrap();
            }
        });
        got.recv_timeout(Duration::from_secs(2)).unwrap();
        let start = std::time::Instant::now();
        client.cancel();
        assert!(matches!(
            rx.recv_timeout(Duration::from_secs(2)).unwrap(),
            Err(SessionError::Cancelled)
        ));
        assert!(start.elapsed() < Duration::from_millis(500));
        worker.join().unwrap();
        peer.join().unwrap();
        assert!(!client.is_alive());
        assert!(client.child.lock_blocking().is_none());
        assert!(matches!(
            client.run("ping", &[], "VIN", "/key", 1, 1, Duration::from_secs(1)),
            Err(SessionError::Cancelled)
        ));
    }

    #[test]
    fn dropping_an_async_request_retires_the_outstanding_response() {
        let dir = tmp_state_dir("drop-future");
        let (client, got, peer) = accept_blocked_mock(dir.path());
        let mut request = Box::pin(client.run_async(
            "lock",
            &[],
            "VIN",
            "/key",
            60,
            60,
            Duration::from_secs(130),
        ));
        assert!(async_io::block_on(future::poll_once(request.as_mut())).is_none());
        got.recv_timeout(Duration::from_secs(2)).unwrap();
        drop(request);
        peer.join().unwrap();
        assert!(!client.is_alive());
        assert!(client.child.lock_blocking().is_none());
    }

    #[test]
    fn cancellation_wakes_mutex_waiters_before_the_owner_releases_its_permit() {
        let dir = tmp_state_dir("cancel-lock-wait");
        let (client, got, peer) = accept_blocked_mock(dir.path());
        let mut owner = Box::pin(client.run_async(
            "lock",
            &[],
            "VIN",
            "/key",
            60,
            60,
            Duration::from_secs(130),
        ));
        assert!(async_io::block_on(future::poll_once(owner.as_mut())).is_none());
        got.recv_timeout(Duration::from_secs(2)).unwrap();
        let mut waiter = Box::pin(client.run_async(
            "ping",
            &[],
            "VIN",
            "/key",
            60,
            60,
            Duration::from_secs(130),
        ));
        assert!(async_io::block_on(future::poll_once(waiter.as_mut())).is_none());
        client.cancel();
        assert!(matches!(
            async_io::block_on(waiter),
            Err(SessionError::Cancelled)
        ));
        assert!(
            client.child.try_lock().is_none(),
            "the owner's permit must still be held"
        );
        drop(owner);
        peer.join().unwrap();
    }

    #[test]
    fn cancellation_interrupts_accept_and_reaps_the_spawned_child() {
        let dir = tmp_state_dir("cancel-accept");
        let client = test_client(dir.path());
        let (listener, path) = client.bind_listener().unwrap();
        let child = KillOnDrop(Command::new("sleep").arg("60").spawn().unwrap());
        let pid = child.id();
        let mut accept =
            Box::pin(client.accept_and_handshake_async(&listener, Some(child), path.clone()));
        assert!(async_io::block_on(future::poll_once(accept.as_mut())).is_none());
        client.cancel();
        assert!(matches!(
            async_io::block_on(accept),
            Err(SessionError::Cancelled)
        ));
        assert!(!path.exists());
        assert!(!std::path::Path::new(&format!("/proc/{pid}")).exists());
    }

    #[test]
    fn cancellation_interrupts_a_peer_that_never_sends_hello() {
        let dir = tmp_state_dir("cancel-hello");
        let client = test_client(dir.path());
        let (listener, path) = client.bind_listener().unwrap();
        let peer = UnixStream::connect(&path).unwrap();
        let mut hello = Box::pin(client.accept_and_handshake_async(&listener, None, path.clone()));
        assert!(async_io::block_on(future::poll_once(hello.as_mut())).is_none());
        client.cancel();
        assert!(matches!(
            async_io::block_on(hello),
            Err(SessionError::Cancelled)
        ));
        assert_eq!(
            BufReader::new(peer).read_line(&mut String::new()).unwrap(),
            0
        );
        assert!(!path.exists());
    }

    #[test]
    fn cancellation_interrupts_a_saturated_socket_write() {
        let dir = tmp_state_dir("cancel-write");
        let client = test_client(dir.path());
        let (listener, path) = client.bind_listener().unwrap();
        let mut peer = UnixStream::connect(&path).unwrap();
        writeln!(peer, "{}", hello_ok()[0]).unwrap();
        let handle = client.accept_and_handshake(&listener, None, path).unwrap();
        client.child.lock_blocking().replace(handle);
        // Larger than a Unix socket's send buffer; the peer never reads.
        let args = vec!["x".repeat(8 * 1024 * 1024)];
        let mut write = Box::pin(client.run_async(
            "ping",
            &args,
            "VIN",
            "/key",
            60,
            60,
            Duration::from_secs(130),
        ));
        assert!(async_io::block_on(future::poll_once(write.as_mut())).is_none());
        client.cancel();
        let start = std::time::Instant::now();
        assert!(matches!(
            async_io::block_on(write),
            Err(SessionError::Cancelled)
        ));
        assert!(start.elapsed() < Duration::from_millis(500));
        assert!(!client.is_alive());
    }

    #[test]
    fn test_request_response_roundtrip_over_uds() {
        let dir = tmp_state_dir("roundtrip");
        let (client, got, peer) = accept_mock(dir.path(), hello_ok(), vec![ok_response()]);
        let outcome = client
            .run("lock", &[], "VIN", "/key", 5, 5, Duration::from_secs(5))
            .expect("run");
        assert!(outcome.ok);
        assert_eq!(outcome.stdout, "done");
        // The request line itself is tagged and carries the echoed id.
        let req_line = got.recv_timeout(Duration::from_secs(5)).unwrap();
        let req: serde_json::Value = serde_json::from_str(&req_line).unwrap();
        assert_eq!(req["type"], "request");
        assert_eq!(req["cmd"], "lock");
        drop(peer);
    }

    #[test]
    fn test_handshake_rejects_wrong_version() {
        let dir = tmp_state_dir("badversion");
        let client = test_client(dir.path());
        let (listener, path) = client.bind_listener().expect("bind");
        let (_got, peer) = mock_peer(
            path.clone(),
            vec!["{\"type\":\"hello\",\"v\":999,\"ble_backend\":\"bluez\"}".to_string()],
            vec![],
        );
        match client.accept_and_handshake(&listener, None, path.clone()) {
            Err(SessionError::Handshake(msg)) => assert!(msg.contains("bad hello")),
            Err(e) => panic!("expected Handshake error, got {e:?}"),
            Ok(_) => panic!("expected Handshake error, got Ok"),
        }
        assert!(
            !path.exists(),
            "socket path must not linger after a failed handshake"
        );
        drop(peer);
    }

    #[test]
    fn test_events_and_heartbeats_dont_confuse_run() {
        let dir = tmp_state_dir("demux");
        let event = "{\"type\":\"event\",\"kind\":\"presence_near\",\"vin\":\"V\",\"time\":\"t\"}"
            .to_string();
        let heartbeat = "{\"type\":\"heartbeat\",\"unix\":123}".to_string();
        // Unsolicited frames go in greet (sent on connect, before any
        // request); the reply to run()'s request comes from replies.
        let mut greet = hello_ok();
        greet.push(event);
        greet.push(heartbeat);
        let (client, _got, peer) = accept_mock(dir.path(), greet, vec![ok_response()]);
        let outcome = client
            .run("ping", &[], "VIN", "/key", 5, 5, Duration::from_secs(5))
            .expect("run");
        assert!(outcome.ok);
        let ev = client.poll_event().expect("event diverted to queue");
        assert_eq!(ev.kind, "presence_near");
        assert_eq!(ev.vin, "V");
        assert!(client.poll_event().is_none());
        drop(peer);
    }

    #[test]
    fn test_id_mismatch_is_fatal() {
        let dir = tmp_state_dir("mismatch");
        let wrong = "{\"type\":\"response\",\"id\":\"nope\",\"ok\":true,\"stdout\":\"\",\"stderr\":\"\",\"exit_code\":0}"
            .to_string();
        let (client, _got, peer) = accept_mock(dir.path(), hello_ok(), vec![wrong]);
        match client.run("lock", &[], "VIN", "/key", 5, 5, Duration::from_secs(5)) {
            Err(SessionError::IdMismatch) => {}
            other => panic!("expected IdMismatch, got {other:?}"),
        }
        drop(peer);
    }

    #[test]
    fn test_silent_child_trips_watchdog_not_command_timeout() {
        let dir = tmp_state_dir("watchdog");
        // Peer hellos, then never speaks again: no responses, no
        // heartbeats. No replies scripted, so it just idles on read.
        let mut client = test_client(dir.path());
        client.handshake_timeout = Duration::from_secs(2);
        client.frame_timeout = Duration::from_millis(200);
        let (listener, path) = client.bind_listener().expect("bind");
        let (_got, peer) = mock_peer(path.clone(), hello_ok(), vec![]);
        let handle = client
            .accept_and_handshake(&listener, None, path)
            .expect("handshake");
        client.child.lock_blocking().replace(handle);
        let start = std::time::Instant::now();
        // Command deadline is 30s; the watchdog must fail this in ~200ms.
        // A silent child (no frames at all) kills the reader, so the run
        // observes a dead transport (BrokenPipe), not a slow one (Timeout):
        // both prove the frame watchdog fired instead of the 30s deadline.
        match client.run("lock", &[], "VIN", "/key", 5, 5, Duration::from_secs(30)) {
            Err(SessionError::BrokenPipe | SessionError::Timeout) => {}
            other => panic!("expected watchdog BrokenPipe/Timeout, got {other:?}"),
        }
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "watchdog must beat the command timeout"
        );
        drop(peer);
    }

    #[test]
    fn test_frame_tagging_is_strict() {
        // Response shape without a tag is rejected (never sniffed).
        assert!(serde_json::from_str::<Frame>(
            r#"{"id":"5","ok":true,"stdout":"done","stderr":"","exit_code":0}"#
        )
        .is_err());
        // Event shape without a tag is rejected too.
        assert!(serde_json::from_str::<Frame>(
            r#"{"kind":"presence_error","vin":"V","time":"t","error":"radio"}"#
        )
        .is_err());
        // A response mislabeled as an event is rejected outright (the
        // tag selects the variant, whose required fields are then
        // missing) — mislabeled frames are never misrouted.
        assert!(serde_json::from_str::<Frame>(
            r#"{"type":"event","id":"5","ok":true,"stdout":"","stderr":"","exit_code":0}"#,
        )
        .is_err());
    }

    #[test]
    fn test_spawn_failure_is_reported_not_panicked() {
        let dir = tmp_state_dir("spawnfail");
        let client = test_client(dir.path());
        let result = client.run(
            "lock",
            &[],
            "5YJ3E1EA0PF000000",
            "/nonexistent/key.pem",
            5,
            5,
            Duration::from_secs(1),
        );
        // Bind succeeds (tmpdir), spawn of the bogus binary fails.
        assert!(matches!(result, Err(SessionError::Spawn(_))));
    }

    #[test]
    fn review_spawn_failure_removes_bound_socket() {
        let dir = tmp_state_dir("spawncleanup");
        let client = test_client(dir.path());
        let path = client.sock_path();
        let result = client.run("ping", &[], "VIN", "/key", 5, 5, Duration::from_secs(1));
        assert!(matches!(result, Err(SessionError::Spawn(_))));
        assert!(
            !path.exists(),
            "failed spawn leaked socket {}",
            path.display()
        );
    }

    #[test]
    fn review_idle_child_disconnect_surfaces_presence_stop() {
        let dir = tmp_state_dir("idledeath");
        let client = test_client(dir.path());
        let (listener, path) = client.bind_listener().unwrap();
        let peer = thread::spawn({
            let path = path.clone();
            move || {
                let mut stream = UnixStream::connect(path).unwrap();
                writeln!(stream, "{}", hello_ok()[0]).unwrap();
                // Model a child exiting while no user command is in flight.
            }
        });
        let handle = client.accept_and_handshake(&listener, None, path).unwrap();
        peer.join().unwrap();
        // Synchronize with reader termination, rather than sleeping.
        assert!(matches!(
            async_io::block_on(client.cancellable(
                async { handle.rx.recv().await.map_err(|_| SessionError::BrokenPipe) },
                Duration::from_secs(2)
            )),
            Err(SessionError::BrokenPipe)
        ));
        let event = client.poll_event();
        assert!(
            event.is_some_and(|e| e.kind == "presence_stopped"),
            "an idle transport loss must reach Core's phone-key restart path"
        );
    }

    #[test]
    fn test_invalidate_without_a_running_child() {
        let dir = tmp_state_dir("invalidate");
        let client = test_client(dir.path());
        client.invalidate();
        client.invalidate();
    }

    #[test]
    fn test_killed_child_does_not_leave_stale_events() {
        // The reader diverts events into a shared queue that survives child
        // restarts. run() kills the child on IdMismatch/timeout but never
        // clears that queue, so a presence_near from the dead child is polled
        // after the restart as if it were fresh.
        let dir = tmp_state_dir("stale");
        let wrong = "{\"type\":\"response\",\"id\":\"nope\",\"ok\":true,\"stdout\":\"\",\"stderr\":\"\",\"exit_code\":0}"
            .to_string();
        let (client, _got, peer) = accept_mock(dir.path(), hello_ok(), vec![wrong]);
        client.events.lock().unwrap().push_back(SessionEvent {
            kind: "presence_near".to_string(),
            vin: "V".to_string(),
            time: "t".to_string(),
            error: String::new(),
            error_code: String::new(),
            generation: client.generation(),
        });
        let _ = client.run("lock", &[], "VIN", "/key", 5, 5, Duration::from_secs(5));
        assert!(
            client
                .poll_event()
                .is_some_and(|event| event.kind == "presence_stopped"),
            "stale event from the killed child must be dropped, not replayed"
        );
        drop(peer);
    }
}
