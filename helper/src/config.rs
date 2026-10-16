use std::fmt;
use std::fs;
use std::io::{self, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::Path;
use std::sync::LazyLock;

use crate::cover_action::{self, CoverAction};
use regex::Regex;
use serde::{Deserialize, Serialize};
use tempfile::NamedTempFile;

pub(crate) const MAX_TIMEOUT_SEC: i32 = 300;
pub(crate) const MAX_KEY_NAME_LEN: usize = 64;
/// Longest accepted vehicle name, in characters.
pub(crate) const MAX_VEHICLE_NAME_LEN: usize = 64;
/// Most quick actions the status notification offers.
pub(crate) const MAX_NOTIFICATION_ACTIONS: usize = 3;
/// Notification quick actions before the user chooses any.
pub(crate) const DEFAULT_NOTIFICATION_ACTIONS: [CoverAction; 2] =
    [CoverAction::Lock, CoverAction::Unlock];

/// Keeps the first occurrence of each action, at most
/// `MAX_NOTIFICATION_ACTIONS` of them.
pub(crate) fn normalize_notification_actions(actions: &[CoverAction]) -> Vec<CoverAction> {
    let mut unique = Vec::new();
    for action in actions {
        if !unique.contains(action) && unique.len() < MAX_NOTIFICATION_ACTIONS {
            unique.push(*action);
        }
    }
    unique
}

/// Trims a vehicle name and checks it is 1..=`MAX_VEHICLE_NAME_LEN`
/// characters without control characters.
pub(crate) fn validate_vehicle_name(name: &str) -> Result<&str, String> {
    let name = name.trim();
    if name.is_empty() || name.chars().count() > MAX_VEHICLE_NAME_LEN {
        return Err(format!("name must be 1..{MAX_VEHICLE_NAME_LEN} characters"));
    }
    if name.chars().any(char::is_control) {
        return Err("name must not contain control characters".into());
    }
    Ok(name)
}

/// The accepted values of Config.model. `Auto` ("") is "Auto (from VIN)":
/// the QML client guesses the model from the VIN's WMI prefix and nothing is
/// forced. Every other name doubles as a `Model` id in the client-side MODELS
/// list (app/qml/js/VehicleState.js) and a key into the model images the
/// front page shows. Keep this, VehicleState.js's MODELS, and that file's
/// VIN-prefix `guessModel()` table in sync when adding/removing models.
#[derive(
    Debug,
    Clone,
    Copy,
    Default,
    PartialEq,
    Eq,
    Serialize,
    strum::EnumString,
    strum::IntoStaticStr,
    strum::VariantArray,
)]
#[serde(rename_all = "lowercase")]
#[strum(serialize_all = "lowercase", ascii_case_insensitive)]
pub(crate) enum Model {
    #[default]
    #[serde(rename = "")]
    #[strum(serialize = "")]
    Auto,
    Model3,
    Models,
    Modelx,
    Modely,
    Cybertruck,
}

impl Model {
    /// Parses a user/file value, ignoring surrounding whitespace and case.
    pub(crate) fn parse(text: &str) -> Result<Self, ConfigError> {
        text.trim().parse().map_err(|_| ConfigError::InvalidModel)
    }
}

static VIN_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[A-HJ-NPR-Z0-9]{17}$").unwrap());
// key_name is a display-only label (the protocol library never uses it) -
// this only keeps an unbounded/control-character string out of config.json
// and the set_config log line.
static KEY_NAME_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[A-Za-z0-9 ._-]*$").unwrap());

/// Validation failure for a `SetConfig` payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ConfigError {
    PositiveTimeout,
    MaxTimeout,
    InvalidVin,
    InvalidKeyName,
    InvalidModel,
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ConfigError::PositiveTimeout => write!(f, "timeouts must be positive"),
            ConfigError::MaxTimeout => write!(f, "timeouts must be <= {MAX_TIMEOUT_SEC}"),
            ConfigError::InvalidVin => {
                write!(f, "invalid VIN format (17 alphanumeric chars, no I/O/Q)")
            }
            ConfigError::InvalidKeyName => write!(
                f,
                "key name must be <= {MAX_KEY_NAME_LEN} chars (letters, digits, spaces, . _ -)"
            ),
            ConfigError::InvalidModel => write!(
                f,
                "model must be one of {}",
                <Model as strum::VariantArray>::VARIANTS
                    .iter()
                    .filter(|m| **m != Model::Auto)
                    .map(|m| <&str>::from(*m))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        }
    }
}

impl std::error::Error for ConfigError {}

/// Schema version of `config.json`, so a future field change can migrate old
/// files instead of guessing. `Core::new` runs any pending migrations for
/// `version < CURRENT` and then rewrites the file at `CURRENT`. Serialized as
/// its numeric index; an out-of-range value (a newer build's config) is kept
/// verbatim in `Unknown` so this build never rewrites a file it doesn't
/// understand.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default, Serialize, Deserialize)]
#[serde(from = "u8", into = "u8")]
pub(crate) enum ConfigVersion {
    /// Pre-phone-key: no pairing state at all.
    #[default]
    V0,
    /// Phone-key era: `paired_vin: Option<String>`.
    V1,
    /// Current: `vin_state: VinState` (and the `version` key itself).
    V2,
    /// A version newer than this build knows; carried verbatim so the file
    /// is never downgraded.
    Unknown(u8),
}

