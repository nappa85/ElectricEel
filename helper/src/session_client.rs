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
//! queue (capacity 64, oldest dropped) so a UI that never polls cannot
//! wedge the child. The C ABI polls that queue to surface phone-key state
//! to QML.
//!
//! Requests are never sent concurrently: the only caller is `Core::run`
//! (and its siblings), itself serialized by `ble_sem`, so a single
//! in-flight request at a time is a precondition here, not something this
//! module enforces on its own.

use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex};
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
    /// the reader thread; writes happen only under the client's `child`
    /// lock (single-flight by contract), so no write mutex is needed.
    stream: UnixStream,
    rx: mpsc::Receiver<String>,
    /// Bound socket path, removed on kill. Unlinked right after accept so
    /// the accept window is the only time the path exists.
    sock_path: PathBuf,
}

impl Drop for ChildHandle {
    /// Backstop for the socket path: `KillOnDrop` already reaps the
    /// process above; unlinking here guarantees no stale path survives
    /// any drop path either. Both halves ignore errors (nothing sensible
    /// to do in Drop, and double unlink/kill is harmless).
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.sock_path);
    }
}

/// Spawns a background thread that owns the socket's read half for the
/// child lifetime, forwarding complete response lines onto a channel and
/// diverting events/heartbeats. This is what makes `recv_timeout` in
/// `SessionClient::run` a real read-with-timeout despite sockets not
/// natively supporting one, and what enforces the frame deadline: any
/// gap longer than `frame_timeout` (no response, event, or heartbeat)
/// kills the child as wedged rather than hanging the caller. EOF or a
/// read error just ends the thread; the channel closing is how `run`
/// finds out.
fn spawn_reader(
    reader: BufReader<UnixStream>,
    events: Arc<Mutex<VecDeque<SessionEvent>>>,
) -> mpsc::Receiver<String> {
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let mut reader = reader;
        let mut line = String::new();
        loop {
            line.clear();
            match reader.read_line(&mut line) {
                Ok(0) => break, // EOF: child exited or connection reset
                Ok(_) => {
                    let raw = line.trim_end().to_string();
                    match serde_json::from_str::<Frame>(&raw) {
                        Ok(Frame::Response(_)) => {
                            if tx.send(raw).is_err() {
                                break;
                            }
                        }
                        Ok(Frame::Event(event)) => {
                            let mut queue = events.lock().unwrap();
                            if queue.len() == 64 {
                                queue.pop_front();
                            }
                            queue.push_back(event);
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
                // Child and reaps it. (SO_RCVTIMEO expiry surfaces as
                // WouldBlock on Unix; TimedOut is matched too so a
                // platform quirk can't silently disable the watchdog.)
                Err(e)
                    if e.kind() == std::io::ErrorKind::WouldBlock
                        || e.kind() == std::io::ErrorKind::TimedOut =>
                {
                    break;
                }
                Err(_) => break,
            }
        }
    });
    rx
}

pub struct SessionClient {
    bin_path: PathBuf,
    ble_backend: String,
    state_dir: PathBuf,
    child: Mutex<Option<ChildHandle>>,
    next_id: AtomicU64,
    events: Arc<Mutex<VecDeque<SessionEvent>>>,
    /// Bound on socket accept + hello handshake.
    handshake_timeout: Duration,
    /// Deadline per frame on an established connection. Must exceed the
    /// child's heartbeat interval by several multiples.
    frame_timeout: Duration,
}

impl SessionClient {
    #[must_use]
    pub fn new(bin_path: PathBuf, ble_backend: &str, state_dir: PathBuf) -> Self {
        SessionClient {
            bin_path,
            ble_backend: ble_backend.to_string(),
            state_dir,
            child: Mutex::new(None),
            next_id: AtomicU64::new(1),
            events: Arc::new(Mutex::new(VecDeque::new())),
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
        if let Ok(mut guard) = self.child.lock() {
            if let Some(handle) = guard.take() {
                Self::kill(handle);
            }
        }
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
            self.next_id.load(Ordering::Relaxed)
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
        let listener = UnixListener::bind(&sock_path).map_err(SessionError::Spawn)?;
        Ok((listener, sock_path))
    }

    fn spawn(
        &self,
        vin: &str,
        key_file: &str,
        connect_timeout_sec: i32,
        command_timeout_sec: i32,
    ) -> Result<ChildHandle, SessionError> {
        let (listener, sock_path) = self.bind_listener()?;
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
        self.accept_and_handshake(listener, Some(child), sock_path)
    }

    /// Accepts the single child connection and runs the hello handshake.
    /// `child` is `None` only in tests driving a mock peer (nothing to
    /// reap on failure); production always passes `Some`.
    pub(crate) fn accept_and_handshake(
        &self,
        listener: UnixListener,
        child: Option<KillOnDrop>,
        sock_path: PathBuf,
    ) -> Result<ChildHandle, SessionError> {
        let mut child = child;
        // Accept runs on a thread so a child that never dials can't hang
        // spawn past the handshake deadline.
        let accepted = {
            let (tx, rx) = mpsc::channel();
            thread::spawn(move || {
                let _ = tx.send(listener.accept());
            });
            let Ok(outcome) = rx.recv_timeout(self.handshake_timeout) else {
                if let Some(mut c) = child.take() {
                    let _ = c.kill();
                    let _ = c.wait();
                }
                let _ = std::fs::remove_file(&sock_path);
                return Err(SessionError::Handshake(
                    "tesla-session did not connect in time".to_string(),
                ));
            };
            outcome
        };
        let stream = match accepted {
            Ok((s, _)) => s,
            Err(e) => {
                if let Some(mut c) = child.take() {
                    let _ = c.kill();
                    let _ = c.wait();
                }
                let _ = std::fs::remove_file(&sock_path);
                return Err(SessionError::Handshake(format!("accept failed: {e}")));
            }
        };
        // Unlink right after accept: from here on no new peer can dial in,
        // so frames can only come from our child.
        let _ = std::fs::remove_file(&sock_path);

        // Hello handshake, synchronously: the first frame must be a
        // versioned hello, otherwise this child speaks a different
        // protocol and must not survive. The handshake reads through the
        // SAME BufReader the reader thread will own afterwards: a fresh
        // reader per phase would buffer ahead and silently drop frames
        // already read (e.g. presence events sent right after hello).
        // Timeouts are socket options, shared by the clones below.
        stream
            .set_read_timeout(Some(self.handshake_timeout))
            .map_err(SessionError::Spawn)?;
        let mut reader = BufReader::new(stream.try_clone().map_err(SessionError::Spawn)?);
        let mut line = String::new();
        let hello_ok = match reader.read_line(&mut line) {
            Ok(_) => matches!(
                serde_json::from_str::<Frame>(line.trim_end()),
                Ok(Frame::Hello { v }) if v == PROTOCOL_VERSION
            ),
            Err(_) => false,
        };
        // The frame deadline governs the established connection from here.
        stream
            .set_read_timeout(Some(self.frame_timeout))
            .map_err(SessionError::Spawn)?;
        if !hello_ok {
            if let Some(mut c) = child.take() {
                let _ = c.kill();
                let _ = c.wait();
            }
            return Err(SessionError::Handshake(format!(
                "bad hello (want {{\"type\":\"hello\",\"v\":{PROTOCOL_VERSION}}}): {}",
                line.trim_end()
            )));
        }
        // `child` is None only in tests driving a mock peer (nothing to
        // reap); production always passes the spawned process. Either way
        // it moves into the handle untouched.
        let events = Arc::clone(&self.events);
        let rx = spawn_reader(reader, events);
        Ok(ChildHandle {
            child,
            stream,
            rx,
            sock_path,
        })
    }

    /// Kills the given handle outright rather than dropping it - a live
    /// Child's Drop impl does not send a signal, so an abandoned handle
    /// would otherwise leak a running tesla-session (and its BLE session)
    /// for every timeout/error, not just close our end of the socket. The
    /// socket path was already unlinked at accept; remove it again in
    /// case accept never got that far (spawn failure paths).
    fn kill(handle: ChildHandle) {
        let mut handle = handle;
        // Drop the process first (KillOnDrop SIGKILLs and reaps promptly),
        // then the rest (ChildHandle::drop unlinks the socket path).
        // Kept as a named function so call sites read as an action: every
        // error path below funnels the doomed child through here instead
        // of relying on scope-end Drops scattered across the function.
        drop(handle.child.take());
        drop(handle);
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
        let mut guard = self.child.lock().unwrap();
        if guard.is_none() {
            *guard = Some(self.spawn(vin, key_file, connect_timeout_sec, command_timeout_sec)?);
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

        if handle.stream.write_all(line.as_bytes()).is_err() {
            let handle = guard.take().unwrap();
            Self::kill(handle);
            return Err(SessionError::BrokenPipe);
        }

        // The reader thread has already diverted Event/Heartbeat frames
        // into `events`/oblivion; this channel contains only responses (or
        // malformed lines that must fail the child), so a single timed
        // recv is enough.
        match handle.rx.recv_timeout(timeout) {
            Ok(raw) => {
                let frame: Frame = match serde_json::from_str(&raw) {
                    Ok(l) => l,
                    Err(e) => {
                        let handle = guard.take().unwrap();
                        Self::kill(handle);
                        return Err(SessionError::Decode(e));
                    }
                };
                let resp = match frame {
                    Frame::Event(_) | Frame::Heartbeat { .. } | Frame::Hello { .. } => {
                        unreachable!("reader diverts non-response frames")
                    }
                    Frame::Response(resp) => resp,
                };
                if resp.id != id {
                    let handle = guard.take().unwrap();
                    Self::kill(handle);
                    return Err(SessionError::IdMismatch);
                }
                Ok(RunOutcome {
                    ok: resp.ok,
                    stdout: resp.stdout,
                    stderr: resp.stderr,
                    exit_code: resp.exit_code,
                })
            }
            Err(RecvTimeoutError::Timeout | RecvTimeoutError::Disconnected) => {
                let handle = guard.take().unwrap();
                Self::kill(handle);
                Err(SessionError::Timeout)
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
        self.events.lock().unwrap().pop_front()
    }

    pub(crate) fn clear_events(&self) {
        self.events.lock().unwrap().clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_client(dir: &std::path::Path) -> SessionClient {
        SessionClient::new(
            PathBuf::from("/nonexistent/tesla-session"),
            "bluez",
            dir.to_path_buf(),
        )
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
    fn accept_mock(
        dir: &std::path::Path,
        greet: Vec<String>,
        replies: Vec<String>,
    ) -> (
        SessionClient,
        mpsc::Receiver<String>,
        thread::JoinHandle<()>,
    ) {
        let client = test_client(dir);
        let (listener, path) = client.bind_listener().expect("bind");
        let (got, peer) = mock_peer(path.clone(), greet, replies);
        let handle = client
            .accept_and_handshake(listener, None, path)
            .expect("handshake");
        client.child.lock().unwrap().replace(handle);
        (client, got, peer)
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
        match client.accept_and_handshake(listener, None, path.clone()) {
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
            .accept_and_handshake(listener, None, path)
            .expect("handshake");
        client.child.lock().unwrap().replace(handle);
        let start = std::time::Instant::now();
        // Command deadline is 30s; the watchdog must fail this in ~200ms.
        match client.run("lock", &[], "VIN", "/key", 5, 5, Duration::from_secs(30)) {
            Err(SessionError::Timeout) => {}
            other => panic!("expected Timeout, got {other:?}"),
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
    fn test_invalidate_without_a_running_child() {
        let dir = tmp_state_dir("invalidate");
        let client = test_client(dir.path());
        client.invalidate();
        client.invalidate();
    }
}
