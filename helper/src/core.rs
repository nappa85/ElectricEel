//! In-process control core for harbour-electric-eel (see `docs/architecture.md`).
//! The runtime drives it from its serial BLE executor; config, key files and
//! the in-process protocol library (`protocol_client.rs`) are owned by Rust.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use crate::commands::Command;
use crate::config::{Config, ConfigVersion, Model, VinState};
use crate::cover_action::CoverAction;
use crate::error::{HelperError, OperationError};
use crate::protocol_client::{ProtocolClient, RunOutcome, SessionError, SessionEvent};
use crate::share::{parse_shared_text, Destination, DestinationKind};
use crate::tesla_ble::{Domain, Operation};

/// `get_config`'s payload: (vin, model, `key_name`, `connect_timeout_sec`,
/// `command_timeout_sec`, `has_key`, `public_key_pem`).
pub(crate) type GetConfigReply = (String, Model, String, i32, i32, bool, String);

/// Delay before retrying a failed presence start.
const PHONE_KEY_RETRY: Duration = Duration::from_secs(5);

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

pub struct Core {
    cfg: Mutex<Config>,
    /// Capacity-1 foreground-command admission gate. Background protocol
    /// responses remain independent of foreground command scheduling.
    ble_sem: Mutex<()>,
    state_dir: PathBuf,
    /// The protocol library client; `None` only when BLE is unavailable.
    session: Option<ProtocolClient>,
    /// Serializes lifecycle/config/key mutations (every writer of `cfg` holds
    /// it, so `cfg` itself is only locked briefly and never across I/O).
    /// Always acquired before cfg.
    phone_key_gate: Mutex<()>,
    /// Whether the `BlueZ` proximity/authentication service is currently started.
    phone_key_started: AtomicBool,
    phone_key_enabled: AtomicBool,
    pub(crate) keepalive_wakeup: Arc<(Mutex<u64>, Condvar)>,
    phone_key_retry_at: Mutex<Option<Instant>>,
}