impl ConfigVersion {
    pub(crate) const CURRENT: Self = Self::V2;
}

impl From<u8> for ConfigVersion {
    fn from(v: u8) -> Self {
        match v {
            0 => Self::V0,
            1 => Self::V1,
            2 => Self::V2,
            v => Self::Unknown(v),
        }
    }
}

impl From<ConfigVersion> for u8 {
    fn from(v: ConfigVersion) -> u8 {
        match v {
            ConfigVersion::V0 => 0,
            ConfigVersion::V1 => 1,
            ConfigVersion::V2 => 2,
            ConfigVersion::Unknown(v) => v,
        }
    }
}

/// Pairing state of the current VIN's local key. `Paired` means the key is
/// eligible for automatic phone-key presence (NFC enrollment completed, or
/// a pre-phone-key V0 config was migrated); explicit `Unpaired` state is
/// preserved across restarts. This replaces the
/// old `paired_vin: Option<String>` field, which stored a duplicate copy of
/// the VIN just to say "this VIN is paired".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum VinState {
    #[default]
    Unpaired,
    Paired,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct Config {
    /// Schema version of this file. `Core::new` migrates older files up to
    /// `ConfigVersion::CURRENT` and rewrites them at that version.
    #[serde(default)]
    pub version: ConfigVersion,
    pub vin: String,
    // #[serde(default)]: config.json files written before this field existed
    // (anything pre-0.1.6) have no "model" key. Without a default, serde
    // treats that as a missing required field and Config::load() rejects
    // the whole file, silently falling back to Config::default() - which
    // wipes the already-configured VIN too, not
    // just the model. sanitize() still normalizes/validates whatever comes
    // out of this either way.
    #[serde(default)]
    pub model: Model,
    pub key_name: String,
    pub connect_timeout_sec: i32,
    pub command_timeout_sec: i32,
    pub cover_left_action: Option<CoverAction>,
    pub cover_right_action: Option<CoverAction>,
    /// Whether the current VIN's key has completed NFC pairing.
    /// `Unpaired` is the serde default; older schema versions that predate
    /// this field are migrated by `Core::new`.
    #[serde(default)]
    pub vin_state: VinState,
    /// The name the owner gave the car, as last read from (or written to)
    /// the vehicle. Empty until the car reported one.
    #[serde(default)]
    pub vehicle_name: String,
    /// Whether the persistent status notification is shown.
    #[serde(default)]
    pub notification_enabled: bool,
    /// Quick actions offered on the status notification, in order.
    #[serde(default)]
    pub notification_actions: Vec<CoverAction>,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            version: ConfigVersion::CURRENT,
            vin: String::new(),
            model: Model::Auto,
            key_name: "harbour-electric-eel".to_string(),
            connect_timeout_sec: 20,
            command_timeout_sec: 5,
            cover_left_action: None,
            cover_right_action: None,
            vin_state: VinState::Unpaired,
            vehicle_name: String::new(),
            notification_enabled: false,
            notification_actions: DEFAULT_NOTIFICATION_ACTIONS.to_vec(),
        }
    }
}

/// Deserializes a config, translating legacy schema versions into the
/// current fields. The `version` key wins when present; otherwise it's
/// inferred from which fields the file carries:
/// - `vin_state` present -> the file is already current-shaped (V2);
/// - `paired_vin` present -> the phone-key era (V1): a value equal to the
///   configured VIN means paired, anything else unpaired;
/// - neither -> pre-phone-key (V0), left `Unpaired` for `Core::new` to
///   decide from key-file presence whether a working key is on disk.
impl<'de> Deserialize<'de> for Config {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        // Null is an explicit default value; absence identifies legacy schemas.
        #[derive(Default)]
        enum EnrollmentField<T> {
            #[default]
            Missing,
            Present(T),
        }

        fn present_optional<'de, D, T>(deserializer: D) -> Result<EnrollmentField<T>, D::Error>
        where
            D: serde::Deserializer<'de>,
            T: Deserialize<'de> + Default,
        {
            Option::<T>::deserialize(deserializer)
                .map(|value| EnrollmentField::Present(value.unwrap_or_default()))
        }

        #[derive(Deserialize)]
        struct LegacyConfig {
            #[serde(default)]
            version: Option<ConfigVersion>,
            vin: String,
            #[serde(default)]
            model: String,
            key_name: String,
            connect_timeout_sec: i32,
            command_timeout_sec: i32,
            #[serde(default, deserialize_with = "cover_action::deserialize_optional")]
            cover_left_action: Option<CoverAction>,
            #[serde(default, deserialize_with = "cover_action::deserialize_optional")]
            cover_right_action: Option<CoverAction>,
            #[serde(default, deserialize_with = "present_optional")]
            vin_state: EnrollmentField<VinState>,
            #[serde(default, deserialize_with = "present_optional")]
            paired_vin: EnrollmentField<String>,
            #[serde(default)]
            vehicle_name: String,
            #[serde(default)]
            notification_enabled: bool,
            #[serde(default, deserialize_with = "cover_action::deserialize_list")]
            notification_actions: Option<Vec<CoverAction>>,
        }

