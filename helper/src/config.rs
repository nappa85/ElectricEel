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
/// `Core::new` healed it because the VIN is set and both key files are on
/// disk); `Unpaired` (the serde default) means it hasn't. This replaces the
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
            #[serde(default)]
            vin_state: Option<VinState>,
            #[serde(default)]
            paired_vin: Option<String>,
        }

        let raw = LegacyConfig::deserialize(deserializer)?;
        let (vin_state, version) = if let Some(state) = raw.vin_state {
            (state, raw.version.unwrap_or(ConfigVersion::V2))
        } else if let Some(v) = raw.paired_vin.as_deref() {
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
    /// [`Config::default`]; an unparseable file is logged and defaults too.
    pub(crate) fn load(path: &Path) -> io::Result<Config> {
        let data = match fs::read(path) {
            Ok(d) => d,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Config::default()),
            Err(e) => return Err(e),
        };
        let mut cfg: Config = match serde_json::from_slice(&data) {
            Ok(cfg) => cfg,
            Err(e) => {
                eprintln!(
                    "electric-eel: ignoring unparseable config {}: {}",
                    path.display(),
                    e
                );
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
        if !self.vin.trim().is_empty() && !VIN_RE.is_match(self.vin.trim()) {
            eprintln!("electric-eel: config.json vin fails validation, clearing");
            self.vin = String::new();
        }
        let model = self.model.trim().to_ascii_lowercase();
        if VALID_MODELS.contains(&model.as_str()) {
            self.model = model;
        } else {
            eprintln!("electric-eel: config.json model fails validation, resetting to default");
            self.model = default.model;
        }
        let key_name = self.key_name.trim();
        if key_name.len() > MAX_KEY_NAME_LEN || !KEY_NAME_RE.is_match(key_name) {
            eprintln!("electric-eel: config.json key_name fails validation, resetting to default");
            self.key_name = default.key_name;
        }
    }

    /// Writes to a `.tmp` sibling and renames into place, so a crash mid-write
    /// can't truncate/zero the file and silently reset the VIN/key/timeouts.
    /// Both the temp file's contents (fsync before rename) and the parent
    /// directory entry (fsync after rename) are flushed to disk, so a power
    /// loss right after the rename can't lose the new config either.
    pub(crate) fn save(&self, path: &Path) -> io::Result<()> {
        let data = serde_json::to_vec_pretty(self)?;
        let dir = path.parent().unwrap_or_else(|| Path::new("."));
        let mut tmp = NamedTempFile::new_in(dir)?;
        tmp.as_file()
            .set_permissions(fs::Permissions::from_mode(0o600))?;
        tmp.write_all(&data)?;
        tmp.as_file().sync_all()?;
        tmp.persist(path)?;
        fs::File::open(dir)?.sync_all()
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
}
