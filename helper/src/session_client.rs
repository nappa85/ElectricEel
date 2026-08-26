//! Client for the persistent tesla-session companion process (see
//! helper/session/), which keeps one *vehicle.Vehicle BLE session alive
//! across many commands instead of paying a full connect+StartSession
//! handshake per command, the way spawning a fresh `tesla-control` does.
//!
//! Talks newline-delimited JSON over the child's stdin/stdout, matching
//! `Run()`'s own (ok, stdout, stderr, `exit_code`) shape so callers can't tell
//! the difference except by latency. Any failure at this layer (spawn,
//! broken pipe, timeout, a response that doesn't decode or doesn't match
//! the request it's replying to) drops the whole child and reports an
//! error - `Helper::run()` in helper.rs falls back to the one-shot
//! `run_binary("tesla-control", ...)` path on any such error, so a bug
//! here degrades to today's behavior rather than to a broken app.
//!
//! tesla-session's presence-maintenance loop (presence-start/presence-stop)
//! also writes unsolicited `{"kind": ..., ...}` event lines onto the same
//! stdout stream, outside any request/response pairing - see
//! `dispatchPresenceStart` and `presenceLoop` in `helper/session/main.go`.
//! The stdout reader demultiplexes those events into a bounded side queue so
//! they cannot accumulate in front of command responses. The C ABI polls that
//! queue to surface phone-key state to QML.
//!
//! Requests are never sent concurrently: the only caller is `Helper::run`,
//! itself serialized by `ble_sem`, so a single in-flight request at a time
//! is a precondition here, not something this module enforces on its own.

use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use serde::{Deserialize, Serialize};

/// How long a BLE session may sit idle before tesla-session tears it down
/// and lets the vehicle/adapter go back to sleep. Not user-configurable
/// (see `KNOWN_ISSUES.md`: shipped as an invisible optimization, not a
/// Settings toggle).
pub(crate) const IDLE_TIMEOUT_SEC: u32 = 90;

#[derive(Serialize)]
struct Request<'a> {
    id: &'a str,
    cmd: &'a str,
    args: &'a [String],
}

#[derive(Deserialize)]
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

/// Either shape a line on tesla-session's stdout can take. `run()` waits
/// specifically for a `Response`, silently skipping any `Event` lines it
/// reads along the way.
#[derive(Deserialize)]
#[serde(untagged)]
enum Line {
    Response(Response),
    Event(SessionEvent),
}

pub(crate) struct RunOutcome {
    pub ok: bool,
    pub stdout: String,
    pub stderr: String,
    pub exit_code: i32,
}

#[derive(Debug)]
pub(crate) enum SessionError {
    Spawn(std::io::Error),
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
            SessionError::BrokenPipe => write!(f, "tesla-session pipe closed"),
            SessionError::Timeout => write!(f, "tesla-session did not respond in time"),
            SessionError::Decode(e) => write!(f, "malformed tesla-session response: {e}"),
            SessionError::IdMismatch => write!(f, "tesla-session response id mismatch"),
        }
    }
}

struct ChildHandle {
    child: Child,
    stdin: ChildStdin,
    rx: mpsc::Receiver<String>,
}

/// Spawns a background thread that owns the child's stdout for its whole
/// lifetime, forwarding complete lines onto a channel - this is what makes
/// `recv_timeout` in `SessionClient::run` a real read-with-timeout despite
/// `std::process::ChildStdout` not natively supporting one. EOF or a read
/// error just ends the thread; the channel closing is how `run` finds out.
fn spawn_reader(
    stdout: std::process::ChildStdout,
    events: Arc<Mutex<VecDeque<SessionEvent>>>,
) -> mpsc::Receiver<String> {
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let mut reader = BufReader::new(stdout);
        let mut line = String::new();
        loop {
            line.clear();
            match reader.read_line(&mut line) {
                Ok(0) | Err(_) => break,
                Ok(_) => {
                    let raw = line.trim_end().to_string();
                    if let Ok(Line::Event(event)) = serde_json::from_str::<Line>(&raw) {
                        let mut queue = events.lock().unwrap();
                        if queue.len() == 64 {
                            queue.pop_front();
                        }
                        queue.push_back(event);
                    } else if tx.send(raw).is_err() {
                        break;
                    }
                }
            }
        }
    });
    rx
}