impl Core {
    /// Builds the control core from on-disk state.
    ///
    /// # Errors
    ///
    /// Returns [`HelperError::SessionUnavailable`] when `config.json` can't be
    /// read from `state_dir`.
    pub(crate) fn new(
        state_dir: impl Into<PathBuf>,
        session: Option<ProtocolClient>,
    ) -> Result<Core, HelperError> {
        let state_dir = state_dir.into();
        let config_path = state_dir.join("config.json");
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
            state_dir,
            session,
            phone_key_gate: Mutex::new(()),
            phone_key_started: AtomicBool::new(false),
            phone_key_enabled: AtomicBool::new(false),
            keepalive_wakeup: Arc::new((Mutex::new(0), Condvar::new())),
            phone_key_retry_at: Mutex::new(None),
        })
    }

    fn config_path(&self) -> PathBuf {
        self.state_dir.join("config.json")
    }

    pub(crate) fn private_key_path(&self) -> PathBuf {
        self.state_dir.join("private_key.pem")
    }

    pub(crate) fn public_key_path(&self) -> PathBuf {
        self.state_dir.join("public_key.pem")
    }

    fn key_files_in(state_dir: &Path) -> bool {
        state_dir.join("private_key.pem").is_file() && state_dir.join("public_key.pem").is_file()
    }

    fn key_string(&self) -> String {
        self.private_key_path().to_string_lossy().into_owned()
    }

    /// A command needs a configured VIN and a private key.
    fn ensure_ready(&self, cfg: &Config) -> Result<(), HelperError> {
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
        Ok(())
    }

    /// VIN and timeouts for one operation (snapshot isolation: a concurrent
    /// config change applies to the next operation).
    fn snapshot(&self) -> Result<(String, i32, i32), HelperError> {
        let _gate = lock(&self.phone_key_gate);
        let cfg = lock(&self.cfg);
        self.ensure_ready(&cfg)?;
        Ok((
            cfg.vin.clone(),
            cfg.connect_timeout_sec,
            cfg.command_timeout_sec,
        ))
    }

    fn library(&self) -> Result<&ProtocolClient, HelperError> {
        self.session.as_ref().ok_or_else(|| {
            HelperError::SessionUnavailable("protocol library unavailable".to_string())
        })
    }

    /// Executes a known command through the protocol library.
    pub(crate) fn run(&self, cmd: &str, args: &[String]) -> Result<RunOutcome, HelperError> {
        let cmd: Command = cmd
            .parse()
            .map_err(|_| HelperError::UnknownCommand(cmd.to_string()))?;
        // The single argument parser validates ranges/arity/units and builds
        // the typed operation; a rejection is the caller's argument error.
        let operation = crate::protocol_commands::parse(cmd, args)
            .map_err(|e| HelperError::InvalidArgument(format!("{cmd}: {e}")))?;
        let _permit = self
            .ble_sem
            .try_lock()
            .map_err(|_| HelperError::Busy("another BLE command is in progress".to_string()))?;
        let (vin, connect, command) = self.snapshot()?;
        // PINs are always redacted; schedule coordinates are location data
        // and get the same treatment (arg count only, never values).
        if cmd.has_pin() || cmd.has_location() {
            eprintln!("Core: run({cmd}, [{} redacted args])", args.len());
        } else {
            eprintln!("Core: run({cmd}, {args:?})");
        }
        let result = self
            .library()?
            .run(cmd, operation, &vin, &self.key_string(), connect, command)
            .map_err(|e| HelperError::SessionUnavailable(format!("{cmd}: {e}")));
        if let Ok(outcome) = &result {
            if outcome.ok {
                match cmd {
                    Command::VehicleName => self.remember_vehicle_name(&outcome.stdout),
                    Command::SetVehicleName => self.remember_vehicle_name(&args[0]),
                    _ => (),
                }
            }
        }
        // Failures reach the UI only as transient text; keep them in the
        // phone-key log too. Never log arguments (PINs, coordinates).
        match &result {
            Ok(outcome) if !outcome.ok => crate::keylog::log(
                "core",
                &format!(
                    "command {cmd} failed exit={}: {}",
                    outcome.exit_code,
                    outcome.stderr.trim()
                ),
            ),
            Err(error) => crate::keylog::log("core", &format!("command failed: {error}")),
            Ok(_) => (),
        }
        result
    }

    /// Creates (or, without `force`, reuses) the local private key and returns
    /// its PEM public key.
    pub(crate) fn generate_key(&self, force: bool) -> Result<String, OperationError> {
        let _gate = lock(&self.phone_key_gate);
        let current = lock(&self.cfg).clone();
        current.ensure_writable().map_err(OperationError::Persist)?;
        let replacing = force || !self.private_key_path().is_file();
        if replacing {
            // Persist the enrollment reset before a potentially destructive
            // key write. A failure must never leave a new key marked paired.
            let mut candidate = current;
            candidate.vin_state = VinState::Unpaired;
            candidate
                .save(&self.config_path())
                .map_err(OperationError::Persist)?;
            *lock(&self.cfg) = candidate;
            self.stop_phone_key_locked();
            // A live session holds the old private key: retire it before the
            // key file changes so nothing signs with a stale key.
            if let Some(session) = &self.session {
                session.invalidate();
            }
        }
        eprintln!("Core: generate_key(force={force})");
        crate::keys::generate(&self.private_key_path(), &self.public_key_path(), force)
            .map_err(OperationError::KeygenFailed)
    }

    /// Enrolls the current public key with the vehicle via BLE, requiring
    /// physical NFC-card approval at the center console.
    pub(crate) fn pair(&self) -> Result<(bool, String, String), HelperError> {
        let Ok(_permit) = self.ble_sem.try_lock() else {
            return Err(HelperError::Busy(
                "another BLE command is in progress".to_string(),
            ));
        };
        let gate = lock(&self.phone_key_gate);
        let session = self.session.as_ref().ok_or_else(|| {
            HelperError::SessionUnavailable(
                "pairing requires a persistent session to confirm NFC enrollment".to_string(),
            )
        })?;
        lock(&self.cfg)
            .ensure_writable()
            .map_err(|e| HelperError::SessionUnavailable(e.to_string()))?;
        self.stop_phone_key_locked();
        let (vin, connect, command) = {
            let cfg = lock(&self.cfg);
            self.ensure_ready(&cfg)?;
            (
                cfg.vin.clone(),
                cfg.connect_timeout_sec,
                cfg.command_timeout_sec,
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
        // Configuration changes and presence stop must not wait for NFC approval.
        let paired_public_key =
            crate::keys::public_bytes(&pubkey_path).map_err(HelperError::SessionUnavailable)?;
        drop(gate);

        eprintln!("Core: pair()");
        // Only the protocol library confirms vehicle enrollment.
        let outcome = session
            .pair(&pubkey_path, &vin, &self.key_string(), connect, command)
            .map_err(|e| {
                HelperError::SessionUnavailable(format!(
                    "persistent session failed during pairing: {e}"
                ))
            })?;
        if !outcome.ok {
            return Ok((false, outcome.stdout, outcome.stderr.trim().to_string()));
        }
        let gate = lock(&self.phone_key_gate);
        let mut candidate = lock(&self.cfg).clone();
        candidate
            .ensure_writable()
            .map_err(|e| HelperError::SessionUnavailable(e.to_string()))?;
        if candidate.vin != vin {
            return Err(HelperError::SessionUnavailable(
                "configuration changed during pairing; refusing to mark another VIN paired".into(),
            ));
        }
        if crate::keys::public_bytes(&pubkey_path).map_err(HelperError::SessionUnavailable)?
            != paired_public_key
        {
            return Err(HelperError::SessionUnavailable(
                "key changed during pairing; refusing to mark an unenrolled key paired".into(),
            ));
        }
        candidate.vin_state = VinState::Paired;
        candidate.save(&self.config_path()).map_err(|e| {
            HelperError::SessionUnavailable(format!(
                "paired key but could not persist phone-key state: {e}"
            ))
        })?;
        *lock(&self.cfg) = candidate;
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
        let _gate = lock(&self.phone_key_gate);
        lock(&self.cfg)
            .ensure_writable()
            .map_err(OperationError::Persist)?;
        if self.phone_key_started.load(Ordering::SeqCst)
            && self
                .session
                .as_ref()
                .is_some_and(ProtocolClient::is_presence_active)
        {
            return Ok(());
        }
        let was_started = self.phone_key_started.swap(false, Ordering::SeqCst);
        let (vin, connect, command, eligible) = {
            let cfg = lock(&self.cfg);
            (
                cfg.vin.clone(),
                cfg.connect_timeout_sec,
                cfg.command_timeout_sec,
                !cfg.vin.is_empty()
                    && cfg.vin_state == VinState::Paired
                    && self.private_key_path().is_file(),
            )
        };
        if !eligible {
            self.set_phone_key_enabled(false);
            crate::keylog::log("core", "phone-key start refused: not paired");
            return Err(OperationError::NotPaired);
        }
        let Some(session) = &self.session else {
            crate::keylog::log("core", "phone-key start refused: no session client");
            return Err(OperationError::NoSession);
        };
        if was_started && !session.is_presence_active() {
            // Retire a dead/non-presence generation before starting scanning.
            // Its pending stopped event must not restart the replacement again.
            session.invalidate();
        }
        self.set_phone_key_enabled(true);
        crate::keylog::log(
            "core",
            &format!("phone-key start vin={vin} connect={connect}s"),
        );
        match session.start_presence(&vin, &self.key_string(), connect, command) {
            Ok(()) => {
                self.phone_key_started.store(true, Ordering::SeqCst);
                *lock(&self.phone_key_retry_at) = None;
                crate::keylog::log("core", "phone-key presence started");
                Ok(())
            }
            Err(error) => {
                self.phone_key_started.store(false, Ordering::SeqCst);
                self.schedule_phone_key_retry();
                crate::keylog::log("core", &format!("phone-key start failed: {error}"));
                Err(match error {
                    SessionError::BluetoothOff(message) => OperationError::BluetoothOff(message),
                    other => OperationError::PresenceFailed(other.to_string()),
                })
            }
        }
    }

    /// Stops proximity mode and closes its live BLE connection. Idempotent.
    pub(crate) fn stop_phone_key(&self) {
        let _gate = lock(&self.phone_key_gate);
        self.stop_phone_key_locked();
    }

    fn stop_phone_key_locked(&self) {
        self.set_phone_key_enabled(false);
        *lock(&self.phone_key_retry_at) = None;
        if !self.phone_key_started.load(Ordering::SeqCst) {
            return;
        }
        crate::keylog::log("core", "phone-key stop");
        if let Some(session) = &self.session {
            if session.stop_presence().is_err() {
                session.invalidate();
            }
        }
        self.phone_key_started.store(false, Ordering::SeqCst);
    }

    pub(crate) fn phone_key_enabled(&self) -> bool {
        self.phone_key_enabled.load(Ordering::SeqCst)
    }

    fn set_phone_key_enabled(&self, enabled: bool) {
        let mut generation = lock(&self.keepalive_wakeup.0);
        self.phone_key_enabled.store(enabled, Ordering::SeqCst);
        *generation = generation.wrapping_add(1);
        self.keepalive_wakeup.1.notify_all();
    }

    /// Event observation is independent of lifecycle locks and radio I/O.
    pub(crate) fn drain_phone_key_event(&self) -> Option<SessionEvent> {
        self.session.as_ref().and_then(ProtocolClient::poll_event)
    }

    pub(crate) async fn wait_phone_key_event(&self) {
        if let Some(session) = &self.session {
            session.wait_event().await;
        } else {
            std::future::pending::<()>().await;
        }
    }

    pub(crate) fn session_generation(&self) -> u64 {
        self.session.as_ref().map_or(0, ProtocolClient::generation)
    }

    /// When a scheduled presence restart becomes due, if one is pending.
    pub(crate) fn phone_key_retry_deadline(&self) -> Option<Instant> {
        *lock(&self.phone_key_retry_at)
    }

    /// Called only by the serial lifecycle executor. Never from the UI/control
    /// event drain: restarting can wait for a complete radio round trip.
    pub(crate) fn maintain_phone_key(
        &self,
        stopped_generation: u64,
    ) -> Option<Result<(), OperationError>> {
        let gate = lock(&self.phone_key_gate);
        let stopped = stopped_generation != 0 && stopped_generation == self.session_generation();
        if stopped {
            self.phone_key_started.store(false, Ordering::SeqCst);
        }
        let due = lock(&self.phone_key_retry_at).is_none_or(|deadline| Instant::now() >= deadline);
        if self.phone_key_enabled()
            && due
            && (!self.phone_key_started.load(Ordering::SeqCst) || stopped)
        {
            if let Some(session) = &self.session {
                session.invalidate();
            }
            drop(gate);
            Some(self.start_phone_key())
        } else {
            None
        }
    }

    /// Non-blocking shutdown request (safe on the GUI thread).
    pub(crate) fn cancel_session(&self) {
        self.set_phone_key_enabled(false);
        if let Some(session) = &self.session {
            session.cancel();
        }
    }

    pub(crate) fn reap_session(&self) {
        if let Some(session) = &self.session {
            session.invalidate();
        }
    }

    fn schedule_phone_key_retry(&self) {
        *lock(&self.phone_key_retry_at) = Some(Instant::now() + PHONE_KEY_RETRY);
    }

    /// Called when the device resumes from system suspend. The `org.bluez`
    /// system-bus connection and any live GATT link are typically stale after
    /// the freezer, so retire the session and restart presence on a fresh one.
    pub(crate) fn handle_resume(&self) {
        let gate = lock(&self.phone_key_gate);
        crate::keylog::log("core", "resume from suspend - recycling BLE session");
        // Allow `start_phone_key` to claim the flag again.
        self.phone_key_started.store(false, Ordering::SeqCst);
        if let Some(session) = &self.session {
            session.clear_events();
            session.invalidate();
        }
        drop(gate);
        let _ = self.start_phone_key();
    }

    /// Test-only access to the protocol client.
    #[cfg(test)]
    pub(crate) fn protocol(&self) -> &ProtocolClient {
        self.session.as_ref().expect("protocol client")
    }

    #[cfg(test)]
    pub(crate) fn set_config(
        &self,
        vin: &str,
        model: &str,
        key_name: &str,
        connect_timeout_sec: i32,
        command_timeout_sec: i32,
    ) -> Result<(), OperationError> {
        self.set_config_with_cover_actions(
            vin,
            model,
            key_name,
            connect_timeout_sec,
            command_timeout_sec,
            None,
        )
    }

    /// Persists new settings. Only a VIN or timeout change retires the
    /// protocol session; cosmetic settings (model, key name, cover actions)
    /// never disturb a live phone-key link.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn set_config_with_cover_actions(
        &self,
        vin: &str,
        model: &str,
        key_name: &str,
        connect_timeout_sec: i32,
        command_timeout_sec: i32,
        cover_actions: Option<(Option<CoverAction>, Option<CoverAction>)>,
    ) -> Result<(), OperationError> {
        let model = crate::config::validate_config(
            vin,
            model,
            key_name,
            connect_timeout_sec,
            command_timeout_sec,
        )?;

        let gate = lock(&self.phone_key_gate);
        let current = lock(&self.cfg).clone();
        let vin_changed = current.vin != vin.trim();
        let session_changed = vin_changed
            || current.connect_timeout_sec != connect_timeout_sec
            || current.command_timeout_sec != command_timeout_sec;
        let mut candidate = current;
        candidate.vin = vin.trim().to_string();
        candidate.model = model;
        candidate.key_name = key_name.trim().to_string();
        candidate.connect_timeout_sec = connect_timeout_sec;
        candidate.command_timeout_sec = command_timeout_sec;
        if let Some((left, right)) = cover_actions {
            candidate.cover_left_action = left;
            candidate.cover_right_action = right;
        }
        if vin_changed {
            candidate.vin_state = VinState::Unpaired;
            // A different car: its name has to be read again.
            candidate.vehicle_name.clear();
        }
        // The gate serializes writers, so the save needs no cfg lock.
        candidate
            .save(&self.config_path())
            .map_err(OperationError::Persist)?;
        eprintln!(
            "Core: set_config(vin={}, model={:?}, keyName={:?}, connectTimeout={}s, commandTimeout={}s)",
            candidate.vin, candidate.model, candidate.key_name, connect_timeout_sec, command_timeout_sec
        );
        *lock(&self.cfg) = candidate;
        if !session_changed {
            return Ok(());
        }
        // Presence runs with the old VIN/timeouts: retire it.
        self.stop_phone_key_locked();
        if let Some(session) = &self.session {
            session.invalidate();
        }
        drop(gate);
        if !vin_changed {
            let _ = self.start_phone_key();
        }
        Ok(())
    }

    /// Parses shared text without sending anything, so the Navigation page
    /// can show "will send coordinates" vs "will send address" before the
    /// user confirms. Returns (kind, value1, value2): ("gps", lat, lon) or
    /// ("address", text, "").
    pub(crate) fn preview_destination(
        text: &str,
    ) -> Result<(DestinationKind, String, String), HelperError> {
        parse_shared_text(text)
            .map(|destination| destination.preview())
            .map_err(|e| HelperError::InvalidArgument(e.to_string()))
    }

    /// Sends a parsed destination to the car navigation over the live BLE
    /// session (signed field-53/field-21 actions — see
    /// `docs/navigation-share.md`). Serializes against other BLE commands.
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
        let (vin, connect, command) = self.snapshot()?;
        let session = self.session.as_ref().ok_or_else(|| {
            HelperError::SessionUnavailable("persistent BLE session is unavailable".to_string())
        })?;
        let operation = match dest {
            Destination::LatLon { lat, lon } => Operation::NavigateGps {
                latitude: lat,
                longitude: lon,
            },
            Destination::Address(address) => Operation::NavigateAddress(address),
        };
        eprintln!("Core: share_destination()");
        session
            .execute_in(
                Domain::Infotainment,
                operation,
                &vin,
                &self.key_string(),
                connect,
                command,
            )
            .map(|o| (o.ok, o.stdout, o.stderr))
            .map_err(|e| HelperError::SessionUnavailable(format!("navigation share failed: {e}")))
    }

    /// The configured VIN, without any filesystem access.
    pub(crate) fn vin(&self) -> String {
        lock(&self.cfg).vin.clone()
    }

    pub(crate) fn get_config(&self) -> GetConfigReply {
        // Snapshot config under the lock, then do filesystem I/O unlocked.
        let (vin, model, key_name, connect_timeout_sec, command_timeout_sec) = {
            let cfg = lock(&self.cfg);
            (
                cfg.vin.clone(),
                cfg.model,
                cfg.key_name.clone(),
                cfg.connect_timeout_sec,
                cfg.command_timeout_sec,
            )
        };
        // A usable key needs both halves present and non-empty.
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

    /// Persists the status-notification settings alone; a cosmetic change
    /// that never touches the phone-key session.
    pub(crate) fn set_notification(
        &self,
        enabled: bool,
        actions: &[CoverAction],
    ) -> Result<(), OperationError> {
        let _gate = lock(&self.phone_key_gate);
        let mut candidate = lock(&self.cfg).clone();
        candidate.notification_enabled = enabled;
        candidate.notification_actions = crate::config::normalize_notification_actions(actions);
        candidate
            .save(&self.config_path())
            .map_err(OperationError::Persist)?;
        *lock(&self.cfg) = candidate;
        Ok(())
    }

    /// Caches the car's own name in the config. Best effort: an unchanged
    /// or invalid name, or a failed save, keeps the previous one.
    fn remember_vehicle_name(&self, name: &str) {
        let Ok(name) = crate::config::validate_vehicle_name(name) else {
            return;
        };
        let _gate = lock(&self.phone_key_gate);
        let mut candidate = lock(&self.cfg).clone();
        if candidate.vehicle_name == name {
            return;
        }
        candidate.vehicle_name = name.to_string();
        match candidate.save(&self.config_path()) {
            Ok(()) => *lock(&self.cfg) = candidate,
            Err(error) => {
                crate::keylog::log("core", &format!("vehicle name not saved: {error}"));
            }
        }
    }

    #[cfg(feature = "runtime")]
    pub(crate) fn vehicle_name(&self) -> String {
        lock(&self.cfg).vehicle_name.clone()
    }

    #[cfg(feature = "runtime")]
    pub(crate) fn notification_settings(&self) -> (bool, Vec<CoverAction>) {
        let cfg = lock(&self.cfg);
        (cfg.notification_enabled, cfg.notification_actions.clone())
    }

    #[cfg(feature = "runtime")]
    pub(crate) fn cover_actions(&self) -> (CoverAction, CoverAction) {
        let cfg = lock(&self.cfg);
        (
            cfg.cover_left_action.unwrap_or(CoverAction::FrunkOpen),
            cfg.cover_right_action.unwrap_or(CoverAction::TrunkOpen),
        )
    }
}