        let raw = LegacyConfig::deserialize(deserializer)?;
        let (vin_state, version) = if let EnrollmentField::Present(state) = raw.vin_state {
            (state, raw.version.unwrap_or(ConfigVersion::V2))
        } else if let EnrollmentField::Present(v) = raw.paired_vin {
            let state = if !v.is_empty() && v == raw.vin {
                VinState::Paired
            } else {
                VinState::Unpaired
            };
            (state, raw.version.unwrap_or(ConfigVersion::V1))
        } else {
            (VinState::Unpaired, raw.version.unwrap_or(ConfigVersion::V0))
        };
        Ok(Config {
            version,
            vin: raw.vin,
            model: Model::parse(&raw.model).unwrap_or_else(|_| {
                eprintln!("electric-eel: config.json model fails validation, resetting to default");
                Model::default()
            }),
            key_name: raw.key_name,
            connect_timeout_sec: raw.connect_timeout_sec,
            command_timeout_sec: raw.command_timeout_sec,
            cover_left_action: raw.cover_left_action,
            cover_right_action: raw.cover_right_action,
            vin_state,
            vehicle_name: raw.vehicle_name,
            notification_enabled: raw.notification_enabled,
            notification_actions: raw.notification_actions.map_or_else(
                || DEFAULT_NOTIFICATION_ACTIONS.to_vec(),
                |actions| normalize_notification_actions(&actions),
            ),
        })
    }
}

impl Config {
    /// Reads the config, returning an I/O error if the file exists but can't
    /// be read. A missing file is the caller's signal to fall back to
    /// [`Config::default`]; an unparseable file is backed up alongside the
    /// original (`.corrupt-<timestamp>`) and defaults too, so a truncated
    /// write never silently discards the VIN without forensic evidence.
    pub(crate) fn load(path: &Path) -> io::Result<Config> {
        let data = match fs::read(path) {
            Ok(d) => d,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Config::default()),
            Err(e) => return Err(e),
        };
        let mut cfg: Config = match serde_json::from_slice(&data) {
            Ok(cfg) => cfg,
            Err(e) => {
                // A future schema may change required fields or enum values.
                // Treat it as unsupported, not corrupt: defaulting would allow
                // this build's next save to overwrite a perfectly valid file.
                #[derive(Deserialize)]
                struct VersionHeader {
                    version: Option<u64>,
                }
                let version = serde_json::from_slice::<VersionHeader>(&data)
                    .ok()
                    .and_then(|header| header.version);
                if version
                    .is_some_and(|version| version > u64::from(u8::from(ConfigVersion::CURRENT)))
                {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("unsupported newer config schema: {e}"),
                    ));
                }
                eprintln!(
                    "electric-eel: ignoring unparseable config {}: {}",
                    path.display(),
                    e
                );
                backup_corrupt_config(path, &data);
                return Ok(Config::default());
            }
        };
        cfg.sanitize();
        Ok(cfg)
    }

    /// Defense-in-depth against a hand-edited or otherwise corrupted
    /// config.json - the write path (`SetConfig` -> `validate_config`)
    /// already rejects all of this, so the only way a bad value gets here
    /// is editing the file directly. That's not a real attack surface
    /// (0600, owned by the service's own account - see the RPM spec), but
    /// an out-of-range Duration or a stray control character still
    /// shouldn't flow straight into a subprocess argv unexamined. Resets
    /// only the offending field(s) to their defaults rather than
    /// discarding the whole config, so one bad field doesn't also cost
    /// the VIN.
    fn sanitize(&mut self) {
        let default = Config::default();
        if self.connect_timeout_sec <= 0 || self.connect_timeout_sec > MAX_TIMEOUT_SEC {
            eprintln!(
                "electric-eel: config.json connect_timeout_sec={} out of range, resetting to default",
                self.connect_timeout_sec
            );
            self.connect_timeout_sec = default.connect_timeout_sec;
        }
        if self.command_timeout_sec <= 0 || self.command_timeout_sec > MAX_TIMEOUT_SEC {
            eprintln!(
                "electric-eel: config.json command_timeout_sec={} out of range, resetting to default",
                self.command_timeout_sec
            );
            self.command_timeout_sec = default.command_timeout_sec;
        }
        let vin_trimmed = self.vin.trim().to_string();
        if !vin_trimmed.is_empty() && !VIN_RE.is_match(&vin_trimmed) {
            eprintln!("electric-eel: config.json vin fails validation, clearing");
            self.vin = String::new();
        } else {
            self.vin = vin_trimmed;
        }
        let key_name_trimmed = self.key_name.trim().to_string();
        if key_name_trimmed.len() > MAX_KEY_NAME_LEN || !KEY_NAME_RE.is_match(&key_name_trimmed) {
            eprintln!("electric-eel: config.json key_name fails validation, resetting to default");
            self.key_name = default.key_name;
        } else {
            self.key_name = key_name_trimmed;
        }
    }

    /// Writes to a `.tmp` sibling and renames into place, so a crash mid-write
    /// can't truncate/zero the file and silently reset the VIN/key/timeouts.
    /// Both the temp file's contents (fsync before rename) and the parent
    /// directory entry (fsync after rename) are flushed to disk, so a power
    /// loss right after the rename can't lose the new config either.
    pub(crate) fn save(&self, path: &Path) -> io::Result<()> {
        self.ensure_writable()?;
        let data = serde_json::to_vec_pretty(self)?;
        write_atomic(path, &data)
    }

    pub(crate) fn ensure_writable(&self) -> io::Result<()> {
        if let ConfigVersion::Unknown(version) = self.version {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("config schema {version} is newer than this build; update the app before changing settings or keys"),
            ));
        }
        Ok(())
    }
}