pub struct SessionClient {
    bin_path: PathBuf,
    ble_backend: String,
    child: Mutex<Option<ChildHandle>>,
    next_id: AtomicU64,
    events: Arc<Mutex<VecDeque<SessionEvent>>>,
}

impl SessionClient {
    #[must_use]
    pub fn new(bin_path: PathBuf, ble_backend: &str) -> Self {
        SessionClient {
            bin_path,
            ble_backend: ble_backend.to_string(),
            child: Mutex::new(None),
            next_id: AtomicU64::new(1),
            events: Arc::new(Mutex::new(VecDeque::new())),
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
    /// the child is idle. Harmless today (the feature this guards is off
    /// by default and unverified on real hardware - see `KNOWN_ISSUES.md`),
    /// but worth knowing before relying on `SetConfig` feeling instant
    /// while a command is running.
    pub(crate) fn invalidate(&self) {
        if let Ok(mut guard) = self.child.lock() {
            if let Some(mut handle) = guard.take() {
                let _ = handle.child.kill();
                let _ = handle.child.wait();
            }
        }
    }

    /// The BLE transport backend this client was constructed with ("hci" or
    /// "bluez"). helper.rs consults it so the hci-only one-shot fallback
    /// (`run_binary("tesla-control", ...)`) is suppressed while a bluez
    /// session is in use - spawning raw HCI code would bring down the very
    /// adapter connections (e.g. a soundbar) that bluez mode exists to keep.
    pub(crate) fn ble_backend(&self) -> &str {
        &self.ble_backend
    }

    fn spawn(
        &self,
        vin: &str,
        key_file: &str,
        connect_timeout_sec: i32,
        command_timeout_sec: i32,
    ) -> Result<ChildHandle, SessionError> {
        let log_dir = crate::keylog::log_dir();
        let mut command = Command::new(&self.bin_path);
        command
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
            .arg("-log-dir")
            .arg(&log_dir)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            // Inherited, not discarded: tesla-session's own startup
            // failures (bad flags, an unexpected panic) should land in
            // the app's own journal tag, same place run_binary's captured
            // tesla-control stderr effectively ends up via Run()'s reply.
            .stderr(Stdio::inherit());
        // Linux: SIGTERM the child if the app dies, so a killed UI cannot
        // leak tesla-session holding the BLE radio. prctl is Linux-only.
        #[cfg(target_os = "linux")]
        {
            use std::os::unix::process::CommandExt;
            // SAFETY: pre_exec runs in the child after fork, before exec.
            unsafe {
                command.pre_exec(|| {
                    libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM as usize, 0, 0, 0);
                    Ok(())
                });
            }
        }
        let mut child = command.spawn().map_err(SessionError::Spawn)?;

        let stdin = child.stdin.take().expect("piped stdin");
        let stdout = child.stdout.take().expect("piped stdout");
        let rx = spawn_reader(stdout, Arc::clone(&self.events));
        Ok(ChildHandle { child, stdin, rx })
    }

    /// Kills the given handle. On Unix, sends SIGTERM first so tesla-session's
    /// signal handler can stop discovery and disconnect GATT cleanly before
    /// exit - an immediate SIGKILL (`Child::kill`) leaves org.bluez discovery
    /// running and has been observed to destabilize Sailfish's bluetoothd.
    /// Waits up to 5s for exit, then SIGKILL if still alive.
    fn kill(mut handle: ChildHandle) {
        #[cfg(unix)]
        {
            use std::time::Duration;
            const SIGTERM: i32 = 15;
            let pid = handle.child.id();
            // SAFETY: kill(2) with SIGTERM is the standard graceful-shutdown
            // signal; pid comes from our own child process.
            let term_sent = unsafe { libc::kill(pid.cast_signed(), SIGTERM) == 0 };
            if term_sent {
                let deadline = Duration::from_secs(5);
                let start = std::time::Instant::now();
                while start.elapsed() < deadline {
                    match handle.child.try_wait() {
                        Ok(Some(_)) => return,
                        Ok(None) => std::thread::sleep(Duration::from_millis(100)),
                        Err(_) => break,
                    }
                }
            }
        }
        let _ = handle.child.kill();
        let _ = handle.child.wait();
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
        let mut line = serde_json::to_string(&Request { id: &id, cmd, args })
            .expect("Request only contains strings; cannot fail to encode");
        line.push('\n');

        if handle.stdin.write_all(line.as_bytes()).is_err() || handle.stdin.flush().is_err() {
            let handle = guard.take().unwrap();
            Self::kill(handle);
            return Err(SessionError::BrokenPipe);
        }

        // The reader thread has already diverted Event lines into `events`;
        // this channel contains only responses (or malformed lines that must
        // fail the child), so a single timed recv is enough.
        match handle.rx.recv_timeout(timeout) {
            Ok(raw) => {
                let line: Line = match serde_json::from_str(&raw) {
                    Ok(l) => l,
                    Err(e) => {
                        let handle = guard.take().unwrap();
                        Self::kill(handle);
                        return Err(SessionError::Decode(e));
                    }
                };
                let resp = match line {
                    Line::Event(_) => unreachable!("reader diverts events"),
                    Line::Response(resp) => resp,
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
    /// the privileged tesla-keygen binary (helper/ removal, Phases 3-4).
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

    // No real tesla-session binary (and no BLE adapter) in this
    // environment - what's verifiable here is that a missing/non-
    // executable binary is a clean Spawn error, not a panic or hang, and
    // that invalidate() on a client that never spawned anything is a
    // harmless no-op. The write/read/timeout/id-matching logic above is
    // exercised for real by helper/session's own tests on the child
    // process side; the two halves only meet on a real device.
    #[test]
    fn test_spawn_failure_is_reported_not_panicked() {
        let client = SessionClient::new(PathBuf::from("/nonexistent/tesla-session"), "hci");
        let result = client.run(
            "lock",
            &[],
            "5YJ3E1EA0PF000000",
            "/nonexistent/key.pem",
            5,
            5,
            Duration::from_secs(1),
        );
        assert!(matches!(result, Err(SessionError::Spawn(_))));
    }

    #[test]
    fn test_invalidate_without_a_running_child() {
        let client = SessionClient::new(PathBuf::from("/nonexistent/tesla-session"), "hci");
        client.invalidate();
        client.invalidate();
    }

    // These exercise Line's untagged parsing directly - the piece run()'s
    // skip-events loop depends on to tell a presence-loop event apart from
    // the response it's actually waiting for, without needing a real
    // tesla-session child to produce the two shapes on a pipe.
    #[test]
    fn test_line_parses_response_shape() {
        let raw = r#"{"id":"5","ok":true,"stdout":"done","stderr":"","exit_code":0}"#;
        match serde_json::from_str::<Line>(raw).expect("valid Response line") {
            Line::Response(r) => {
                assert_eq!(r.id, "5");
                assert!(r.ok);
                assert_eq!(r.stdout, "done");
            }
            Line::Event(_) => panic!("Response-shaped line parsed as Event"),
        }
    }

    #[test]
    fn test_line_parses_event_shape_without_response_fields() {
        let raw = r#"{"kind":"presence_error","vin":"5YJ3E1EA0PF000000","time":"2026-08-17T09:00:00Z","error":"radio"}"#;
        match serde_json::from_str::<Line>(raw).expect("valid Event line") {
            Line::Event(event) => {
                assert_eq!(event.kind, "presence_error");
                assert_eq!(event.vin, "5YJ3E1EA0PF000000");
                assert_eq!(event.error, "radio");
            }
            Line::Response(_) => panic!("Event-shaped line parsed as Response"),
        }
    }

    #[test]
    fn test_line_rejects_garbage() {
        assert!(serde_json::from_str::<Line>(r#"{"unexpected":"shape"}"#).is_err());
    }
}
