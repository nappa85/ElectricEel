//! D-Bus daemon surface for the control core (pre-Phase-4 and fallback). This
//! module is only compiled with the `dbus` cargo feature, which only the
//! `electric-eel-daemon` binary enables; the app's staticlib uses the separate
//! runtime feature for its zbus MCE client without caller authorization.
//!
//! All real work lives in [`crate::core::Core`]; these interface methods are
//! thin wrappers that (a) authorize the D-Bus caller and (b) hand off to the
//! shared core, preserving the pre-Phase-4 wire behavior exactly so a mixed
//! app+daemon install can't regress silently during the transition.

use zbus::message::Header;

use crate::authorize::authorize;
use crate::core::{Core, GetConfigReply};
use crate::error::HelperError;

pub const IFACE_NAME: &str = "org.electriceel.Helper1";

pub struct Helper {
    core: Core,
    /// A connection dedicated to outbound proxy calls (`GetConnectionCredentials`
    /// in `authorize()`), deliberately separate from the connection the
    /// `ObjectServer` uses to dispatch these very method calls. zbus's own
    /// docs warn against blocking-API calls from within an interface method
    /// on the *same* connection that's driving that method's dispatch (the
    /// "async sandwich" footgun) - a second, independent connection/socket
    /// avoids that reentrancy hazard entirely.
    credentials_conn: zbus::blocking::Connection,
    allowed_callers: Vec<String>,
}

impl Helper {
    pub fn new(
        core: Core,
        allowed_callers: Vec<String>,
        credentials_conn: zbus::blocking::Connection,
    ) -> Helper {
        Helper {
            core,
            credentials_conn,
            allowed_callers,
        }
    }

    fn authorize_sender(&self, header: &Header<'_>) -> Result<(), HelperError> {
        let sender = header
            .sender()
            .ok_or_else(|| HelperError::Forbidden("cannot resolve caller credentials".to_string()))?
            .to_string();
        authorize(&self.credentials_conn, &sender, &self.allowed_callers)
    }
}

// clippy::needless_pass_by_value is a false positive for every method
// below: #[zbus(header)] header is constructed and passed in by value by
// the zbus::interface macro itself (verified empirically - `&Header<'_>`
// is a type mismatch against the macro-generated call site, not a style
// choice), and D-Bus "in" parameters like run()'s `args: Vec<String>`
// can't become `&[String]` because zvariant has no owner to borrow a
// `Vec<String>` from while deserializing the message body (unlike a bare
// `&str`, which zvariant *can* deserialize zero-copy - see set_config's
// vin/key_name, which clippy correctly flagged and are fixed below).
#[allow(clippy::needless_pass_by_value)]
#[zbus::interface(name = "org.electriceel.Helper1")]
impl Helper {
    /// Executes a single command. cmd must be one of the known tesla-control
    /// subcommands; args are passed through verbatim (never as flags).
    fn run(
        &self,
        cmd: String,
        args: Vec<String>,
        #[zbus(header)] header: Header<'_>,
    ) -> Result<(bool, String, String, i32), HelperError> {
        self.authorize_sender(&header)?;
        self.core.run(&cmd, &args)
    }

    /// Creates a new local private key and returns its PEM-encoded public key.
    fn generate_key(
        &self,
        force: bool,
        #[zbus(header)] header: Header<'_>,
    ) -> Result<(bool, String, String), HelperError> {
        self.authorize_sender(&header)?;
        Ok(match self.core.generate_key(force) {
            Ok(pubkey) => (true, pubkey, String::new()),
            Err(err) => (false, String::new(), err.to_string()),
        })
    }

    /// Enrolls the current public key with the vehicle via BLE.
    fn pair(
        &self,
        #[zbus(header)] header: Header<'_>,
    ) -> Result<(bool, String, String), HelperError> {
        self.authorize_sender(&header)?;
        self.core.pair()
    }

    fn set_config(
        &self,
        vin: &str,
        model: &str,
        key_name: &str,
        connect_timeout_sec: i32,
        command_timeout_sec: i32,
        #[zbus(header)] header: Header<'_>,
    ) -> Result<(bool, String), HelperError> {
        self.authorize_sender(&header)?;
        match self.core.set_config(
            vin,
            model,
            key_name,
            connect_timeout_sec,
            command_timeout_sec,
        ) {
            Ok(()) => Ok((true, String::new())),
            Err(msg) => Ok((false, msg.to_string())),
        }
    }

    fn get_config(
        &self,
        #[zbus(header)] header: Header<'_>,
    ) -> Result<GetConfigReply, HelperError> {
        self.authorize_sender(&header)?;
        Ok(self.core.get_config())
    }

    /// Returns the daemon's own build version (`CARGO_PKG_VERSION`). Same
    /// value as the app's `APP_VERSION` - both are stamped from the one
    /// upstream git tag, so app and daemon can't drift apart.
    fn get_version(&self, #[zbus(header)] header: Header<'_>) -> Result<String, HelperError> {
        self.authorize_sender(&header)?;
        Ok(env!("CARGO_PKG_VERSION").to_string())
    }
}