/// Durable owner-only replacement shared by configuration and public-key writes.
pub(crate) fn write_atomic(path: &Path, data: &[u8]) -> io::Result<()> {
    // A bare file name has the empty parent "", which can't be opened for the
    // directory fsync; it means the current directory.
    let dir = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let mut tmp = NamedTempFile::new_in(dir)?;
    tmp.as_file()
        .set_permissions(fs::Permissions::from_mode(0o600))?;
    tmp.write_all(data)?;
    tmp.as_file().sync_all()?;
    tmp.persist(path)?;
    fs::File::open(dir)?.sync_all()
}

/// Preserve an unparseable config for forensics instead of silently dropping
/// it. Best-effort: failures are logged and ignored so a backup problem can
/// never turn a recoverable corrupt config into a hard startup failure.
fn backup_corrupt_config(path: &Path, data: &[u8]) {
    use std::time::{SystemTime, UNIX_EPOCH};
    // Nanosecond stamp + pid: two failures within the same second (or two
    // processes) must not overwrite each other's evidence.
    let (secs, nanos) = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or((0, 0), |d| (d.as_secs(), d.subsec_nanos()));
    let backup = path.with_extension(format!(
        "corrupt-{secs}-{nanos:09}-{}.json",
        std::process::id()
    ));
    // Create with 0600 atomically (no world-readable window) and fail if the
    // name exists instead of overwriting.
    let written = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&backup)
        .and_then(|mut file| file.write_all(data));
    if let Err(e) = written {
        eprintln!(
            "electric-eel: could not back up corrupt config {}: {}",
            backup.display(),
            e
        );
        return;
    }
    eprintln!(
        "electric-eel: backed up unparseable config to {}",
        backup.display()
    );
    prune_corrupt_backups(path);
}

/// Keep only the 5 most recent corrupt backups: `load()` leaves the corrupt
/// file in place (so a newer-schema file is never destroyed), which means
/// every boot would otherwise add another `config.corrupt-*.json`.
fn prune_corrupt_backups(path: &Path) {
    let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
        return;
    };
    let prefix = format!("{stem}.corrupt-");
    let Some(parent) = path.parent() else { return };
    let Ok(entries) = std::fs::read_dir(parent) else {
        return;
    };
    let mut backups: Vec<_> = entries
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| {
            let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
            name.starts_with(&prefix)
                && p.extension()
                    .is_some_and(|e| e.eq_ignore_ascii_case("json"))
        })
        .collect();
    // Parse timestamps so existing, unpadded backup names sort correctly too.
    backups.sort_by_key(|p| {
        let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
        let mut fields = name
            .trim_start_matches(prefix.as_str())
            .trim_end_matches(".json")
            .split('-');
        let secs = fields
            .next()
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(0);
        let nanos = fields
            .next()
            .and_then(|s| s.parse::<u32>().ok())
            .unwrap_or(0);
        let pid = fields
            .next()
            .and_then(|s| s.parse::<u32>().ok())
            .unwrap_or(0);
        (secs, nanos, pid)
    });
    while backups.len() > 5 {
        if let Some(oldest) = backups.first() {
            let _ = std::fs::remove_file(oldest);
        }
        backups.remove(0);
    }
}

