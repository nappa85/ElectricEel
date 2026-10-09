//! In-process control core for harbour-electric-eel (see `docs/architecture.md`
//! for why): everything `helper.rs`'s D-Bus daemon did, minus
//! the D-Bus surface, the caller authorization, and the system-bus
//! connection. The app links this as a staticlib and drives it through the C
//! ABI in `ffi.rs`; the daemon binary (`main.rs` + `helper.rs`, built only
//! with the `dbus` feature) wraps the same `Core` so both halves share one
//! orchestration rather than maintaining a second hand-copy.
//!
//! No zbus, no `authorize` - the caller is the app itself, which already
//! satisfied Sailjail/dbus policy before its first call. Config, key files
//! and the persistent `tesla-session` child behave exactly as they did for
//! the daemon.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::thread;
use std::time::{Duration, Instant};

use wait_timeout::ChildExt;

use crate::commands::{is_known_command, is_pin_command};
use crate::config::{Config, ConfigVersion, VinState};
use crate::error::{HelperError, OperationError};
use crate::session_client::{SessionClient, SessionEvent};
use crate::share::{parse_shared_text, Destination};

/// `GetConfig`'s return payload, in the same order the client (Qt's
/// `QDBusPendingReply`) and the zbus macro both destructure it:
/// (vin, model, `key_name`, `connect_timeout_sec`, `command_timeout_sec`, `has_key`,
/// `public_key_pem`). A transparent alias so the seven-field tuple - above
/// clippy's type-complexity comfort zone, and easy to mismatch by position
/// at the call sites - is spelled once.
pub(crate) type GetConfigReply = (String, String, String, i32, i32, bool, String);

/// Combined exit status of a bundled binary invocation.
pub(crate) struct RunOutcome {
    pub ok: bool,
    pub stdout: String,
    pub stderr: String,
    pub exit_code: i32,
}

pub struct Core {
    cfg: Mutex<Config>,
    /// Capacity-1 semaphore serializing Run/Pair through the single HCI
    /// adapter: a second simultaneous BLE command is rejected rather than
    /// spawning a second tesla-control that fights over the adapter.
    ble_sem: Mutex<()>,
    bin_dir: String,
    state_dir: String,
    /// `Some()` only when a persistent session is desired (the daemon only
    /// enables it with `ELECTRICEEL_PERSISTENT_SESSION`; the app always
    /// enables it) - None means `run()` behaves exactly as one-shot did,
    /// not "session client that always fails over".
    session: Option<SessionClient>,
    /// Serializes lifecycle/config/key mutations. Always acquired before cfg;
    /// a second start waits for the first caller's actual outcome.
    phone_key_gate: Mutex<()>,
    /// Whether the `BlueZ` proximity/authentication service is currently started.
    phone_key_started: AtomicBool,
    phone_key_enabled: AtomicBool,
    phone_key_retry_at: Mutex<Option<Instant>>,
}

impl Core {
    /// Builds the control core from on-disk state.
    ///
    /// # Errors
    ///
    /// Returns [`HelperError::SessionUnavailable`] when `config.json` can't be
    /// read from `state_dir` (e.g. the service started before the store was
    /// initialized).
    pub fn new(
        bin_dir: String,
        state_dir: String,
        session: Option<SessionClient>,
    ) -> Result<Core, HelperError> {
        let config_path = Path::new(&state_dir).join("config.json");
        let mut cfg = Config::load(&config_path).map_err(|e| {
            HelperError::SessionUnavailable(format!(
                "cannot read config {}: {e}",
                config_path.display()
            ))
        })?;
        // Only V0 lacked explicit enrollment state. Never override an explicit
        // unpaired state or rewrite a schema from a newer build.
        let mut dirty = false;
        if cfg.version == ConfigVersion::V0
            && cfg.vin_state == VinState::Unpaired
            && !cfg.vin.is_empty()
            && Self::key_files_in(&state_dir)
        {
            cfg.vin_state = VinState::Paired;
            dirty = true;
            crate::keylog::log("core", "migrated pre-phone-key enrollment state");
        }
        if cfg.version < ConfigVersion::CURRENT {
            cfg.version = ConfigVersion::CURRENT;
            dirty = true;
        }
        if dirty {
            if let Err(e) = cfg.save(&config_path) {
                eprintln!("Core: could not persist config migration: {e}");
            }
        }
        Ok(Core {
            cfg: Mutex::new(cfg),
            ble_sem: Mutex::new(()),
            bin_dir,
            state_dir,
            session,
            phone_key_gate: Mutex::new(()),
            phone_key_started: AtomicBool::new(false),
            phone_key_enabled: AtomicBool::new(false),
            phone_key_retry_at: Mutex::new(None),
        })
    }

    fn config_path(&self) -> PathBuf {
        Path::new(&self.state_dir).join("config.json")
    }

    pub(crate) fn private_key_path(&self) -> PathBuf {
        Path::new(&self.state_dir).join("private_key.pem")
    }

    pub(crate) fn public_key_path(&self) -> PathBuf {
        Path::new(&self.state_dir).join("public_key.pem")
    }

    fn key_files_in(state_dir: &str) -> bool {
        Path::new(state_dir).join("private_key.pem").is_file()
            && Path::new(state_dir).join("public_key.pem").is_file()
    }

    /// Builds the -ble/-vin/-key-file/... flags shared by every tesla-control
    /// invocation, from the persisted config.
    fn common_args_locked(&self, cfg: &Config) -> Result<Vec<String>, HelperError> {
        if cfg.vin.is_empty() {
            return Err(HelperError::NotConfigured(
                "VIN is not set; call SetConfig first".to_string(),
            ));
        }
        if std::fs::metadata(self.private_key_path()).is_err() {
            return Err(HelperError::NoKey(
                "no private key; call GenerateKey first".to_string(),
            ));
        }
        Ok(vec![
            "-ble".to_string(),
            "-keyring-type".to_string(),
            "file".to_string(),
            "-key-file".to_string(),
            self.private_key_path().to_string_lossy().into_owned(),
            "-key-name".to_string(),
            cfg.key_name.clone(),
            "-vin".to_string(),
            cfg.vin.clone(),
            "-connect-timeout".to_string(),
            format!("{}s", cfg.connect_timeout_sec),
            "-command-timeout".to_string(),
            format!("{}s", cfg.command_timeout_sec),
        ])
    }

    /// Executes a single command against the vehicle, either through the
    /// persistent session or (no session / session error) by spawning a
    /// one-shot binary. cmd must be one of the known subcommands.
    pub(crate) fn run(
        &self,
        cmd: &str,
        args: &[String],
    ) -> Result<(bool, String, String, i32), HelperError> {
        if !is_known_command(cmd) {
            return Err(HelperError::UnknownCommand(cmd.to_string()));
        }
        // Go's JSON dispatcher consumes positional strings, and the CLI
        // fallback stops parsing flags at the known command name. Negative
        // coordinates and end-only time intervals are legitimate arguments.
        validate_arg_ranges(cmd, args)?;

        let _permit = self
            .ble_sem
            .try_lock()
            .map_err(|_| HelperError::Busy("another BLE command is in progress".to_string()))?;

        // Snapshot config under the gate, then release the gate before BLE
        // I/O: holding phone_key_gate across a minute-long command would
        // block set_config/generate_key/invalidate (and get_config readers)
        // for the whole round-trip. The snapshot (VIN, key path, timeouts,
        // argv prefix) is immutable for this command; a concurrent config
        // change applies to the next command (snapshot isolation). Only
        // ble_sem is held across I/O (one BLE command at a time).
        let (common, timeout, vin, connect_timeout_sec, command_timeout_sec) = {
            let _gate = self
                .phone_key_gate
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let cfg = self
                .cfg
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let common = self.common_args_locked(&cfg)?;
            // i64 before the sum, not i32: an overflowing i32 sum here used
            // to be able to turn into a negative (already-cancelled)
            // deadline - a previously-fixed bug, preserved here. Clamped at
            // zero so a hand-edited negative timeout can never cast into a
            // near-infinite u64 deadline.
            let secs =
                (i64::from(cfg.connect_timeout_sec) + i64::from(cfg.command_timeout_sec) + 10)
                    .max(0)
                    .cast_unsigned();
            (
                common,
                Duration::from_secs(secs),
                cfg.vin.clone(),
                cfg.connect_timeout_sec,
                cfg.command_timeout_sec,
            )
        };

        let mut command_argv = common;
        command_argv.push(cmd.to_string());
        command_argv.extend(args.iter().cloned());

        // PINs are always redacted; schedule coordinates are location data
        // and get the same treatment (arg count only, never values).
        if is_pin_command(cmd)
            || cmd == "charging-schedule-add"
            || cmd == "precondition-schedule-add"
        {
            eprintln!("Core: run({cmd}, [{} redacted args])", args.len());
        } else {
            eprintln!("Core: run({cmd}, {args:?})");
        }

        let outcome = match &self.session {
            Some(session) => {
                let key_path = self.private_key_path().to_string_lossy().into_owned();
                match session.run(
                    cmd,
                    args,
                    &vin,
                    &key_path,
                    connect_timeout_sec,
                    command_timeout_sec,
                    timeout,
                ) {
                    Ok(o) => RunOutcome {
                        ok: o.ok,
                        stdout: o.stdout,
                        stderr: o.stderr,
                        exit_code: o.exit_code,
                    },
                    Err(e) if session.ble_backend() == "bluez" => {
                        // No fallback: run_binary would spawn a raw-HCI
                        // tesla-control, which takes exclusive adapter
                        // control and drops any other BLE connections (the
                        // whole reason bluez mode exists). Surface the
                        // session error instead - the user must fix the
                        // session, not silently regress to hci.
                        return Err(HelperError::SessionUnavailable(format!(
                            "bluez persistent session failed for {cmd}: {e}"
                        )));
                    }
                    Err(e) => {
                        eprintln!(
                            "Core: persistent session unavailable ({e}); falling back to one-shot tesla-control for {cmd}"
                        );
                        run_binary(&self.bin_dir, "tesla-control", &command_argv, timeout)
                    }
                }
            }
            None => run_binary(&self.bin_dir, "tesla-control", &command_argv, timeout),
        };

        Ok((
            outcome.ok,
            outcome.stdout,
            outcome.stderr,
            outcome.exit_code,
        ))
    }