impl Drop for Core {
    fn drop(&mut self) {
        // The runtime explicitly cancels/stops workers before final destruction.
        // Invalidation is the lifetime backstop.
        self.set_phone_key_enabled(false);
        *lock(&self.phone_key_retry_at) = None;
        self.phone_key_started.store(false, Ordering::SeqCst);
        if let Some(session) = &self.session {
            session.invalidate();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Core;
    use crate::config::{Config, VinState};
    use crate::protocol_client::ProtocolClient;
    use crate::share::DestinationKind;
    use crate::tesla_ble::fake;
    use std::sync::atomic::Ordering;
    use std::time::{Duration, Instant};

    /// A paired-or-not core on the production protocol client over the fake
    /// Go library, with a real key pair on disk.
    fn fake_core(dir: &std::path::Path, vin: &str, state: VinState) -> Core {
        Config {
            vin: vin.into(),
            vin_state: state,
            ..Config::default()
        }
        .save(&dir.join("config.json"))
        .unwrap();
        crate::keys::generate(
            &dir.join("private_key.pem"),
            &dir.join("public_key.pem"),
            false,
        )
        .unwrap();
        Core::new(dir, Some(ProtocolClient::with_library(fake::library()))).unwrap()
    }

    #[test]
    fn renaming_the_car_caches_the_trimmed_name_and_rejects_bad_names() {
        let dir = tempfile::tempdir().unwrap();
        let vin = "5YJ3E1EA0PF100009";
        let core = fake_core(dir.path(), vin, VinState::Paired);
        let outcome = core.run("set-vehicle-name", &["  Eel  ".into()]).unwrap();
        assert!(outcome.ok, "{outcome:?}");
        assert_eq!(core.cfg.lock().unwrap().vehicle_name, "Eel");
        assert_eq!(
            Config::load(&dir.path().join("config.json"))
                .unwrap()
                .vehicle_name,
            "Eel"
        );
        // Invalid names never reach the car or the config.
        assert!(core.run("set-vehicle-name", &["   ".into()]).is_err());
        assert_eq!(core.cfg.lock().unwrap().vehicle_name, "Eel");
    }

    fn failing_watch(vin: &str) {
        fake::script(
            vin,
            fake::Script {
                statuses: [(fake::WATCH_START, (fake::FAILED, "radio failure".into()))].into(),
                ..fake::Script::default()
            },
        );
    }

    fn wait_for_event(
        core: &Core,
        kind: crate::phone_key::PresenceEvent,
    ) -> crate::protocol_client::SessionEvent {
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            if let Some(event) = core.drain_phone_key_event() {
                if event.kind == kind {
                    return event;
                }
            }
            assert!(Instant::now() < deadline, "no {kind} event");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn stale_stopped_generation_cannot_retire_a_replacement_session() {
        let dir = tempfile::tempdir().unwrap();
        let core = fake_core(dir.path(), "5YJ3E1EA0PF100001", VinState::Paired);
        core.start_phone_key().unwrap();
        let generation = core.session_generation();
        core.session.as_ref().unwrap().invalidate();
        // The replacement session (a new generation) is started.
        core.start_phone_key().unwrap();
        let current = core.session_generation();
        assert!(current > generation);
        assert!(core.maintain_phone_key(generation).is_none());
        assert_eq!(core.session_generation(), current);
        assert!(core.session.as_ref().unwrap().is_presence_active());
        assert!(core.phone_key_started.load(Ordering::SeqCst));
        core.stop_phone_key();
    }

    #[test]
    fn concurrent_phone_key_starts_share_one_confirmed_service() {
        use std::sync::{Arc, Barrier};
        let dir = tempfile::tempdir().unwrap();
        let vin = "5YJ3E1EA0PF100002";
        let core = Arc::new(fake_core(dir.path(), vin, VinState::Paired));
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
        let starts = fake::submitted(vin)
            .into_iter()
            .filter(|operation| *operation == fake::WATCH_START)
            .count();
        assert_eq!(starts, 1, "concurrent starts created duplicate services");
        core.stop_phone_key();
    }

    #[test]
    fn phone_key_loss_restarts_and_failed_retries_keep_the_lease_without_spinning() {
        let dir = tempfile::tempdir().unwrap();
        let vin = "5YJ3E1EA0PF100003";
        let core = fake_core(dir.path(), vin, VinState::Paired);
        core.start_phone_key().unwrap();
        // The watcher fails: the controller reports a terminal stop.
        fake::observe(vin, fake::ObservationKind::WatchFailed, fake::FAILED);
        let stopped = wait_for_event(&core, crate::phone_key::PresenceEvent::Stopped);
        assert_eq!(stopped.vin, vin);
        let generation = core.session_generation();
        assert!(core.maintain_phone_key(stopped.generation).unwrap().is_ok());
        assert!(core.session_generation() > generation);
        assert!(core.phone_key_started.load(Ordering::SeqCst));

        // A second loss whose restart fails keeps lease intent and backs off.
        failing_watch(vin);
        fake::observe(vin, fake::ObservationKind::WatchFailed, fake::FAILED);
        let stopped = wait_for_event(&core, crate::phone_key::PresenceEvent::Stopped);
        assert!(core
            .maintain_phone_key(stopped.generation)
            .unwrap()
            .is_err());
        assert!(!core.phone_key_started.load(Ordering::SeqCst));
        assert!(
            core.phone_key_enabled(),
            "retry must retain the CPU lease intent"
        );
        assert!(core.phone_key_retry_deadline().is_some());
        assert!(
            core.maintain_phone_key(0).is_none(),
            "a failed start must not spin before its retry deadline"
        );
        *core.phone_key_retry_at.lock().unwrap() = Some(Instant::now());
        assert!(core.maintain_phone_key(0).unwrap().is_err());
        assert!(
            core.phone_key_enabled(),
            "a failed retry must retain the CPU lease intent"
        );
        core.stop_phone_key();
        assert!(!core.phone_key_enabled());
        assert!(core.phone_key_retry_deadline().is_none());
    }

    #[test]
    fn failed_initial_start_retains_cpu_lease_intent_until_stop() {
        let dir = tempfile::tempdir().unwrap();
        let vin = "5YJ3E1EA0PF100004";
        failing_watch(vin);
        let core = fake_core(dir.path(), vin, VinState::Paired);
        assert!(core.start_phone_key().is_err());
        assert!(
            core.phone_key_enabled(),
            "failed startup must keep the phone awake for retry"
        );
        core.stop_phone_key();
        assert!(
            !core.phone_key_enabled(),
            "an explicit stop must release the lease intent"
        );
    }

    #[test]
    fn schedules_with_signed_coordinates_and_end_only_time_are_sent() {
        let dir = tempfile::tempdir().unwrap();
        let core = fake_core(dir.path(), "5YJ3E1EA0PF100005", VinState::Unpaired);
        for args in [
            ["all", "22:00-06:00", "-33.8688", "151.2093"],
            ["all", "22:00-06:00", "37.7749", "-122.4194"],
            ["all", "-06:00", "48.8584", "2.2945"],
        ] {
            let args = args.map(str::to_string);
            let result = core.run("charging-schedule-add", &args);
            assert!(
                result.as_ref().is_ok_and(|outcome| outcome.ok),
                "valid schedule {args:?} rejected: {result:?}"
            );
        }
    }

    #[test]
    fn out_of_range_arguments_are_rejected_before_the_radio() {
        let dir = tempfile::tempdir().unwrap();
        let vin = "5YJ3E1EA0PF100006";
        let core = fake_core(dir.path(), vin, VinState::Unpaired);
        assert!(matches!(
            core.run("charging-set-limit", &["101".into()]),
            Err(crate::error::HelperError::InvalidArgument(_))
        ));
        assert_eq!(fake::submitted(vin), Vec::<u32>::new());
    }

    // Regression: generate_key used to lock cfg twice on the same thread.
    #[test]
    fn test_generate_key_does_not_deadlock() {
        let dir = tempfile::tempdir().unwrap();
        let core = Core::new(dir.path(), None).unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(core.generate_key(false));
        });
        let pem = rx
            .recv_timeout(Duration::from_secs(5))
            .expect("generate_key did not return within 5s")
            .unwrap();
        assert!(pem.starts_with("-----BEGIN PUBLIC KEY-----"));
    }

    #[test]
    fn test_generate_key_reuse_keeps_pairing() {
        let dir = tempfile::tempdir().unwrap();
        let core = fake_core(dir.path(), "5YJ3E1EA0PF100007", VinState::Paired);
        let existing = std::fs::read_to_string(core.public_key_path()).unwrap();
        let pubkey = core
            .generate_key(false)
            .expect("reuse must preserve the existing enrollment");
        assert_eq!(pubkey, existing.trim());
        let cfg = Config::load(&dir.path().join("config.json")).unwrap();
        assert_eq!(cfg.vin_state, VinState::Paired);
        // Forcing a new key resets the enrollment.
        let replaced = core.generate_key(true).unwrap();
        assert_ne!(replaced, pubkey);
        let cfg = Config::load(&dir.path().join("config.json")).unwrap();
        assert_eq!(cfg.vin_state, VinState::Unpaired);
    }

    #[test]
    fn unknown_config_schema_refuses_settings_keys_and_pairing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        let original = r#"{"version":99,"vin":"5YJ3E1EA0PF000000","model":"","key_name":"phone","connect_timeout_sec":20,"command_timeout_sec":5,"vin_state":"paired","future_setting":true}"#;
        std::fs::write(&path, original).unwrap();
        let state_dir = dir.path().to_string_lossy().into_owned();
        let core = Core::new(state_dir, None).unwrap();
        assert!(core
            .set_config("5YJ3E1EA0PF111111", "", "phone", 20, 5)
            .is_err());
        assert!(core.generate_key(true).is_err());
        assert!(core.pair().is_err());
        assert!(matches!(
            core.start_phone_key(),
            Err(crate::error::OperationError::Persist(_))
        ));
        assert_eq!(std::fs::read_to_string(path).unwrap(), original);
        assert!(!core.private_key_path().exists());
    }