/// Returns the parsed model if the inputs are acceptable, else a
/// human-readable error. Shared by `Core::set_config_with_cover_actions` and
/// unit tests.
pub(crate) fn validate_config(
    vin: &str,
    model: &str,
    key_name: &str,
    connect_timeout: i32,
    command_timeout: i32,
) -> Result<Model, ConfigError> {
    if connect_timeout <= 0 || command_timeout <= 0 {
        return Err(ConfigError::PositiveTimeout);
    }
    if connect_timeout > MAX_TIMEOUT_SEC || command_timeout > MAX_TIMEOUT_SEC {
        return Err(ConfigError::MaxTimeout);
    }
    let vin = vin.trim();
    if !vin.is_empty() && !VIN_RE.is_match(vin) {
        return Err(ConfigError::InvalidVin);
    }
    let model = Model::parse(model)?;
    let key_name = key_name.trim();
    if key_name.len() > MAX_KEY_NAME_LEN || !KEY_NAME_RE.is_match(key_name) {
        return Err(ConfigError::InvalidKeyName);
    }
    Ok(model)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn notification_actions_load_leniently_and_default_when_absent() {
        let base =
            r#""vin":"","key_name":"phone","connect_timeout_sec":20,"command_timeout_sec":5"#;
        let cfg: Config = serde_json::from_str(&format!("{{{base}}}")).unwrap();
        assert!(!cfg.notification_enabled);
        assert_eq!(cfg.notification_actions, DEFAULT_NOTIFICATION_ACTIONS);
        assert_eq!(cfg.vehicle_name, "");
        let cfg: Config = serde_json::from_str(&format!(
            r#"{{{base},"notification_actions":["honk","bogus",7,"honk","lock","unlock","trunk-open"]}}"#
        ))
        .unwrap();
        assert_eq!(
            cfg.notification_actions,
            [CoverAction::Honk, CoverAction::Lock, CoverAction::Unlock]
        );
        let cfg: Config =
            serde_json::from_str(&format!(r#"{{{base},"notification_actions":"honk"}}"#)).unwrap();
        assert_eq!(cfg.notification_actions, DEFAULT_NOTIFICATION_ACTIONS);
    }

    #[test]
    fn vehicle_names_are_trimmed_and_bounded() {
        assert_eq!(validate_vehicle_name("  Eel  "), Ok("Eel"));
        assert!(validate_vehicle_name("   ").is_err());
        assert!(validate_vehicle_name("a\nb").is_err());
        assert!(validate_vehicle_name(&"é".repeat(MAX_VEHICLE_NAME_LEN)).is_ok());
        assert!(validate_vehicle_name(&"é".repeat(MAX_VEHICLE_NAME_LEN + 1)).is_err());
    }

    #[test]
    fn model_parses_case_insensitively_and_serializes_lowercase() {
        assert_eq!(Model::parse(" ModelY ").unwrap(), Model::Modely);
        assert_eq!(Model::parse("").unwrap(), Model::Auto);
        assert_eq!(Model::parse("roadster"), Err(ConfigError::InvalidModel));
        assert_eq!(serde_json::to_value(Model::Auto).unwrap(), "");
        assert_eq!(serde_json::to_value(Model::Model3).unwrap(), "model3");
        assert_eq!(
            ConfigError::InvalidModel.to_string(),
            "model must be one of model3, models, modelx, modely, cybertruck"
        );
    }

    #[test]
    fn explicit_null_enrollment_is_not_misclassified_as_legacy_v0() {
        for (field, version) in [
            (r#""paired_vin":null"#, ConfigVersion::V1),
            (r#""vin_state":null"#, ConfigVersion::V2),
        ] {
            let json = format!(
                r#"{{"vin":"5YJ3E1EA0PF000000","key_name":"phone","connect_timeout_sec":20,"command_timeout_sec":5,{field}}}"#
            );
            let config: Config = serde_json::from_str(&json).unwrap();
            assert_eq!(config.version, version);
            assert_eq!(config.vin_state, VinState::Unpaired);
        }
    }

    #[test]
    fn unknown_schema_save_is_refused_without_modifying_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        let original = b"future config evidence";
        fs::write(&path, original).unwrap();
        let config = Config {
            version: ConfigVersion::Unknown(99),
            ..Config::default()
        };
        assert!(config.save(&path).is_err());
        assert_eq!(fs::read(path).unwrap(), original);
    }

    #[test]
    fn unknown_schema_with_new_fields_is_not_treated_as_corrupt() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        let original = r#"{"version":99,"vin_state":"new_enrollment_state"}"#;
        fs::write(&path, original).unwrap();
        assert!(Config::load(&path).is_err());
        assert_eq!(fs::read_to_string(&path).unwrap(), original);
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    // (name, vin, model, key_name, connect_timeout, command_timeout, want)
    type ValidateConfigCase<'a> = (
        &'a str,
        &'a str,
        &'a str,
        &'a str,
        i32,
        i32,
        Option<ConfigError>,
    );

    #[test]
    // One long, flat table of cases reads more clearly here than splitting
    // into several shorter test functions that would each re-establish the
    // same "all valid except one field" setup.
    #[allow(clippy::too_many_lines)]
    fn test_validate_config() {
        let valid_vin = "5YJ3E1EA0PF000000";
        let default_key_name = "harbour-electric-eel";
        let cases: &[ValidateConfigCase] = &[
            ("all valid", valid_vin, "", default_key_name, 20, 5, None),
            (
                "all valid with model override",
                valid_vin,
                "modely",
                default_key_name,
                20,
                5,
                None,
            ),
            (
                "zero connect timeout",
                valid_vin,
                "",
                default_key_name,
                0,
                5,
                Some(ConfigError::PositiveTimeout),
            ),
            (
                "negative command timeout",
                valid_vin,
                "",
                default_key_name,
                20,
                -1,
                Some(ConfigError::PositiveTimeout),
            ),
            (
                "connect timeout too large",
                valid_vin,
                "",
                default_key_name,
                MAX_TIMEOUT_SEC + 1,
                5,
                Some(ConfigError::MaxTimeout),
            ),
            (
                "command timeout too large",
                valid_vin,
                "",
                default_key_name,
                20,
                MAX_TIMEOUT_SEC + 1,
                Some(ConfigError::MaxTimeout),
            ),
            (
                "exactly at max allowed",
                valid_vin,
                "",
                default_key_name,
                MAX_TIMEOUT_SEC,
                MAX_TIMEOUT_SEC,
                None,
            ),
            (
                "empty VIN clears config",
                "",
                "",
                default_key_name,
                20,
                5,
                None,
            ),
            (
                " 5YJ3E1EA0PF000000 ",
                " 5YJ3E1EA0PF000000 ",
                "",
                default_key_name,
                20,
                5,
                None,
            ),
            (
                "VIN too short",
                "5YJ3E1EA0PF00000",
                "",
                default_key_name,
                20,
                5,
                Some(ConfigError::InvalidVin),
            ),
            (
                "VIN with letter I",
                "5YJ3E1EA0PI000000",
                "",
                default_key_name,
                20,
                5,
                Some(ConfigError::InvalidVin),
            ),
            (
                "VIN with letter O",
                "5YJ3E1EA0PO000000",
                "",
                default_key_name,
                20,
                5,
                Some(ConfigError::InvalidVin),
            ),
            (
                "VIN with lowercase",
                "5yj3e1ea0pf000000",
                "",
                default_key_name,
                20,
                5,
                Some(ConfigError::InvalidVin),
            ),
            (
                "unknown model",
                valid_vin,
                "roadster",
                default_key_name,
                20,
                5,
                Some(ConfigError::InvalidModel),
            ),
            (
                "uppercase model is normalized, not rejected",
                valid_vin,
                "MODEL3",
                default_key_name,
                20,
                5,
                None,
            ),
            ("empty key name clears it", valid_vin, "", "", 20, 5, None),
            (
                "key name at max length",
                valid_vin,
                "",
                &"a".repeat(MAX_KEY_NAME_LEN),
                20,
                5,
                None,
            ),
            (
                "key name too long",
                valid_vin,
                "",
                &"a".repeat(MAX_KEY_NAME_LEN + 1),
                20,
                5,
                Some(ConfigError::InvalidKeyName),
            ),
            (
                "key name with disallowed characters",
                valid_vin,
                "",
                "phone; rm -rf /",
                20,
                5,
                Some(ConfigError::InvalidKeyName),
            ),
            (
                "key name with newline",
                valid_vin,
                "",
                "phone\nkey",
                20,
                5,
                Some(ConfigError::InvalidKeyName),
            ),
            (
                "key name with spaces/dots/dashes/underscores",
                valid_vin,
                "",
                "My Phone_v2.0-test",
                20,
                5,
                None,
            ),
        ];
        for (name, vin, model, key_name, connect_timeout, command_timeout, want) in cases {
            let got =
                validate_config(vin, model, key_name, *connect_timeout, *command_timeout).err();
            assert_eq!(
                got, *want,
                "{name}: validate_config({vin:?}, {model:?}, {key_name:?})"
            );
        }
    }

    #[test]
    fn test_save_config_atomic() {
        let dir = std::env::temp_dir().join(format!("electric-eel-test-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.json");

        let cfg = Config {
            version: ConfigVersion::CURRENT,
            vin: "5YJ3E1EA0PF000000".to_string(),
            model: Model::Auto,
            key_name: "harbour-electric-eel".to_string(),
            connect_timeout_sec: 20,
            command_timeout_sec: 5,
            vin_state: VinState::Paired,
            cover_left_action: Some(CoverAction::FlashLights),
            cover_right_action: Some(CoverAction::Unlock),
            vehicle_name: "Eel".to_string(),
            notification_enabled: true,
            notification_actions: vec![CoverAction::Honk, CoverAction::Lock],
        };
        cfg.save(&path).expect("save");
        let restored = Config::load(&path).unwrap();
        assert_eq!(restored.cover_left_action, cfg.cover_left_action);
        assert_eq!(restored.cover_right_action, cfg.cover_right_action);
        assert_eq!(restored.vehicle_name, "Eel");
        assert!(restored.notification_enabled);
        assert_eq!(restored.notification_actions, cfg.notification_actions);

        let data = fs::read_to_string(&path).expect("config file not written");
        assert!(!data.is_empty(), "config file is empty");

        let leftover = fs::read_dir(&dir)
            .unwrap()
            .filter_map(Result::ok)
            .filter(|e| e.file_name() != "config.json")
            .count();
        assert_eq!(leftover, 0, "temporary file left behind after rename");

        let perm = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(perm, 0o600, "config file permissions");

        let reloaded = Config::load(&path).expect("reload");
        assert_eq!(reloaded.vin, cfg.vin);
        assert_eq!(reloaded.connect_timeout_sec, cfg.connect_timeout_sec);

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_load_sanitizes_out_of_range_fields() {
        let dir =
            std::env::temp_dir().join(format!("electric-eel-test-sanitize-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.json");

        // Simulates a hand-edited config.json - validate_config would
        // reject all five of these fields via SetConfig, but load() must
        // not simply trust a file that bypassed that path.
        fs::write(
            &path,
            r#"{"vin":"not-a-real-vin","model":"roadster","key_name":"phone\nname","connect_timeout_sec":-5,"command_timeout_sec":99999}"#,
        )
        .unwrap();

        let cfg = Config::load(&path).expect("load");
        let default = Config::default();
        assert_eq!(
            cfg.vin, "",
            "invalid vin should be cleared, not smuggled through"
        );
        assert_eq!(
            cfg.model, default.model,
            "invalid model should reset to default"
        );
        assert_eq!(
            cfg.key_name, default.key_name,
            "invalid key_name should reset to default"
        );
        assert_eq!(cfg.connect_timeout_sec, default.connect_timeout_sec);
        assert_eq!(cfg.command_timeout_sec, default.command_timeout_sec);

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_load_pre_model_field_config_keeps_vin() {
        // A config.json written by any pre-0.1.6 build - before the "model"
        // field existed - has no "model" key at all. Regression test for the
        // bug where a missing (not just invalid) field made serde reject the
        // whole file, so load() fell back to Config::default() and silently
        // dropped the VIN/key_name/timeouts too, not just the model.
        let dir =
            std::env::temp_dir().join(format!("electric-eel-test-premodel-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.json");

        fs::write(
            &path,
            r#"{"vin":"5YJ3E1EA0PF000000","key_name":"harbour-electric-eel","connect_timeout_sec":20,"command_timeout_sec":5}"#,
        )
        .unwrap();

        let cfg = Config::load(&path).expect("load");
        assert_eq!(
            cfg.vin, "5YJ3E1EA0PF000000",
            "pre-existing VIN must survive loading an old config.json"
        );
        assert_eq!(
            cfg.model,
            Model::Auto,
            "missing model field should default to Auto, not reject the file"
        );
        assert_eq!(cfg.key_name, "harbour-electric-eel");
        assert_eq!(cfg.connect_timeout_sec, 20);
        assert_eq!(cfg.command_timeout_sec, 5);
        assert_eq!(
            cfg.version,
            ConfigVersion::V0,
            "a pre-phone-key file with no pairing state is schema V0"
        );

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_load_translates_legacy_paired_vin() {
        let dir = std::env::temp_dir().join(format!(
            "electric-eel-test-legacy-paired-{}",
            std::process::id()
        ));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.json");
        let vin = "5YJ3E1EA0PF000000";

        // Legacy field equal to the configured VIN means "paired".
        fs::write(
            &path,
            format!(
                r#"{{"vin":"{vin}","key_name":"harbour-electric-eel","connect_timeout_sec":20,"command_timeout_sec":5,"paired_vin":"{vin}"}}"#
            ),
        )
        .unwrap();
        let cfg = Config::load(&path).expect("load");
        assert_eq!(cfg.vin_state, VinState::Paired);
        assert_eq!(cfg.version, ConfigVersion::V1);

        // Legacy empty field means "explicitly unpaired".
        fs::write(
            &path,
            format!(
                r#"{{"vin":"{vin}","key_name":"harbour-electric-eel","connect_timeout_sec":20,"command_timeout_sec":5,"paired_vin":""}}"#
            ),
        )
        .unwrap();
        let cfg = Config::load(&path).expect("load");
        assert_eq!(cfg.vin_state, VinState::Unpaired);
        assert_eq!(cfg.version, ConfigVersion::V1);

        // Legacy field pointing at a different VIN is not paired.
        fs::write(
            &path,
            format!(
                r#"{{"vin":"{vin}","key_name":"harbour-electric-eel","connect_timeout_sec":20,"command_timeout_sec":5,"paired_vin":"5YJ3E1EA0PF111111"}}"#
            ),
        )
        .unwrap();
        let cfg = Config::load(&path).expect("load");
        assert_eq!(cfg.vin_state, VinState::Unpaired);
        assert_eq!(cfg.version, ConfigVersion::V1);

        // No pairing key at all -> pre-phone-key config (V0); Core::new
        // decides from key-file presence whether to mark it paired.
        fs::write(
            &path,
            format!(
                r#"{{"vin":"{vin}","key_name":"harbour-electric-eel","connect_timeout_sec":20,"command_timeout_sec":5}}"#
            ),
        )
        .unwrap();
        let cfg = Config::load(&path).expect("load");
        assert_eq!(cfg.vin_state, VinState::Unpaired);
        assert_eq!(cfg.version, ConfigVersion::V0);

        // Current-format configs carry vin_state; without a version key they
        // infer V2 and are never migrated.
        fs::write(
            &path,
            format!(
                r#"{{"vin":"{vin}","key_name":"harbour-electric-eel","connect_timeout_sec":20,"command_timeout_sec":5,"vin_state":"paired"}}"#
            ),
        )
        .unwrap();
        let cfg = Config::load(&path).expect("load");
        assert_eq!(cfg.vin_state, VinState::Paired);
        assert_eq!(cfg.version, ConfigVersion::V2);

        // An explicit version key wins over inference...
        fs::write(
            &path,
            format!(
                r#"{{"version":1,"vin":"{vin}","key_name":"harbour-electric-eel","connect_timeout_sec":20,"command_timeout_sec":5,"paired_vin":""}}"#
            ),
        )
        .unwrap();
        let cfg = Config::load(&path).expect("load");
        assert_eq!(cfg.version, ConfigVersion::V1);

        // ...and an unknown (newer-build) version is preserved, never mapped
        // onto a known one so a newer file is never rewritten as older.
        fs::write(
            &path,
            format!(
                r#"{{"version":99,"vin":"{vin}","key_name":"harbour-electric-eel","connect_timeout_sec":20,"command_timeout_sec":5,"vin_state":"paired"}}"#
            ),
        )
        .unwrap();
        let cfg = Config::load(&path).expect("load");
        assert_eq!(cfg.version, ConfigVersion::Unknown(99));
        assert_eq!(cfg.vin_state, VinState::Paired);

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn invalid_cover_preferences_do_not_discard_vehicle_config() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        for invalid in [
            "null",
            r#""unknown-action""#,
            r#""""#,
            "true",
            "42",
            "-3",
            "1.5",
            r#"["honk",{"nested":[1,2]}]"#,
            r#"{"action":"honk","nested":[null,false]}"#,
        ] {
            for (left, right) in [(invalid, r#""honk""#), (r#""unlock""#, invalid)] {
                let json = format!(
                    r#"{{"cover_left_action":{left},"cover_right_action":{right},"vin":"5YJ3E1EA0PF000000","key_name":"phone","connect_timeout_sec":20,"command_timeout_sec":5,"vin_state":"paired"}}"#
                );
                fs::write(&path, json).unwrap();
                let cfg = Config::load(&path).unwrap();
                assert_eq!(cfg.vin, "5YJ3E1EA0PF000000");
                assert_eq!(cfg.vin_state, VinState::Paired);
                assert_eq!(cfg.version, ConfigVersion::CURRENT);
                assert_eq!(
                    cfg.cover_left_action,
                    if left == invalid {
                        None
                    } else {
                        Some(CoverAction::Unlock)
                    }
                );
                assert_eq!(
                    cfg.cover_right_action,
                    if right == invalid {
                        None
                    } else {
                        Some(CoverAction::Honk)
                    }
                );
                assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 1);
            }
        }
        // Escaped JSON strings still map to the exact enum variant.
        let cfg: Config = serde_json::from_str(
            r#"{"vin":"","key_name":"phone","connect_timeout_sec":20,"command_timeout_sec":5,"cover_left_action":"trunk-\u006fpen"}"#,
        ).unwrap();
        assert_eq!(cfg.cover_left_action, Some(CoverAction::TrunkOpen));
        assert_eq!(cfg.cover_right_action, None);
    }

    #[test]
    fn newer_incompatible_config_is_preserved_without_a_corrupt_backup() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        // Newer schemas may replace fields we currently require, or use a
        // version number beyond the u8 accepted by ConfigVersion.
        for version in [99_u64, 256, u64::MAX] {
            let original = format!(
                r#"{{"version":{version},"vin":{{"new":"representation"}},"future":[null,{{"keep":true}}]}}"#
            );
            fs::write(&path, &original).unwrap();
            let error = Config::load(&path).unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::InvalidData);
            assert!(error
                .to_string()
                .contains("unsupported newer config schema"));
            assert_eq!(fs::read_to_string(&path).unwrap(), original);
            assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 1);
        }
    }

    #[test]
    fn test_load_trims_whitespace_fields() {
        // validate_config/set_config trim VIN and key_name, but sanitize()
        // only validates the trimmed value and stores the raw one with
        // surrounding spaces. A hand-edited config then carries "-vin ' 5YJ… '"
        // (with spaces) into tesla-control and fails every command.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        fs::write(
            &path,
            r#"{"vin":" 5YJ3E1EA0PF000000 ","model":"","key_name":"  mykey  ","connect_timeout_sec":20,"command_timeout_sec":5}"#,
        )
        .unwrap();
        let cfg = Config::load(&path).expect("load");
        assert_eq!(
            cfg.vin, "5YJ3E1EA0PF000000",
            "VIN with surrounding spaces must load trimmed"
        );
        assert_eq!(
            cfg.key_name, "mykey",
            "key_name with surrounding spaces must load trimmed"
        );
    }

    #[test]
    fn production_prune_keeps_newest_despite_unpadded_nanos() {
        // prune_corrupt_backups sorts backup paths lexicographically, but the
        // nanosecond stamp is unpadded decimal: "100" sorts before "99"
        // ('1' < '9') even though 99ns is older. With 6 backups the prune
        // must delete the chronologically oldest (99ns) and keep the newest
        // five — lexicographic order deletes a newer file and keeps the
        // oldest beyond the retention window.
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("config.json");
        fs::write(&config, "{}").unwrap();
        let secs = 1_700_000_000_u64;
        for nanos in [99_u32, 100, 101, 102, 103, 104] {
            let name = format!("config.corrupt-{secs}-{nanos}-1.json");
            fs::write(dir.path().join(name), b"evidence").unwrap();
        }
        super::prune_corrupt_backups(&config);
        let remaining: Vec<String> = fs::read_dir(dir.path())
            .unwrap()
            .filter_map(Result::ok)
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.starts_with("config.corrupt-"))
            .collect();
        assert_eq!(
            remaining.len(),
            5,
            "prune must keep exactly 5 backups, kept {remaining:?}"
        );
        assert!(
            !remaining.iter().any(|n| n.contains("-99-")),
            "chronologically oldest (99ns) must be deleted, kept {remaining:?}"
        );
        assert!(
            remaining.iter().any(|n| n.contains("-104-")),
            "newest (104ns) must be kept, kept {remaining:?}"
        );
    }

    #[test]
    fn audit_custom_config_backup_retention_is_scoped_to_its_stem() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("vehicle.settings.json");
        let unrelated = dir.path().join("config.corrupt-1-0-1.json");
        std::fs::write(&unrelated, "other config").unwrap();
        for stamp in 1..=7 {
            std::fs::write(
                dir.path()
                    .join(format!("vehicle.settings.corrupt-{stamp}-0-1.json")),
                "bad",
            )
            .unwrap();
        }
        super::prune_corrupt_backups(&path);
        assert!(unrelated.exists());
        for stamp in 1..=7 {
            assert_eq!(
                dir.path()
                    .join(format!("vehicle.settings.corrupt-{stamp}-0-1.json"))
                    .exists(),
                stamp > 2
            );
        }
    }
}