    /// Creates a new local private key (file-backed - there is no Sailfish
    /// OS keyring backend) and returns its PEM-encoded public key.
    ///
    /// With a persistent session this routes through tesla-session's `keygen`
    /// request (pure crypto, no BLE) so the privileged tesla-keygen binary
    /// isn't exec'd at all. Without a session it falls back to exec'ing
    /// tesla-keygen, the pre-session behavior.
    pub(crate) fn generate_key(&self, force: bool) -> Result<String, OperationError> {
        let _gate = self
            .phone_key_gate
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        self.cfg
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .ensure_writable()
            .map_err(OperationError::Persist)?;
        let private_existed = self.private_key_path().is_file();
        let replacing = force || !private_existed;
        // Holds the config mutex for the whole call, same as the original -
        // GenerateKey doesn't read Config, but this still serializes
        // concurrent key generation against writing the same key files.
        // Read from this single guard below rather than locking again:
        // std::sync::Mutex isn't reentrant, so a second self.cfg.lock() on
        // this thread while _cfg is still held would deadlock forever -
        // exactly what happened here before this fix (Generate Key hanging
        // indefinitely on the QML side, since the worker thread never
        // returns to emit keyGenerated).
        let mut cfg = self
            .cfg
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);

        if replacing {
            // Persist the enrollment reset before a potentially destructive
            // key write. A failure must never leave a new key marked paired.
            let mut candidate = cfg.clone();
            candidate.vin_state = VinState::Unpaired;
            candidate
                .save(&self.config_path())
                .map_err(OperationError::Persist)?;
            self.stop_phone_key_locked(&cfg);
            *cfg = candidate;
            if let Some(session) = &self.session {
                session.invalidate();
            }
        }

        let key_path = self.private_key_path().to_string_lossy().into_owned();

        // Keygen needs no connection, but spawning the session child still
        // wants the configured VIN/timeouts for the -vin/-key-file/-timeout
        // flags - they're only read once, at spawn (see SessionClient::run).
        let vin = cfg.vin.clone();
        let connect_timeout_sec = cfg.connect_timeout_sec;
        let command_timeout_sec = cfg.command_timeout_sec;

        eprintln!("Core: generate_key(force={force})");

        let pubkey = match &self.session {
            None => {
                generate_key_one_shot(&self.bin_dir, &key_path, &self.public_key_path(), force)?
            }
            Some(session) => {
                // Never retry a possibly completed key rotation through a
                // different binary after a transport failure.
                match session.keygen(
                    force,
                    &key_path,
                    &vin,
                    connect_timeout_sec,
                    command_timeout_sec,
                    Duration::from_secs(15),
                ) {
                    Ok(outcome) if outcome.ok => {
                        // Persist the public half to the file Pair()
                        // reads from (and the app's pubkey location).
                        let pubkey = outcome.stdout.trim().to_string();
                        if let Err(e) = crate::config::write_atomic(
                            self.public_key_path().as_path(),
                            pubkey.as_bytes(),
                        ) {
                            return Err(OperationError::WritePubkey(e));
                        }
                        pubkey
                    }
                    Ok(outcome) => {
                        return Err(OperationError::KeygenFailed(
                            outcome.stderr.trim().to_string(),
                        ));
                    }
                    Err(e) => {
                        return Err(OperationError::KeygenFailed(e.to_string()));
                    }
                }
            }
        };