    #[test]
    fn review_vin_change_stays_unpaired_after_restart() {
        let dir = tempfile::tempdir().unwrap();
        let state_dir = dir.path().to_string_lossy().into_owned();
        let core = Core::new(state_dir.clone(), None).unwrap();
        core.set_config("5YJ3E1EA0PF000000", "", "phone", 20, 5)
            .unwrap();
        // A previous vehicle's key files remain when Settings changes VIN.
        std::fs::write(core.private_key_path(), "previous vehicle private key").unwrap();
        std::fs::write(core.public_key_path(), "previous vehicle public key").unwrap();
        core.set_config("5YJ3E1EA0PF111111", "", "phone", 20, 5)
            .unwrap();
        drop(core);

        let reopened = Core::new(state_dir, None).unwrap();
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
        let _core = Core::new(state_dir, None).unwrap();
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
        let core = Core::new(state_dir, None).unwrap();
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
    fn test_preview_destination_vectors() {
        let (kind, v1, v2) = Core::preview_destination("geo:48.8584,2.2945").unwrap();
        assert_eq!(
            (kind, v1.as_str(), v2.as_str()),
            (DestinationKind::Gps, "48.8584", "2.2945")
        );
        let (kind, v1, _) = Core::preview_destination("1600 Amphitheatre Parkway").unwrap();
        assert_eq!(kind, DestinationKind::Address);
        assert_eq!(v1, "1600 Amphitheatre Parkway");
        assert!(Core::preview_destination("").is_err());
        assert!(Core::preview_destination("999,999").is_err());
    }

    #[test]
    fn test_share_destination_needs_vin_and_session() {
        let dir = tempfile::tempdir().unwrap();
        let core = Core::new(dir.path(), None).unwrap();
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
        let core = Core::new(dir.path(), None).unwrap();
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
        let core = Core::new(dir.path(), None).unwrap();
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
        let core = Core::new(dir.path(), None).unwrap();
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
        let core = Core::new(dir.path(), None).unwrap();
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
    fn test_get_config_has_key_requires_usable_keypair() {
        // get_config reports has_key from public_key.pem alone, even when it
        // is empty or private_key.pem is missing. The UI gates every command
        // on has_key, so a half-present key shows "Key ready" and then every
        // run() fails with NoKey. has_key must require both files non-empty.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("private_key.pem"), "private").unwrap();
        std::fs::write(dir.path().join("public_key.pem"), "").unwrap();
        let core = Core::new(dir.path(), None).unwrap();
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
}
