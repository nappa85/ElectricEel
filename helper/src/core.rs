//! In-process control core for harbour-electric-eel (see `BLUEZ_BACKEND_PLAN.md`
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
use std::time::Duration;

use wait_timeout::ChildExt;

use crate::commands::{is_known_command, is_pin_command};
use crate::config::{Config, ConfigVersion, VinState};
use crate::error::{HelperError, OperationError};
use crate::session_client::{SessionClient, SessionEvent};

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
    /// Whether the `BlueZ` proximity/authentication service is currently
    /// started. `compare_exchange` serializes concurrent `start_phone_key`
    /// calls; a plain `Mutex<bool>` was overkill for a single flag.
    phone_key_started: AtomicBool,
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
        // Older schema versions are migrated up to CURRENT and persisted at
        // that version. Pairing is also healed whenever a VIN is configured
        // and both key files are already on disk: V0 installs never had
        // `vin_state`, and later files can still say `unpaired` after a
        // Settings save or a Generate Key reuse that used to wipe the flag.
        // Either way the key was enrolled through the pairing page, so do
        // not force another NFC tap just to start phone-key presence.
        let mut dirty = false;
        if cfg.version < ConfigVersion::CURRENT {
            cfg.version = ConfigVersion::CURRENT;
            dirty = true;
        }
        if cfg.vin_state == VinState::Unpaired
            && !cfg.vin.is_empty()
            && Self::key_files_in(&state_dir)
        {
            cfg.vin_state = VinState::Paired;
            dirty = true;
            crate::keylog::log("core", "vin_state healed to paired (key files present)");
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
            phone_key_started: AtomicBool::new(false),
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
        for a in args {
            if a.starts_with('-') {
                return Err(HelperError::InvalidArgument(format!(
                    "arguments may not start with '-': {a}"
                )));
            }
        }
        validate_arg_ranges(cmd, args)?;

        let (common, timeout, vin, connect_timeout_sec, command_timeout_sec) = {
            let cfg = self.cfg.lock().unwrap();
            let common = self.common_args_locked(&cfg)?;
            // i64 before the sum, not i32: an overflowing i32 sum here used
            // to be able to turn into a negative (already-cancelled)
            // deadline - a previously-fixed bug, preserved here.
            let secs = i64::from(cfg.connect_timeout_sec) + i64::from(cfg.command_timeout_sec) + 10;
            (
                common,
                Duration::from_secs(secs.cast_unsigned()),
                cfg.vin.clone(),
                cfg.connect_timeout_sec,
                cfg.command_timeout_sec,
            )
        };

        let _permit = self
            .ble_sem
            .try_lock()
            .map_err(|_| HelperError::Busy("another BLE command is in progress".to_string()))?;

        let mut command_argv = common;
        command_argv.push(cmd.to_string());
        command_argv.extend(args.iter().cloned());

        if is_pin_command(cmd) {
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
        let private_existed = self.private_key_path().is_file();
        let replacing = force || !private_existed;
        // tesla-session reprints an existing key when force is unset. That
        // is not a new key, so keep vin_state and the live phone-key session.
        if !replacing {
            if let Ok(pubkey) = std::fs::read_to_string(self.public_key_path()) {
                let pubkey = pubkey.trim();
                if !pubkey.is_empty() {
                    eprintln!("Core: generate_key reused existing key");
                    return Ok(pubkey.to_string());
                }
            }
        }

        if replacing {
            self.stop_phone_key();
        }
        // Holds the config mutex for the whole call, same as the original -
        // GenerateKey doesn't read Config, but this still serializes
        // concurrent key generation against writing the same key files.
        // Read from this single guard below rather than locking again:
        // std::sync::Mutex isn't reentrant, so a second self.cfg.lock() on
        // this thread while _cfg is still held would deadlock forever -
        // exactly what happened here before this fix (Generate Key hanging
        // indefinitely on the QML side, since the worker thread never
        // returns to emit keyGenerated).
        let mut cfg = self.cfg.lock().unwrap();

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
                // Pure crypto, so the hci-vs-bluez no-fallback question
                // doesn't apply here; still, the session path is preferred
                // so tesla-keygen stops being exec'd (Phases 3-4). Only
                // falls back to the one-shot when the session errored.
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
                        if let Err(e) = std::fs::write(self.public_key_path(), &pubkey) {
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
                        eprintln!(
                            "Core: persistent session unavailable ({e}); falling back to one-shot tesla-keygen"
                        );
                        generate_key_one_shot(
                            &self.bin_dir,
                            &key_path,
                            &self.public_key_path(),
                            force,
                        )?
                    }
                }
            }
        };

        if replacing {
            cfg.vin_state = VinState::Unpaired;
            if let Err(e) = cfg.save(&self.config_path()) {
                return Err(OperationError::Persist(e));
            }
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
    pub(crate) fn pair(&self) -> Result<(bool, String, String), HelperError> {
        self.stop_phone_key();
        let (common, vin, key_path, connect_timeout_sec, command_timeout_sec, timeout) = {
            let cfg = self.cfg.lock().unwrap();
            let common = self.common_args_locked(&cfg)?;
            // Same envelope as Run() (connect + command + 10s), plus an
            // allowance that covers the physical NFC-card tap at the
            // center console this command waits for. The persistent
            // session (tesla-session, helper/session/main.go) deliberately
            // holds the BLE connection open for 90s *after* a successful
            // add-key-request so the operator has time to walk up and tap
            // the card, and only then replies - so the deadline here must
            // exceed that 90s grace period, or the core kills the session
            // in the middle of pairing and reports a spurious timeout.
            // (A flat 30s used to be added, which under the 90s grace was
            // less than the real reply latency even at default timeouts:
            // 20+5+10+30 = 65s < 90s.) 95s comfortably tops the 90s grace.
            let secs =
                i64::from(cfg.connect_timeout_sec) + i64::from(cfg.command_timeout_sec) + 10 + 95;
            (
                common,
                cfg.vin.clone(),
                self.private_key_path().to_string_lossy().into_owned(),
                cfg.connect_timeout_sec,
                cfg.command_timeout_sec,
                Duration::from_secs(secs.cast_unsigned()),
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
            "add-key-request",
            pubkey_path.to_string_lossy().as_ref(),
            "owner",
            "android_device",
        ]
        .map(str::to_string)
        .to_vec();

        eprintln!("Core: pair()");
        // Unlike run()'s Busy case, this is intentionally a normal (non-error)
        // reply, matching the original implementation.
        let Ok(_permit) = self.ble_sem.try_lock() else {
            return Ok((
                false,
                String::new(),
                "another BLE command is in progress".to_string(),
            ));
        };

        let outcome = if let Some(session) = &self.session {
            // Route through the (possibly bluez) persistent session: the
            // add-key-request command is vendored in commands_vendor.go and
            // inherits whatever transport the session was spawned with. This
            // is what makes bluez pairing work (previously blocked as
            // unsupported - now it's exactly the run() path).
            // No fallback in bluez mode: a one-shot tesla-control would
            // take exclusive adapter control and drop the other
            // connections (e.g. a soundbar) bluez mode exists to keep.
            match session.run(
                &pair_args[0],
                &pair_args[1..],
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
                    return Err(HelperError::SessionUnavailable(format!(
                        "bluez persistent session failed during pairing: {e}"
                    )));
                }
                Err(e) => {
                    eprintln!(
                        "Core: persistent session unavailable ({e}); falling back to one-shot tesla-control for pair"
                    );
                    let mut argv = common;
                    argv.extend(pair_args.iter().cloned());
                    run_binary(&self.bin_dir, "tesla-control", &argv, timeout)
                }
            }
        } else {
            let mut argv = common;
            argv.extend(pair_args.iter().cloned());
            run_binary(&self.bin_dir, "tesla-control", &argv, timeout)
        };
        if !outcome.ok {
            return Ok((false, outcome.stdout, outcome.stderr.trim().to_string()));
        }
        {
            let mut cfg = self.cfg.lock().unwrap();
            cfg.vin_state = VinState::Paired;
            cfg.save(&self.config_path()).map_err(|e| {
                HelperError::SessionUnavailable(format!(
                    "paired key but could not persist phone-key state: {e}"
                ))
            })?;
        }
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
        if self.phone_key_started.load(Ordering::SeqCst) {
            return Ok(());
        }
        let (vin, key_path, connect_timeout_sec, command_timeout_sec, eligible) = {
            let cfg = self.cfg.lock().unwrap();
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
        // Claim the flag before the (blocking) start so a second concurrent
        // caller returns Ok instead of starting the service twice.
        if self
            .phone_key_started
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            return Ok(());
        }
        crate::keylog::log(
            "core",
            &format!("phone-key start vin={vin} connect={connect_timeout_sec}s"),
        );
        match session.start_presence(
            &vin,
            &key_path,
            connect_timeout_sec,
            command_timeout_sec,
            Duration::from_secs(10),
        ) {
            Ok(outcome) if outcome.ok => {
                crate::keylog::log("core", "phone-key presence started");
                Ok(())
            }
            Ok(outcome) => {
                self.phone_key_started.store(false, Ordering::SeqCst);
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
                crate::keylog::log("core", &format!("phone-key start error: {e}"));
                Err(OperationError::PresenceFailed(e.to_string()))
            }
        }
    }

    /// Stops proximity mode and closes its live BLE connection. Idempotent.
    pub(crate) fn stop_phone_key(&self) {
        if !self.phone_key_started.load(Ordering::SeqCst) {
            return;
        }
        crate::keylog::log("core", "phone-key stop");
        if let Some(session) = &self.session {
            let cfg = self.cfg.lock().unwrap();
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

    pub(crate) fn poll_phone_key_event(&self) -> Option<SessionEvent> {
        let mut event = self.session.as_ref().and_then(SessionClient::poll_event);
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
        if event.as_ref().is_some_and(|e| e.kind == "presence_stopped") {
            self.phone_key_started.store(false, Ordering::SeqCst);
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
                            event.kind = "presence_error".to_string();
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

        self.stop_phone_key();
        let mut cfg = self.cfg.lock().unwrap();
        let vin_changed = cfg.vin != vin.trim();
        cfg.vin = vin.trim().to_string();
        cfg.model = model.trim().to_ascii_lowercase();
        cfg.key_name = key_name.trim().to_string();
        cfg.connect_timeout_sec = connect_timeout_sec;
        cfg.command_timeout_sec = command_timeout_sec;
        if vin_changed {
            cfg.vin_state = VinState::Unpaired;
        }
        if let Err(e) = cfg.save(&self.config_path()) {
            return Err(OperationError::Persist(e));
        }
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
        if !vin_changed {
            let _ = self.start_phone_key();
        }
        Ok(())
    }

    pub(crate) fn get_config(&self) -> GetConfigReply {
        let cfg = self.cfg.lock().unwrap();
        let (has_key, pub_key) = match std::fs::read_to_string(self.public_key_path()) {
            Ok(data) => (true, data),
            Err(_) => (false, String::new()),
        };
        (
            cfg.vin.clone(),
            cfg.model.clone(),
            cfg.key_name.clone(),
            cfg.connect_timeout_sec,
            cfg.command_timeout_sec,
            has_key,
            pub_key,
        )
    }
}

impl Drop for Core {
    fn drop(&mut self) {
        self.stop_phone_key();
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

/// Rejects out-of-range numeric arguments for the few commands with a
/// well-defined value range, before anything reaches the wired session or
/// the subprocess. This is server-side defense-in-depth: the QML sliders in
/// `ArgumentDialog.qml` already bound these, but the JSON stdin path accepts
/// arbitrary args, and the vendored upstream handlers (`commands_vendor.go`)
/// do `Atoi` to `int32` with no range check, so a huge value would wrap or be
/// sent to the vehicle. Keep the per-command bounds here in sync with the
/// `min`/`max` in `app/qml/js/CommandCatalog.js`.
fn validate_arg_ranges(cmd: &str, args: &[String]) -> Result<(), HelperError> {
    let bounds: Option<(i64, i64)> = match cmd {
        "charging-set-limit" => Some((50, 100)),
        "charging-set-amps" => Some((1, 48)),
        _ => None,
    };
    let Some((lo, hi)) = bounds else {
        return Ok(());
    };
    let arg = args.first().ok_or_else(|| {
        HelperError::InvalidArgument(format!("{cmd} requires a numeric argument"))
    })?;
    let value: i64 = arg.trim().parse().map_err(|_| {
        HelperError::InvalidArgument(format!("{cmd} argument must be an integer: {arg}"))
    })?;
    if !(lo..=hi).contains(&value) {
        return Err(HelperError::InvalidArgument(format!(
            "{cmd} argument {value} out of range [{lo}, {hi}]"
        )));
    }
    Ok(())
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
    let Ok(mut child) = Command::new(&path)
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

    // Drain stdout/stderr on their own threads *before* waiting, so a child
    // that fills the OS pipe buffer can't deadlock us against wait_timeout.
    let mut stdout_pipe = child.stdout.take().expect("piped stdout");
    let mut stderr_pipe = child.stderr.take().expect("piped stderr");
    let stdout_thread = thread::spawn(move || {
        let mut buf = String::new();
        let _ = stdout_pipe.read_to_string(&mut buf);
        buf
    });
    let stderr_thread = thread::spawn(move || {
        let mut buf = String::new();
        let _ = stderr_pipe.read_to_string(&mut buf);
        buf
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
    fn test_v2_unpaired_with_key_files_is_healed() {
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
        assert_eligible_but_no_session(&core);
        let cfg = crate::config::Config::load(&dir.path().join("config.json")).unwrap();
        assert_eq!(cfg.vin_state, crate::config::VinState::Paired);
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
        let dir = tempfile::tempdir().unwrap();
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
            .expect("reuse must not invoke tesla-keygen");
        assert_eq!(pubkey, "existing-pem");
        let cfg = crate::config::Config::load(&dir.path().join("config.json")).unwrap();
        assert_eq!(cfg.vin_state, crate::config::VinState::Paired);
    }
}