        if replacing {
            // A live persistent session (if any) loaded the private key into
            // memory at connect time - the file on disk just changed under it,
            // so it must reconnect rather than keep signing with a stale key.
            if let Some(session) = &self.session {
                session.invalidate();
            }
        }
        Ok(pubkey)
    }

    /// Enrolls the current public key with the vehicle via BLE, requiring
    /// physical NFC-card approval at the center console (matches the
    /// official app's "add key" flow).
    #[allow(clippy::too_many_lines)]
    pub(crate) fn pair(&self) -> Result<(bool, String, String), HelperError> {
        let Ok(_permit) = self.ble_sem.try_lock() else {
            return Err(HelperError::Busy(
                "another BLE command is in progress".to_string(),
            ));
        };
        let gate = self
            .phone_key_gate
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let session = self.session.as_ref().ok_or_else(|| {
            HelperError::SessionUnavailable(
                "pairing requires a persistent session to confirm NFC enrollment".to_string(),
            )
        })?;
        self.cfg
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .ensure_writable()
            .map_err(|e| HelperError::SessionUnavailable(e.to_string()))?;
        {
            let cfg = self
                .cfg
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            self.stop_phone_key_locked(&cfg);
        }
        let (vin, key_path, connect_timeout_sec, command_timeout_sec, timeout) = {
            let cfg = self
                .cfg
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            self.common_args_locked(&cfg)?;
            // Same envelope as Run() (connect + command + 10s), plus an
            // allowance that covers the physical NFC-card tap at the
            // center console this command waits for. The persistent
            // session holds the connection for up to 90s after transmission
            // while checking vehicle enrollment and authentication. The
            // deadline must include that approval window.
            // (A flat 30s used to be added, which under the 90s grace was
            // less than the real reply latency even at default timeouts:
            // 20+5+10+30 = 65s < 90s.) 95s comfortably tops the 90s grace.
            // Clamped at zero like run() so a negative timeout can never
            // cast into a near-infinite deadline.
            let secs =
                (i64::from(cfg.connect_timeout_sec) + i64::from(cfg.command_timeout_sec) + 10 + 95)
                    .max(0)
                    .cast_unsigned();
            (
                cfg.vin.clone(),
                self.private_key_path().to_string_lossy().into_owned(),
                cfg.connect_timeout_sec,
                cfg.command_timeout_sec,
                Duration::from_secs(secs),
            )
        };
        let pubkey_path = self.public_key_path();
        if std::fs::metadata(&pubkey_path).is_err() {
            return Ok((
                false,
                String::new(),
                "no public key on file; call GenerateKey first".to_string(),
            ));
        }

        // Form factor is what tells the vehicle this is a real phone key that
        // authorizes driving. A cloud_key is a Fleet/API key: it can send BLE
        // commands (lock/unlock/climate/...) but the car does NOT count it as a
        // drive-authorizing key, so the driver still has to tap the physical
        // NFC card to drive. Enrolling as a phone form factor (android_device)
        // makes the vehicle treat the connected session as a phone key, which
        // authorizes both unlock and drive - no NFC tap needed to drive.
        let pair_args = [
            pubkey_path.to_string_lossy().as_ref(),
            "owner",
            "android_device",
        ]
        .map(str::to_string);

        eprintln!("Core: pair()");
        // Only the persistent dispatcher confirms vehicle enrollment. A lost
        // response must not replay the request through an unverified fallback.
        let outcome = match session.run(
            "pair",
            &pair_args,
            &vin,
            &key_path,
            connect_timeout_sec,
            command_timeout_sec,
            timeout,
        ) {
            Ok(o) => RunOutcome {
                ok: o.ok,
                stdout: o.stdout,
                stderr: o.stderr,
                exit_code: o.exit_code,
            },
            Err(e) => {
                return Err(HelperError::SessionUnavailable(format!(
                    "persistent session failed during pairing: {e}"
                )));
            }
        };
        if !outcome.ok {
            return Ok((false, outcome.stdout, outcome.stderr.trim().to_string()));
        }
        {
            let mut cfg = self
                .cfg
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let mut candidate = cfg.clone();
            candidate.vin_state = VinState::Paired;
            candidate.save(&self.config_path()).map_err(|e| {
                HelperError::SessionUnavailable(format!(
                    "paired key but could not persist phone-key state: {e}"
                ))
            })?;
            *cfg = candidate;
        }
        drop(gate);
        if let Err(start_error) = self.start_phone_key() {
            return Ok((
                false,
                outcome.stdout,
                format!("paired, but automatic phone key could not start: {start_error}"),
            ));
        }
        Ok((true, outcome.stdout, String::new()))
    }

    /// Starts the background `BlueZ` proximity/authentication service when the
    /// current key is paired to the configured VIN. Idempotent.
    pub(crate) fn start_phone_key(&self) -> Result<(), OperationError> {
        let _gate = self
            .phone_key_gate
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if self.phone_key_started.load(Ordering::SeqCst)
            && self
                .session
                .as_ref()
                .is_some_and(SessionClient::is_presence_active)
        {
            return Ok(());
        }
        let was_started = self.phone_key_started.swap(false, Ordering::SeqCst);
        let (vin, key_path, connect_timeout_sec, command_timeout_sec, eligible) = {
            let cfg = self
                .cfg
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            (
                cfg.vin.clone(),
                self.private_key_path().to_string_lossy().into_owned(),
                cfg.connect_timeout_sec,
                cfg.command_timeout_sec,
                !cfg.vin.is_empty()
                    && cfg.vin_state == VinState::Paired
                    && self.private_key_path().is_file(),
            )
        };
        if !eligible {
            self.phone_key_enabled.store(false, Ordering::SeqCst);
            crate::keylog::log("core", "phone-key start refused: not paired");
            return Err(OperationError::NotPaired);
        }
        let Some(session) = &self.session else {
            crate::keylog::log("core", "phone-key start refused: no session client");
            return Err(OperationError::NoSession);
        };
        if session.ble_backend() != "bluez" {
            crate::keylog::log("core", "phone-key start refused: requires bluez");
            return Err(OperationError::RequiresBluez);
        }
        if was_started && !session.is_presence_active() {
            // Retire a dead/non-presence generation before starting scanning.
            // Its pending stopped event must not restart the replacement again.
            session.invalidate();
        }
        self.phone_key_enabled.store(true, Ordering::SeqCst);
        crate::keylog::log(
            "core",
            &format!("phone-key start vin={vin} connect={connect_timeout_sec}s"),
        );
        // Same envelope as run(): presence must also survive a slow adapter
        // (BLE connect + command + margin). A flat 10s guaranteed spurious
        // PresenceFailed on slow hardware and a 5s retry storm.
        let presence_secs = (i64::from(connect_timeout_sec) + i64::from(command_timeout_sec) + 10)
            .max(0)
            .cast_unsigned();
        match session.start_presence(
            &vin,
            &key_path,
            connect_timeout_sec,
            command_timeout_sec,
            Duration::from_secs(presence_secs),
        ) {
            Ok(outcome) if outcome.ok => {
                self.phone_key_started.store(true, Ordering::SeqCst);
                *self
                    .phone_key_retry_at
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
                crate::keylog::log("core", "phone-key presence started");
                Ok(())
            }
            Ok(outcome) => {
                self.phone_key_started.store(false, Ordering::SeqCst);
                self.schedule_phone_key_retry();
                crate::keylog::log(
                    "core",
                    &format!("phone-key start failed: {}", outcome.stderr.trim()),
                );
                Err(OperationError::PresenceFailed(
                    outcome.stderr.trim().to_string(),
                ))
            }
            Err(e) => {
                self.phone_key_started.store(false, Ordering::SeqCst);
                self.schedule_phone_key_retry();
                crate::keylog::log("core", &format!("phone-key start error: {e}"));
                Err(OperationError::PresenceFailed(e.to_string()))
            }
        }
    }

    /// Stops proximity mode and closes its live BLE connection. Idempotent.
    pub(crate) fn stop_phone_key(&self) {
        let _gate = self
            .phone_key_gate
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let cfg = self
            .cfg
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        self.stop_phone_key_locked(&cfg);
    }

    fn stop_phone_key_locked(&self, cfg: &Config) {
        self.phone_key_enabled.store(false, Ordering::SeqCst);
        *self
            .phone_key_retry_at
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
        if !self.phone_key_started.load(Ordering::SeqCst) {
            return;
        }
        crate::keylog::log("core", "phone-key stop");
        if let Some(session) = &self.session {
            let result = session.stop_presence(
                &cfg.vin,
                &self.private_key_path().to_string_lossy(),
                cfg.connect_timeout_sec,
                cfg.command_timeout_sec,
                Duration::from_secs(10),
            );
            if result.is_err() {
                session.invalidate();
            }
        }
        self.phone_key_started.store(false, Ordering::SeqCst);
    }

    pub(crate) fn phone_key_enabled(&self) -> bool {
        self.phone_key_enabled.load(Ordering::SeqCst)
    }

    fn schedule_phone_key_retry(&self) {
        *self
            .phone_key_retry_at
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) =
            Some(Instant::now() + Duration::from_secs(5));
    }

    pub(crate) fn poll_phone_key_event(&self) -> Option<SessionEvent> {
        let mut event = self.session.as_ref().and_then(SessionClient::poll_event);
        let retry_due = self
            .phone_key_retry_at
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .is_some_and(|deadline| Instant::now() >= deadline);
        if event.is_none() && retry_due && self.phone_key_enabled.load(Ordering::SeqCst) {
            event = Some(SessionEvent {
                kind: "presence_stopped".to_string(),
                vin: self.get_config().0,
                time: String::new(),
                error: String::new(),
            });
        }
        if let Some(event) = &event {
            crate::keylog::log(
                "core",
                &format!(
                    "event kind={} err={}",
                    event.kind,
                    if event.error.is_empty() {
                        "-"
                    } else {
                        &event.error
                    }
                ),
            );
        }
        if let Some(event) = &mut event {
            if event.vin.is_empty() {
                event.vin = self.get_config().0;
            }
        }
        if event.as_ref().is_some_and(|e| e.kind == "presence_stopped") {
            let gate = self
                .phone_key_gate
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            self.phone_key_started.store(false, Ordering::SeqCst);
            if !self.phone_key_enabled.load(Ordering::SeqCst) {
                return event;
            }
            if self
                .phone_key_retry_at
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .is_some_and(|deadline| Instant::now() < deadline)
            {
                return event;
            }
            if let Some(session) = &self.session {
                session.invalidate();
            }
            drop(gate);
            match self.start_phone_key() {
                Ok(()) => {
                    if let Some(event) = &mut event {
                        event.kind = "presence_restarted".to_string();
                        event.error.clear();
                    }
                }
                Err(error) => {
                    if let Some(event) = &mut event {
                        let msg = error.to_string();
                        if !msg.is_empty() {
                            // Preserve stopped for UI diagnostics. Mode remains
                            // enabled, so the CPU lease spans the bounded retry.
                            event.error = msg;
                        }
                    }
                }
            }
        }
        event
    }

    /// Called when the device resumes from system suspend (screen off / sleep).
    /// The `org.bluez` system-bus socket and any live GATT link are typically
    /// stale after the freezer - the Go child is still alive but its D-Bus
    /// connection will time out on the next command. Proactively kill the
    /// idle child so the next `run()` spawns a fresh one with a new bus
    /// connection instead of waiting for a full `connect_timeout+command_timeout`
    /// timeout. Idempotent and safe to call even when no child exists or no
    /// session is configured.
    pub(crate) fn handle_resume(&self) {
        let gate = self
            .phone_key_gate
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        crate::keylog::log("core", "resume from suspend - recycling BLE session");
        eprintln!("Core: handle_resume - device woke from suspend, recycling stale BLE session");
        // Allow `start_phone_key` to claim the flag again after we killed its
        // old presenceLoop. Without this a stale `true` would make the restart
        // a no-op.
        self.phone_key_started.store(false, Ordering::SeqCst);
        if let Some(session) = &self.session {
            session.clear_events();
            session.invalidate();
        }
        // Best-effort restart: if we were paired before suspend we want
        // presence scanning back immediately, not only after the next user
        // command recreates the child. Failures are just logged - the next
        // periodic `poll_phone_key_event` or explicit user action will retry
        // and `start_phone_key` already emits a descriptive error.
        drop(gate);
        let _ = self.start_phone_key();
    }

    pub(crate) fn set_config(
        &self,
        vin: &str,
        model: &str,
        key_name: &str,
        connect_timeout_sec: i32,
        command_timeout_sec: i32,
    ) -> Result<(), OperationError> {
        crate::config::validate_config(
            vin,
            model,
            key_name,
            connect_timeout_sec,
            command_timeout_sec,
        )?;

        let gate = self
            .phone_key_gate
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut cfg = self
            .cfg
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let vin_changed = cfg.vin != vin.trim();
        let mut candidate = cfg.clone();
        candidate.vin = vin.trim().to_string();
        candidate.model = model.trim().to_ascii_lowercase();
        candidate.key_name = key_name.trim().to_string();
        candidate.connect_timeout_sec = connect_timeout_sec;
        candidate.command_timeout_sec = command_timeout_sec;
        if vin_changed {
            candidate.vin_state = VinState::Unpaired;
        }
        if let Err(e) = candidate.save(&self.config_path()) {
            return Err(OperationError::Persist(e));
        }
        self.stop_phone_key_locked(&cfg);
        *cfg = candidate;
        eprintln!(
            "Core: set_config(vin={}, model={:?}, keyName={:?}, connectTimeout={}s, commandTimeout={}s)",
            cfg.vin, cfg.model, cfg.key_name, connect_timeout_sec, command_timeout_sec
        );
        // A live persistent session (if any) was spawned with the old
        // VIN/timeouts baked into its argv - drop it so the next run()
        // spawns a fresh one with the new config instead of silently
        // continuing to talk to the previous vehicle/timeout settings.
        if let Some(session) = &self.session {
            session.invalidate();
        }
        drop(cfg);
        drop(gate);
        if !vin_changed {
            let _ = self.start_phone_key();
        }
        Ok(())
    }

    /// Parses shared text without sending anything, so the Navigation page
    /// can show "will send coordinates" vs "will send address" before the
    /// user confirms. Returns (kind, value1, value2): ("gps", lat, lon) or
    /// ("address", text, ""). Pure CPU, no session/token needed.
    pub(crate) fn preview_destination(text: &str) -> Result<(String, String, String), HelperError> {
        match parse_shared_text(text) {
            Ok(Destination::LatLon { lat, lon }) => {
                Ok(("gps".to_string(), format!("{lat}"), format!("{lon}")))
            }
            Ok(Destination::Address(addr)) => Ok(("address".to_string(), addr, String::new())),
            Err(e) => Err(HelperError::InvalidArgument(e.to_string())),
        }
    }

    /// Sends a parsed destination to the car navigation over the live BLE
    /// session (signed field-53/field-21 actions — see
    /// `docs/navigation-share.md` §1). No network, no token, no
    /// Fleet API: the payload rides the same authenticated BLE connection
    /// as lock/unlock.
    ///
    /// Takes `ble_sem` like `run()`: this uses the radio, so it serializes
    /// against concurrent BLE commands.
    pub(crate) fn share_destination(
        &self,
        text: &str,
    ) -> Result<(bool, String, String), HelperError> {
        let dest =
            parse_shared_text(text).map_err(|e| HelperError::InvalidArgument(e.to_string()))?;
        let _permit = self
            .ble_sem
            .try_lock()
            .map_err(|_| HelperError::Busy("another BLE command is in progress".to_string()))?;
        // Snapshot isolation like run(): release the gate before BLE I/O.
        let (vin, key_path, connect_timeout_sec, command_timeout_sec, timeout) = {
            let _gate = self
                .phone_key_gate
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let cfg = self
                .cfg
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            // Same NoKey gate as run(): a missing private key must fail here
            // with a clear error, not as an obscure Go child failure.
            self.common_args_locked(&cfg)?;
            // Same envelope as run(): connect + command + 10s, clamped at zero.
            let secs =
                (i64::from(cfg.connect_timeout_sec) + i64::from(cfg.command_timeout_sec) + 10)
                    .max(0)
                    .cast_unsigned();
            (
                cfg.vin.clone(),
                self.private_key_path().to_string_lossy().into_owned(),
                cfg.connect_timeout_sec,
                cfg.command_timeout_sec,
                Duration::from_secs(secs),
            )
        };
        let Some(session) = &self.session else {
            return Err(HelperError::SessionUnavailable(
                "persistent BLE session is unavailable".to_string(),
            ));
        };
        let args: Vec<String> = match dest {
            Destination::LatLon { lat, lon } => {
                vec!["gps".to_string(), format!("{lat}"), format!("{lon}")]
            }
            Destination::Address(addr) => vec!["address".to_string(), addr],
        };
        eprintln!("Core: share_destination({} args)", args[0]);
        // VIN/key-file/timeouts are spawn-time argv; key-file is unused by
        // the navigate path but the spawn signature requires it.
        match session.run(
            "navigate",
            &args,
            &vin,
            &key_path,
            connect_timeout_sec,
            command_timeout_sec,
            timeout,
        ) {
            Ok(o) => Ok((o.ok, o.stdout, o.stderr)),
            Err(e) => Err(HelperError::SessionUnavailable(format!(
                "navigation share failed: {e}"
            ))),
        }
    }

    pub(crate) fn get_config(&self) -> GetConfigReply {
        // Snapshot config under the lock, then do filesystem I/O with no
        // lock held: get_config is called twice per presence poll, and
        // blocking writers behind two reads + two stats per second is a
        // needless stall.
        let (vin, model, key_name, connect_timeout_sec, command_timeout_sec) = {
            let cfg = self
                .cfg
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            (
                cfg.vin.clone(),
                cfg.model.clone(),
                cfg.key_name.clone(),
                cfg.connect_timeout_sec,
                cfg.command_timeout_sec,
            )
        };
        // A usable key needs both halves present and non-empty: public alone
        // (or an empty file left by a crashed keygen) must not report "Key
        // ready" and then fail every run() with NoKey.
        let pub_key = std::fs::read_to_string(self.public_key_path()).unwrap_or_default();
        let has_key = !pub_key.trim().is_empty()
            && self.private_key_path().is_file()
            && std::fs::metadata(self.private_key_path()).is_ok_and(|m| m.len() > 0);
        (
            vin,
            model,
            key_name,
            connect_timeout_sec,
            command_timeout_sec,
            has_key,
            pub_key,
        )
    }
}

