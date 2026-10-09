use std::fmt;
use std::fs;
use std::io::{self, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::LazyLock;

use regex::Regex;
use serde::{Deserialize, Serialize};
use tempfile::NamedTempFile;

pub(crate) const MAX_TIMEOUT_SEC: i32 = 300;
pub(crate) const MAX_KEY_NAME_LEN: usize = 64;

/// The accepted values of Config.model. "" is "Auto (from VIN)": the QML
/// client guesses the model from the VIN's WMI prefix and nothing is forced.
/// Every other entry doubles as a `Model` id in the client-side MODELS list
/// (app/qml/js/VehicleState.js) and a key into the model images the front
/// page shows. Keep this, VehicleState.js's MODELS, and that file's
/// VIN-prefix `guessModel()` table in sync when adding/removing models.
pub(crate) const VALID_MODELS: [&str; 6] =
    ["", "model3", "models", "modelx", "modely", "cybertruck"];

static VIN_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[A-HJ-NPR-Z0-9]{17}$").unwrap());
// key_name is functionally near-inert (tesla-control only consults it for
// an OS-keyring-backed key, and this app always passes -keyring-type file,
// which loads by -key-file and never reaches the keyring lookup) - this
// isn't guarding against it doing anything dangerous, just against an
// unbounded/control-character string sitting in config.json and getting
// echoed into SetConfig's journal log line on every call.
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
                VALID_MODELS
                    .into_iter()
                    .filter(|m| !m.is_empty())
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
    // wipes the already-configured VIN from the running daemon too, not
    // just the model. sanitize() still normalizes/validates whatever comes
    // out of this either way.
    #[serde(default)]
    pub model: String,
    pub key_name: String,
    pub connect_timeout_sec: i32,
    pub command_timeout_sec: i32,
    /// Whether the current VIN's key has completed NFC pairing.
    /// `Unpaired` is the serde default; older schema versions that predate
    /// this field are migrated by `Core::new`.
    #[serde(default)]
    pub vin_state: VinState,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            version: ConfigVersion::CURRENT,
            vin: String::new(),
            model: String::new(),
            key_name: "harbour-electric-eel".to_string(),
            connect_timeout_sec: 20,
            command_timeout_sec: 5,
            vin_state: VinState::Unpaired,
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
            #[serde(default, deserialize_with = "present_optional")]
            vin_state: EnrollmentField<VinState>,
            #[serde(default, deserialize_with = "present_optional")]
            paired_vin: EnrollmentField<String>,
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
            model: raw.model,
            key_name: raw.key_name,
            connect_timeout_sec: raw.connect_timeout_sec,
            command_timeout_sec: raw.command_timeout_sec,
            vin_state,
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
                let version = serde_json::from_slice::<serde_json::Value>(&data)
                    .ok()
                    .and_then(|json| json.get("version").and_then(serde_json::Value::as_u64));
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
        let model = self.model.trim().to_ascii_lowercase();
        if VALID_MODELS.contains(&model.as_str()) {
            self.model = model;
        } else {
            eprintln!("electric-eel: config.json model fails validation, resetting to default");
            self.model = default.model;
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
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
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
    #[cfg(unix)]
    {
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&backup)
        {
            Ok(mut f) => {
                if let Err(e) = f.write_all(data) {
                    eprintln!(
                        "electric-eel: could not back up corrupt config {}: {}",
                        backup.display(),
                        e
                    );
                    return;
                }
            }
            Err(e) => {
                eprintln!(
                    "electric-eel: could not back up corrupt config {}: {}",
                    backup.display(),
                    e
                );
                return;
            }
        }
        // Defense in depth: ensure mode even if the platform ignored it.
        let _ = std::fs::set_permissions(&backup, std::fs::Permissions::from_mode(0o600));
    }
    #[cfg(not(unix))]
    {
        if let Err(e) = fs::write(&backup, data) {
            eprintln!(
                "electric-eel: could not back up corrupt config {}: {}",
                backup.display(),
                e
            );
            return;
        }
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
    let Some(parent) = path.parent() else { return };
    let Ok(entries) = std::fs::read_dir(parent) else {
        return;
    };
    let mut backups: Vec<_> = entries
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| {
            let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
            name.starts_with("config.corrupt-")
                && p.extension()
                    .is_some_and(|e| e.eq_ignore_ascii_case("json"))
        })
        .collect();
    // Parse timestamps so existing, unpadded backup names sort correctly too.
    backups.sort_by_key(|p| {
        let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
        let mut fields = name
            .trim_start_matches("config.corrupt-")
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

/// Returns `Ok(())` if the inputs are acceptable, else a human-readable error.
/// Shared by `SetConfig` and unit tests so the bounds can be verified without a
/// live D-Bus connection.
pub(crate) fn validate_config(
    vin: &str,
    model: &str,
    key_name: &str,
    connect_timeout: i32,
    command_timeout: i32,
) -> Result<(), ConfigError> {
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
    if !VALID_MODELS.contains(&model.trim().to_ascii_lowercase().as_str()) {
        return Err(ConfigError::InvalidModel);
    }
    let key_name = key_name.trim();
    if key_name.len() > MAX_KEY_NAME_LEN || !KEY_NAME_RE.is_match(key_name) {
        return Err(ConfigError::InvalidKeyName);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

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
            model: String::new(),
            key_name: "harbour-electric-eel".to_string(),
            connect_timeout_sec: 20,
            command_timeout_sec: 5,
            vin_state: VinState::Paired,
        };
        cfg.save(&path).expect("save");

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
            cfg.model, "",
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
}
