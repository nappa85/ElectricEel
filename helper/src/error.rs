//! Error types shared by the in-process control core (`core.rs`) and, when
//! built with the `dbus` feature, the D-Bus daemon surface (`helper.rs`).
//!
//! The `#[derive(zbus::DBusError)]` on [`HelperError`] is feature-gated: the
//! app links the staticlib that builds the plain core, and must not drag in
//! zbus/async-io machinery it never runs. Only the daemon binary enables the
//! `dbus` feature, gaining the D-Bus error name
//! `"org.electriceel.Helper1.<Variant>"` that the pre-Phase-4 QML client
//! matched exactly.

use std::io;

use crate::config::ConfigError;

/// Errors from the soft-fail core operations (`set_config`, `generate_key`,
/// `start_phone_key`): the operation did not proceed, but the failure is
/// delivered to the caller as `ok=false` + a message rather than as a
/// D-Bus/ABI error (unlike [`HelperError`]). Typed so the known cases can be
/// matched directly and the underlying `io`/config cause is kept in the
/// `source()` chain instead of being flattened into a bare `String`.
#[derive(Debug, thiserror::Error)]
pub(crate) enum OperationError {
    /// Presence can't start: no configured VIN, or the current key hasn't
    /// completed NFC pairing with it.
    #[error("phone key is waiting for a configured VIN and completed pairing")]
    NotPaired,
    /// Presence needs the persistent session, which isn't running.
    #[error("persistent BLE session is unavailable")]
    NoSession,
    /// Automatic phone-key presence requires the `BlueZ` backend.
    #[error("automatic phone key requires the BlueZ backend")]
    RequiresBluez,
    /// The `BlueZ` presence service refused to start (holds its stderr).
    #[error("{0}")]
    PresenceFailed(String),
    /// Key generation was refused or the generated public key couldn't be
    /// read back (holds the trimmed stderr / read error).
    #[error("{0}")]
    KeygenFailed(String),
    /// The `SetConfig` payload failed validation.
    #[error("{0}")]
    Config(#[from] ConfigError),
    /// `config.json` could not be persisted.
    #[error("could not persist config: {0}")]
    Persist(#[source] io::Error),
    /// The public key could not be written to disk.
    #[error("could not write public key: {0}")]
    WritePubkey(#[source] io::Error),
}

/// Error type for hard failures that abort a call and surface to the caller
/// as a D-Bus error (daemon) or a `CoreError`-carrying message (app FFI),
/// rather than as a soft `ok=false` reply.
#[derive(Debug)]
#[cfg_attr(
    feature = "dbus",
    derive(zbus::DBusError),
    zbus(prefix = "org.electriceel.Helper1")
)]
pub enum HelperError {
    Forbidden(String),
    Busy(String),
    NotConfigured(String),
    NoKey(String),
    UnknownCommand(String),
    InvalidArgument(String),
    /// Raised instead of the hci fallback when the bluez persistent session
    /// is unusable - falling back to a raw-HCI one-shot would tear down the
    /// very adapter connections (e.g. a soundbar) that bluez mode exists to
    /// preserve, so the failure must surface to the caller instead.
    SessionUnavailable(String),
    #[cfg_attr(feature = "dbus", zbus(error))]
    #[cfg(feature = "dbus")]
    ZBus(zbus::Error),
}

// std::error::Error so callers get source() chains and `?` ergonomics.
// The dbus-feature build already gets an Error impl from the
// zbus::DBusError derive; this covers the app staticlib build.
#[cfg(not(feature = "dbus"))]
impl std::error::Error for HelperError {}

#[cfg(not(feature = "dbus"))]
impl std::fmt::Display for HelperError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            HelperError::Forbidden(m) => write!(f, "forbidden: {m}"),
            HelperError::Busy(m) => write!(f, "busy: {m}"),
            HelperError::NotConfigured(m) => write!(f, "not configured: {m}"),
            HelperError::NoKey(m) => write!(f, "no key: {m}"),
            HelperError::UnknownCommand(m) => write!(f, "unknown command: {m}"),
            HelperError::InvalidArgument(m) => write!(f, "invalid argument: {m}"),
            HelperError::SessionUnavailable(m) => write!(f, "session unavailable: {m}"),
        }
    }
}