impl Drop for Core {
    fn drop(&mut self) {
        // Never block in Drop: stop_phone_key() performs a bounded BLE RPC
        // that can stall for seconds and lock cfg/gate (deadlock if dropped
        // while the same thread holds them). Best-effort non-blocking
        // teardown only; the runtime already stops presence explicitly.
        self.phone_key_enabled.store(false, Ordering::SeqCst);
        *self
            .phone_key_retry_at
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
        self.phone_key_started.store(false, Ordering::SeqCst);
        if let Some(session) = &self.session {
            session.invalidate();
        }
    }
}
/// Generates a keypair by executing the bundled tesla-keygen binary - the
/// pre-session (and no-session) path for `GenerateKey`. Returns (ok,
/// `pubkey_text`); on failure the second element holds the trimmed stderr
/// instead. Kept as the fallback until Phase 4 removes the binary
/// dependency entirely.
fn generate_key_one_shot(
    bin_dir: &str,
    key_path: &str,
    pubkey_path: &std::path::Path,
    force: bool,
) -> Result<String, OperationError> {
    let mut args = vec![
        "-keyring-type".to_string(),
        "file".to_string(),
        "-key-file".to_string(),
        key_path.to_string(),
        "-output".to_string(),
        pubkey_path.to_string_lossy().into_owned(),
    ];
    if force {
        args.push("-f".to_string());
    }
    args.push("create".to_string());

    let outcome = run_binary(bin_dir, "tesla-keygen", &args, Duration::from_secs(15));
    if !outcome.ok {
        return Err(OperationError::KeygenFailed(
            outcome.stderr.trim().to_string(),
        ));
    }
    match std::fs::read_to_string(pubkey_path) {
        Ok(pub_key) => Ok(pub_key.trim().to_string()),
        Err(e) => Err(OperationError::KeygenFailed(e.to_string())),
    }
}

/// Rejects out-of-range numeric arguments for commands with a well-defined
/// value range, before anything reaches the wired session or the subprocess.
/// This is server-side defense-in-depth: the QML sliders in
/// `ArgumentDialog.qml` already bound these, but the JSON stdin path accepts
/// arbitrary args, and the vendored upstream handlers (`commands_vendor.go`)
/// do `Atoi` to `int32` with no range check, so a huge value would wrap or be
/// sent to the vehicle. Keep the per-command bounds here in sync with the
/// `min`/`max` in `app/qml/js/CommandCatalog.js`.
#[allow(clippy::too_many_lines)]
fn validate_arg_ranges(cmd: &str, args: &[String]) -> Result<(), HelperError> {
    fn invalid(cmd: &str, arg: &str) -> HelperError {
        HelperError::InvalidArgument(format!("{cmd} argument out of range or invalid: {arg}"))
    }
    fn parse_int_arg(arg: &str) -> Option<i64> {
        arg.trim().parse().ok()
    }
    fn parse_float_arg(arg: &str) -> Option<f64> {
        let v: f64 = arg.trim().parse().ok()?;
        v.is_finite().then_some(v)
    }
    // Strip an optional single-letter unit suffix (e.g. "21C" -> ("21", Some('C'))).
    fn strip_unit(arg: &str) -> (&str, Option<char>) {
        let t = arg.trim();
        match t.chars().last() {
            Some(c) if c.is_ascii_alphabetic() => (&t[..t.len() - c.len_utf8()], Some(c)),
            _ => (t, None),
        }
    }
    fn check_arity(cmd: &str, args: &[String], expected: usize) -> Result<(), HelperError> {
        if args.len() != expected {
            return Err(invalid(cmd, &args.join(" ")));
        }
        Ok(())
    }
    match cmd {
        "charging-set-limit" => {
            check_arity(cmd, args, 1)?;
            let arg = args.first().ok_or_else(|| {
                HelperError::InvalidArgument(format!("{cmd} requires a numeric argument"))
            })?;
            let value: i64 = arg.trim().parse().map_err(|_| {
                HelperError::InvalidArgument(format!("{cmd} argument must be an integer: {arg}"))
            })?;
            if !(50..=100).contains(&value) {
                return Err(HelperError::InvalidArgument(format!(
                    "{cmd} argument {value} out of range [50, 100]"
                )));
            }
            Ok(())
        }
        "charging-set-amps" => {
            check_arity(cmd, args, 1)?;
            let arg = args.first().ok_or_else(|| {
                HelperError::InvalidArgument(format!("{cmd} requires a numeric argument"))
            })?;
            let value: i64 = arg.trim().parse().map_err(|_| {
                HelperError::InvalidArgument(format!("{cmd} argument must be an integer: {arg}"))
            })?;
            if !(1..=48).contains(&value) {
                return Err(HelperError::InvalidArgument(format!(
                    "{cmd} argument {value} out of range [1, 48]"
                )));
            }
            Ok(())
        }
        "charging-schedule" => {
            check_arity(cmd, args, 1)?;
            let arg = args.first().ok_or_else(|| {
                HelperError::InvalidArgument(format!("{cmd} requires a numeric argument"))
            })?;
            let value = parse_int_arg(arg).ok_or_else(|| invalid(cmd, arg))?;
            if !(0..=1439).contains(&value) {
                return Err(invalid(cmd, arg));
            }
            Ok(())
        }
        "media-set-volume" => {
            check_arity(cmd, args, 1)?;
            let arg = args.first().ok_or_else(|| {
                HelperError::InvalidArgument(format!("{cmd} requires a numeric argument"))
            })?;
            let value = parse_float_arg(arg).ok_or_else(|| invalid(cmd, arg))?;
            if !(0.0..=10.0).contains(&value) {
                return Err(invalid(cmd, arg));
            }
            Ok(())
        }
        "climate-set-temp" => {
            check_arity(cmd, args, 1)?;
            let arg = args.first().ok_or_else(|| {
                HelperError::InvalidArgument(format!("{cmd} requires a numeric argument"))
            })?;
            let (num, unit) = strip_unit(arg);
            // Only C/F (or bare) suffixes are valid; anything else (e.g. "21x")
            // is garbage, not a temperature.
            if let Some(u) = unit {
                let up = u.to_ascii_uppercase();
                if up != 'C' && up != 'F' {
                    return Err(invalid(cmd, arg));
                }
            }
            let value = parse_float_arg(num).ok_or_else(|| invalid(cmd, arg))?;
            // QML only sends Celsius, but upstream also accepts Fahrenheit.
            let (lo, hi) = match unit.map(|c| c.to_ascii_uppercase()) {
                Some('F') => (59.0, 82.0),
                _ => (15.0, 28.0),
            };
            if !(lo..=hi).contains(&value) {
                return Err(invalid(cmd, arg));
            }
            Ok(())
        }
        "software-update-start" => {
            check_arity(cmd, args, 1)?;
            let arg = args.first().ok_or_else(|| {
                HelperError::InvalidArgument(format!("{cmd} requires a numeric argument"))
            })?;
            // Signed coordinates are valid positional values, but a negative
            // delay is never valid, including Go's minute/hour duration forms.
            if arg.trim().starts_with('-') {
                return Err(invalid(cmd, arg));
            }
            let (num, unit) = strip_unit(arg);
            // Only bare seconds and s/m/h suffixes are valid. Unknown
            // suffixes ("10x", "abc") are rejected here instead of being
            // passed to upstream Go duration parsing.
            let total_secs: f64 = match unit.map(|c| c.to_ascii_lowercase()) {
                // Seconds form QML sends ("600s") or bare seconds ("600").
                Some('s') | None => {
                    let digits = if unit.is_some() { num } else { arg };
                    // Seconds are integers <= 3600: exactly representable in
                    // f64, so the int-to-float cast is lossless here.
                    #[allow(clippy::cast_precision_loss)]
                    let secs = parse_int_arg(digits).ok_or_else(|| invalid(cmd, arg))? as f64;
                    secs
                }
                Some('m') => parse_float_arg(num).ok_or_else(|| invalid(cmd, arg))? * 60.0,
                Some('h') => parse_float_arg(num).ok_or_else(|| invalid(cmd, arg))? * 3600.0,
                _ => return Err(invalid(cmd, arg)),
            };
            if !(0.0..=3600.0).contains(&total_secs) {
                return Err(invalid(cmd, arg));
            }
            Ok(())
        }
        "charging-schedule-add" | "precondition-schedule-add" => {
            // [DAYS, TIME, LATITUDE, LONGITUDE, REPEAT?, ID?, ENABLED?]:
            // 4 required, up to 3 trailing optionals.
            if args.len() < 4 || args.len() > 7 {
                return Err(invalid(cmd, &args.join(" ")));
            }
            // [DAYS, TIME, LATITUDE, LONGITUDE, ...]: only the coordinates
            // have machine-checkable ranges here.
            for (idx, (lo, hi)) in [(2usize, (-90.0, 90.0)), (3usize, (-180.0, 180.0))] {
                if let Some(arg) = args.get(idx) {
                    let value = parse_float_arg(arg).ok_or_else(|| invalid(cmd, arg))?;
                    if !(lo..=hi).contains(&value) {
                        return Err(invalid(cmd, arg));
                    }
                }
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

/// Execs a bundled binary with a hard deadline, returning combined exit
/// status. Never invoked with attacker-controlled binary names.
pub(crate) fn run_binary(
    bin_dir: &str,
    name: &str,
    args: &[String],
    timeout: Duration,
) -> RunOutcome {
    let path = Path::new(bin_dir).join(name);
    let Ok(child) = Command::new(&path)
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .inspect_err(|e| eprintln!("Core: failed to spawn {}: {e}", path.display()))
    else {
        return RunOutcome {
            ok: false,
            stdout: String::new(),
            stderr: String::new(),
            exit_code: -1,
        };
    };
    // Wrapped at birth: the timeout arm below kills explicitly, but the
    // wait_timeout error arm and any panic unwind would otherwise leak the
    // child — Drop reaps it (see crate::child::KillOnDrop).
    let mut child = crate::child::KillOnDrop(child);

    // Drain stdout/stderr on their own threads *before* waiting, so a child
    // that fills the OS pipe buffer can't deadlock us against wait_timeout.
    // Bounded at 1 MiB per stream: vehicle state is kilobytes, and an
    // unbounded read_to_string would let a buggy child OOM the core.
    // No expect(): this is reachable from C across FFI, where a panic is
    // undefined behavior. A missing pipe (cannot happen after piped() above,
    // but checked anyway) fails gracefully.
    let (Some(mut stdout_pipe), Some(mut stderr_pipe)) = (child.stdout.take(), child.stderr.take())
    else {
        let _ = child.kill();
        let _ = child.wait();
        return RunOutcome {
            ok: false,
            stdout: String::new(),
            stderr: "Core: failed to capture child output".to_string(),
            exit_code: -1,
        };
    };
    let stdout_thread = thread::spawn(move || {
        let mut raw = Vec::new();
        let _ = (&mut stdout_pipe).take(1024 * 1024).read_to_end(&mut raw);
        String::from_utf8_lossy(&raw).into_owned()
    });
    let stderr_thread = thread::spawn(move || {
        let mut raw = Vec::new();
        let _ = (&mut stderr_pipe).take(1024 * 1024).read_to_end(&mut raw);
        String::from_utf8_lossy(&raw).into_owned()
    });

    let wait_result = child.wait_timeout(timeout);

    let (ok, exit_code, timed_out) = match wait_result {
        Ok(Some(status)) => (status.success(), status.code().unwrap_or(-1), false),
        Ok(None) => {
            // Deadline exceeded: kill and reap, matching Go's
            // context.WithTimeout-triggered SIGKILL.
            let _ = child.kill();
            let _ = child.wait();
            (false, -1, true)
        }
        Err(_) => (false, -1, false),
    };

    let stdout = stdout_thread.join().unwrap_or_default();
    let mut stderr = stderr_thread.join().unwrap_or_default();
    if timed_out {
        stderr.push_str("\nCore: timed out waiting for tesla-control");
    }
    RunOutcome {
        ok,
        stdout,
        stderr,
        exit_code,
    }
}

#[cfg(test)]
mod tests {
    use super::Core;

    #[test]
    fn negative_software_update_durations_remain_rejected() {
        for delay in ["-1s", "-1m", "-1h", " -1h "] {
            assert!(
                super::validate_arg_ranges("software-update-start", &[delay.to_string()]).is_err()
            );
        }
    }

    #[test]
    fn concurrent_phone_key_starts_share_one_confirmed_service() {
        use std::sync::{Arc, Barrier};
        let dir = tempfile::tempdir().unwrap();
        let state_dir = dir.path().to_string_lossy().into_owned();
        let response =
            r#"{"type":"response","id":"{ID}","ok":true,"stdout":"","stderr":"","exit_code":0}"#
                .to_string();
        let (session, requests, peer) = crate::session_client::tests::accept_mock(
            dir.path(),
            vec![r#"{"type":"hello","v":1}"#.to_string()],
            vec![response.clone(), response],
        );
        let core = Arc::new(Core::new(state_dir.clone(), state_dir, Some(session)).unwrap());
        std::fs::write(core.private_key_path(), "private").unwrap();
        {
            let mut cfg = core.cfg.lock().unwrap();
            cfg.vin = "5YJ3E1EA0PF000000".to_string();
            cfg.vin_state = crate::config::VinState::Paired;
        }
        let barrier = Arc::new(Barrier::new(2));
        let callers: Vec<_> = (0..2)
            .map(|_| {
                let core = Arc::clone(&core);
                let barrier = Arc::clone(&barrier);
                std::thread::spawn(move || {
                    barrier.wait();
                    core.start_phone_key()
                })
            })
            .collect();
        for caller in callers {
            caller.join().unwrap().unwrap();
        }
        let request: serde_json::Value =
            serde_json::from_str(&requests.try_recv().unwrap()).unwrap();
        assert_eq!(request["cmd"], "presence-start");
        assert!(
            requests.try_recv().is_err(),
            "concurrent starts created duplicate services"
        );
        core.stop_phone_key();
        core.session.as_ref().unwrap().invalidate();
        peer.join().unwrap();
    }

    #[test]
    fn phone_key_loss_retries_without_reporting_active_or_spinning() {
        use std::sync::atomic::Ordering;
        use std::time::Instant;
        let dir = tempfile::tempdir().unwrap();
        let state_dir = dir.path().to_string_lossy().into_owned();
        let (session, _requests, peer) = crate::session_client::tests::accept_mock(
            dir.path(),
            vec![r#"{"type":"hello","v":1}"#.to_string()],
            vec![r#"{"type":"response","id":"{ID}","ok":true,"stdout":"","stderr":"","exit_code":0}"#.to_string()],
        );
        let core = Core::new(state_dir.clone(), state_dir, Some(session)).unwrap();
        std::fs::write(core.private_key_path(), "private").unwrap();
        {
            let mut cfg = core.cfg.lock().unwrap();
            cfg.vin = "5YJ3E1EA0PF000000".to_string();
            cfg.vin_state = crate::config::VinState::Paired;
        }
        core.start_phone_key().unwrap();
        assert!(core.phone_key_started.load(Ordering::SeqCst));
        // The scripted peer exits on the next request, simulating transport loss.
        assert!(core.run("ping", &[]).is_err());
        peer.join().unwrap();
        let stopped = core.poll_phone_key_event().unwrap();
        assert_eq!(stopped.kind, "presence_stopped");
        assert_eq!(stopped.vin, "5YJ3E1EA0PF000000");
        assert!(!core.phone_key_started.load(Ordering::SeqCst));
        assert!(
            core.phone_key_enabled(),
            "retry must retain the CPU lease intent"
        );
        assert!(core.phone_key_retry_at.lock().unwrap().is_some());
        assert!(
            core.poll_phone_key_event().is_none(),
            "failed startup must not spin the event poll"
        );
        *core.phone_key_retry_at.lock().unwrap() = Some(Instant::now());
        assert_eq!(
            core.poll_phone_key_event().unwrap().kind,
            "presence_stopped"
        );
        assert!(
            core.phone_key_enabled(),
            "a failed retry must retain the CPU lease intent"
        );
        core.stop_phone_key();
        assert!(!core.phone_key_enabled.load(Ordering::SeqCst));
        assert!(core.phone_key_retry_at.lock().unwrap().is_none());
    }

    #[test]
    fn failed_initial_start_retains_cpu_lease_intent_until_stop() {
        let dir = tempfile::tempdir().unwrap();
        let state = dir.path().to_string_lossy().into_owned();
        let session = crate::session_client::SessionClient::new(
            dir.path().join("missing-session"),
            "bluez",
            dir.path().to_path_buf(),
        );
        let mut core = Core::new(state.clone(), state, Some(session)).unwrap();
        std::fs::write(core.private_key_path(), "private").unwrap();
        {
            let mut cfg = core.cfg.lock().unwrap();
            cfg.vin = "5YJ3E1EA0PF000000".to_string();
            cfg.vin_state = crate::config::VinState::Paired;
        }
        assert!(core.start_phone_key().is_err());
        let mut enabled = false;
        assert_eq!(
            unsafe {
                crate::ffi::core_phone_key_enabled(
                    std::ptr::addr_of_mut!(core),
                    std::ptr::addr_of_mut!(enabled),
                )
            },
            crate::ffi::CoreError::Ok
        );
        assert!(
            enabled,
            "failed startup must keep the phone awake for retry"
        );
        core.stop_phone_key();
        assert_eq!(
            unsafe {
                crate::ffi::core_phone_key_enabled(
                    std::ptr::addr_of_mut!(core),
                    std::ptr::addr_of_mut!(enabled),
                )
            },
            crate::ffi::CoreError::Ok
        );
        assert!(!enabled, "an explicit stop must release the lease intent");
    }

    #[test]
    fn unknown_config_schema_refuses_settings_keys_and_pairing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        let original = r#"{"version":99,"vin":"5YJ3E1EA0PF000000","model":"","key_name":"phone","connect_timeout_sec":20,"command_timeout_sec":5,"vin_state":"paired","future_setting":true}"#;
        std::fs::write(&path, original).unwrap();
        let state_dir = dir.path().to_string_lossy().into_owned();
        let core = Core::new(state_dir.clone(), state_dir, None).unwrap();
        assert!(core
            .set_config("5YJ3E1EA0PF111111", "", "phone", 20, 5)
            .is_err());
        assert!(core.generate_key(true).is_err());
        assert!(core.pair().is_err());
        assert_eq!(std::fs::read_to_string(path).unwrap(), original);
        assert!(!core.private_key_path().exists());
    }

    #[test]
    fn review_vin_change_stays_unpaired_after_restart() {
        let dir = tempfile::tempdir().unwrap();
        let state_dir = dir.path().to_string_lossy().into_owned();
        let core = Core::new(state_dir.clone(), state_dir.clone(), None).unwrap();
        core.set_config("5YJ3E1EA0PF000000", "", "phone", 20, 5)
            .unwrap();
        // A previous vehicle's key files remain when Settings changes VIN.
        std::fs::write(core.private_key_path(), "previous vehicle private key").unwrap();
        std::fs::write(core.public_key_path(), "previous vehicle public key").unwrap();
        core.set_config("5YJ3E1EA0PF111111", "", "phone", 20, 5)
            .unwrap();
        drop(core);

        let reopened = Core::new(state_dir.clone(), state_dir, None).unwrap();
        let result = reopened.start_phone_key();
        assert!(
            matches!(result, Err(crate::error::OperationError::NotPaired)),
            "changing VIN requires enrollment for that vehicle, including after restart: {result:?}"
        );
    }

    #[test]
    fn review_startup_does_not_rewrite_unknown_config_schema() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        let original = r#"{"version":99,"vin":"5YJ3E1EA0PF000000","model":"","key_name":"phone","connect_timeout_sec":20,"command_timeout_sec":5,"vin_state":"unpaired","future_setting":{"keep":true}}"#;
        std::fs::write(&path, original).unwrap();
        std::fs::write(dir.path().join("private_key.pem"), "private").unwrap();
        std::fs::write(dir.path().join("public_key.pem"), "public").unwrap();
        let state_dir = dir.path().to_string_lossy().into_owned();
        let _core = Core::new(state_dir.clone(), state_dir, None).unwrap();
        assert_eq!(
            std::fs::read_to_string(path).unwrap(),
            original,
            "an older build must not destroy fields from a newer config schema"
        );
    }

    #[test]
    fn review_failed_config_save_preserves_running_config() {
        let dir = tempfile::tempdir().unwrap();
        let state_dir = dir.path().to_string_lossy().into_owned();
        let core = Core::new(state_dir.clone(), state_dir, None).unwrap();
        core.set_config("5YJ3E1EA0PF000000", "model3", "phone", 20, 5)
            .unwrap();
        let before = core.get_config();
        // Deterministic rename failure, including when tests run as root.
        std::fs::remove_file(core.config_path()).unwrap();
        std::fs::create_dir(core.config_path()).unwrap();
        assert!(core
            .set_config("5YJ3E1EA0PF111111", "modely", "other", 30, 10)
            .is_err());
        assert_eq!(
            core.get_config(),
            before,
            "a rejected save must not silently change the active vehicle/settings"
        );
    }

    #[test]
    fn review_schedule_accepts_signed_coordinates_and_end_only_time() {
        let dir = tempfile::tempdir().unwrap();
        let state_dir = dir.path().to_string_lossy().into_owned();
        let core = Core::new(state_dir.clone(), state_dir, None).unwrap();
        core.set_config("5YJ3E1EA0PF000000", "", "phone", 20, 5)
            .unwrap();
        std::fs::write(core.private_key_path(), "private").unwrap();
        for args in [
            ["all", "22:00-06:00", "-33.8688", "151.2093"],
            ["all", "22:00-06:00", "37.7749", "-122.4194"],
            ["all", "-06:00", "48.8584", "2.2945"],
        ] {
            let args = args.map(str::to_string);
            // With no bundled binary the one-shot path returns Ok(ok=false).
            // This checks that valid positional values reach dispatch at all.
            let result = core.run("charging-schedule-add", &args);
            assert!(
                result.is_ok(),
                "valid schedule {args:?} rejected: {result:?}"
            );
        }
    }

    #[test]
    fn test_get_version_is_semver() {
        // The app compares this to its own APP_VERSION on the Settings page
        // and flags a mismatch, so a malformed/empty version would either
        // trip every install with a false warning or hide a real one.
        let v = env!("CARGO_PKG_VERSION");
        let parts: Vec<&str> = v.split('.').collect();
        assert_eq!(parts.len(), 3, "version {v:?} must be x.y.z");
        assert!(
            v.chars().all(|c| c.is_ascii_digit() || c == '.'),
            "version {v:?} must be digits and dots only"
        );
    }

    #[test]
    fn test_validate_arg_ranges() {
        // Unbounded commands pass through untouched (only the dash-check
        // guard in run() constrains them).
        assert!(super::validate_arg_ranges("lock", &[]).is_ok());
        assert!(super::validate_arg_ranges("ping", &[]).is_ok());

        // Bounded charging commands.
        assert!(super::validate_arg_ranges("charging-set-limit", &["50".into()]).is_ok());
        assert!(super::validate_arg_ranges("charging-set-limit", &["100".into()]).is_ok());
        assert!(super::validate_arg_ranges("charging-set-limit", &["49".into()]).is_err());
        assert!(super::validate_arg_ranges("charging-set-limit", &["101".into()]).is_err());
        assert!(super::validate_arg_ranges("charging-set-amps", &["1".into()]).is_ok());
        assert!(super::validate_arg_ranges("charging-set-amps", &["48".into()]).is_ok());
        assert!(super::validate_arg_ranges("charging-set-amps", &["0".into()]).is_err());
        assert!(super::validate_arg_ranges("charging-set-amps", &["49".into()]).is_err());

        // Non-integer / missing arguments are rejected, not passed through.
        assert!(super::validate_arg_ranges("charging-set-limit", &[]).is_err());
        assert!(super::validate_arg_ranges("charging-set-limit", &["abc".into()]).is_err());
    }

    // Regression test for a real bug: generate_key locked self.cfg, then
    // locked it again on the same thread to read vin/timeouts out of it.
    // std::sync::Mutex isn't reentrant, so that second lock() blocked
    // forever - on the app side this surfaced as the "Generate Key" button
    // reading "Generating..." and never completing. Bounded by a channel +
    // recv_timeout so a reintroduced deadlock fails this test loudly and
    // fast in CI instead of hanging the run.
    #[test]
    fn test_generate_key_does_not_deadlock() {
        let dir = tempfile::tempdir().unwrap();
        let bin_dir = dir.path().join("bin");
        std::fs::create_dir_all(&bin_dir).unwrap();
        let core = Core::new(
            bin_dir.to_string_lossy().into_owned(),
            dir.path().to_string_lossy().into_owned(),
            None,
        )
        .unwrap();

        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(core.generate_key(false));
        });

        match rx.recv_timeout(std::time::Duration::from_secs(5)) {
            Ok(Err(_)) => {
                // No tesla-keygen binary in this test's fake bin_dir, so the
                // one-shot fallback is expected to fail - what's under test
                // is that the call returns at all, not that it succeeds.
            }
            Ok(Ok(pubkey)) => {
                panic!("unexpected success with no tesla-keygen binary present: {pubkey:?}");
            }
            Err(e) => {
                panic!("generate_key did not return within 5s - looks deadlocked on self.cfg (recv: {e:?})");
            }
        }
    }

    #[test]
    fn test_preview_destination_vectors() {
        let (kind, v1, v2) = Core::preview_destination("geo:48.8584,2.2945").unwrap();
        assert_eq!(
            (kind.as_str(), v1.as_str(), v2.as_str()),
            ("gps", "48.8584", "2.2945")
        );
        let (kind, v1, _) = Core::preview_destination("1600 Amphitheatre Parkway").unwrap();
        assert_eq!(kind, "address");
        assert_eq!(v1, "1600 Amphitheatre Parkway");
        assert!(Core::preview_destination("").is_err());
        assert!(Core::preview_destination("999,999").is_err());
    }

    #[test]
    fn test_share_destination_needs_vin_and_session() {
        let dir = tempfile::tempdir().unwrap();
        let core = Core::new(
            dir.path().to_string_lossy().into_owned(),
            dir.path().to_string_lossy().into_owned(),
            None,
        )
        .unwrap();
        // Unparseable text is refused before anything else.
        assert!(core.share_destination("").is_err());
        assert!(core.share_destination("999,999").is_err());
        // No VIN configured (default config) is refused.
        assert!(core
            .share_destination("geo:48.8584,2.2945")
            .unwrap_err()
            .to_string()
            .contains("VIN"));
    }

    #[test]
    fn test_phone_key_requires_completed_pairing() {
        let dir = tempfile::tempdir().unwrap();
        let core = Core::new(
            dir.path().to_string_lossy().into_owned(),
            dir.path().to_string_lossy().into_owned(),
            None,
        )
        .unwrap();
        let result = core.start_phone_key();
        assert!(
            result.is_err(),
            "unpaired core must refuse to start a phone key"
        );
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("completed pairing"),
            "reason should point at the missing pairing"
        );
    }

    fn assert_eligible_but_no_session(core: &Core) {
        let result = core.start_phone_key();
        assert!(
            result.is_err(),
            "no persistent session must still refuse to start"
        );
        let reason = result.unwrap_err();
        assert!(
            reason.to_string().contains("persistent BLE session"),
            "key should pass pairing eligibility: {reason}"
        );
    }

    #[test]
    fn test_legacy_paired_key_is_migrated() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("private_key.pem"), "private").unwrap();
        std::fs::write(dir.path().join("public_key.pem"), "public").unwrap();
        std::fs::write(
            dir.path().join("config.json"),
            r#"{"vin":"5YJ3E1EA0PF000000","model":"","key_name":"harbour-electric-eel","connect_timeout_sec":20,"command_timeout_sec":5}"#,
        )
        .unwrap();
        let core = Core::new(
            dir.path().to_string_lossy().into_owned(),
            dir.path().to_string_lossy().into_owned(),
            None,
        )
        .unwrap();
        assert_eligible_but_no_session(&core);
    }

    #[test]
    fn test_v2_explicit_unpaired_with_key_files_is_preserved() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("private_key.pem"), "private").unwrap();
        std::fs::write(dir.path().join("public_key.pem"), "public").unwrap();
        std::fs::write(
            dir.path().join("config.json"),
            r#"{"version":2,"vin":"5YJ3E1EA0PF000000","model":"","key_name":"harbour-electric-eel","connect_timeout_sec":20,"command_timeout_sec":5,"vin_state":"unpaired"}"#,
        )
        .unwrap();
        let core = Core::new(
            dir.path().to_string_lossy().into_owned(),
            dir.path().to_string_lossy().into_owned(),
            None,
        )
        .unwrap();
        assert!(matches!(
            core.start_phone_key(),
            Err(crate::error::OperationError::NotPaired)
        ));
        let cfg = crate::config::Config::load(&dir.path().join("config.json")).unwrap();
        assert_eq!(cfg.vin_state, crate::config::VinState::Unpaired);
    }

    #[test]
    fn test_key_files_without_vin_stay_unpaired() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("private_key.pem"), "private").unwrap();
        std::fs::write(dir.path().join("public_key.pem"), "public").unwrap();
        let core = Core::new(
            dir.path().to_string_lossy().into_owned(),
            dir.path().to_string_lossy().into_owned(),
            None,
        )
        .unwrap();
        let result = core.start_phone_key();
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("completed pairing"),
            "no VIN means phone-key must still wait for pairing"
        );
    }

    #[test]
    fn test_generate_key_reuse_keeps_pairing() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let keygen = dir.path().join("tesla-keygen");
        // Stand-in for the no-session keygen backend reprinting the old key.
        std::fs::write(&keygen, "#!/bin/sh\nexit 0\n").unwrap();
        std::fs::set_permissions(&keygen, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::write(dir.path().join("private_key.pem"), "private").unwrap();
        std::fs::write(dir.path().join("public_key.pem"), "existing-pem\n").unwrap();
        std::fs::write(
            dir.path().join("config.json"),
            r#"{"version":2,"vin":"5YJ3E1EA0PF000000","model":"","key_name":"harbour-electric-eel","connect_timeout_sec":20,"command_timeout_sec":5,"vin_state":"paired"}"#,
        )
        .unwrap();
        let core = Core::new(
            dir.path().to_string_lossy().into_owned(),
            dir.path().to_string_lossy().into_owned(),
            None,
        )
        .unwrap();
        let pubkey = core
            .generate_key(false)
            .expect("reuse must preserve the existing enrollment");
        assert_eq!(pubkey, "existing-pem");
        let cfg = crate::config::Config::load(&dir.path().join("config.json")).unwrap();
        assert_eq!(cfg.vin_state, crate::config::VinState::Paired);
    }

    #[test]
    fn test_get_config_has_key_requires_usable_keypair() {
        // get_config reports has_key from public_key.pem alone, even when it
        // is empty or private_key.pem is missing. The UI gates every command
        // on has_key, so a half-present key shows "Key ready" and then every
        // run() fails with NoKey. has_key must require both files non-empty.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("private_key.pem"), "private").unwrap();
        std::fs::write(dir.path().join("public_key.pem"), "").unwrap();
        let core = Core::new(
            dir.path().to_string_lossy().into_owned(),
            dir.path().to_string_lossy().into_owned(),
            None,
        )
        .unwrap();
        let (_, _, _, _, _, has_key, _) = core.get_config();
        assert!(
            !has_key,
            "empty public_key.pem must not count as a usable key"
        );
        std::fs::remove_file(dir.path().join("private_key.pem")).unwrap();
        std::fs::write(dir.path().join("public_key.pem"), "public").unwrap();
        let (_, _, _, _, _, has_key, _) = core.get_config();
        assert!(
            !has_key,
            "missing private_key.pem must not count as a usable key"
        );
    }

    #[test]
    fn test_validate_arg_ranges_covers_all_bounded_commands() {
        // CommandCatalog.js bounds climate-set-temp, media-set-volume,
        // charging-schedule and software-update-start, but the server-side
        // defense only checks charging-set-limit/amps. The JSON stdin path
        // bypasses QML sliders, so unvalidated commands accept huge values
        // that upstream Atoi-to-int32 wraps or forwards to the vehicle.
        assert!(
            super::validate_arg_ranges("climate-set-temp", &["999C".into()]).is_err(),
            "climate-set-temp 999C must be rejected server-side"
        );
        assert!(
            super::validate_arg_ranges("media-set-volume", &["99".into()]).is_err(),
            "media-set-volume 99 must be rejected server-side"
        );
        assert!(
            super::validate_arg_ranges("charging-schedule", &["9999".into()]).is_err(),
            "charging-schedule 9999 must be rejected server-side"
        );
        assert!(
            super::validate_arg_ranges("software-update-start", &["99999s".into()]).is_err(),
            "software-update-start 99999s must be rejected server-side"
        );
    }

    #[test]
    fn review_software_update_rejects_garbage_units_and_extra_args() {
        // validate_arg_ranges("software-update-start") only validates the
        // seconds form ("600s" / bare int) and lets every other single-letter
        // suffix through with `_ => Ok(())` for upstream to parse. That hole
        // accepts "10x", "abc" and unbounded "999999h", and every bounded
        // command ignores trailing extra args entirely (only args.first() /
        // args.get(2,3) are inspected).
        for bad in ["10x", "abc", "10z", "999999h", "999999m"] {
            assert!(
                super::validate_arg_ranges("software-update-start", &[bad.to_string()]).is_err(),
                "software-update-start {bad:?} must be rejected server-side, not passed to upstream"
            );
        }
        assert!(
            super::validate_arg_ranges(
                "software-update-start",
                &["0".to_string(), "extra".to_string()]
            )
            .is_err(),
            "trailing extra args must be rejected, not silently ignored"
        );
        assert!(
            super::validate_arg_ranges(
                "charging-set-limit",
                &["80".to_string(), "extra".to_string()]
            )
            .is_err(),
            "charging-set-limit with an extra arg must be rejected, not silently ignored"
        );
    }
}
