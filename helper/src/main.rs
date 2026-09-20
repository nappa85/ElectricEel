use std::path::Path;

use electriceelcore::core::Core;
use electriceelcore::helper::{Helper, IFACE_NAME};
use electriceelcore::session_client::SessionClient;

const BUS_NAME: &str = "org.electriceel.Helper";
const OBJECT_PATH: &str = "/org/electriceel/Helper";

/// binDir holds the bundled, setcap'd tesla-control/tesla-keygen binaries.
/// stateDir holds the private key and persisted config; both are created by
/// the RPM with restrictive permissions for the service's own system user.
fn env_or(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_string())
}

fn env_flag(key: &str) -> bool {
    match std::env::var(key) {
        Ok(v) => v != "0" && !v.eq_ignore_ascii_case("false"),
        Err(_) => false,
    }
}

fn main() {
    let bin_dir = env_or("ELECTRICEEL_BIN_DIR", "/opt/electric-eel/bin");
    let state_dir = env_or("ELECTRICEEL_STATE_DIR", "/var/lib/electric-eel");
    let allowed_callers = electriceelcore::authorize::default_allowed_callers();

    if let Err(e) = std::fs::create_dir_all(&state_dir) {
        eprintln!("cannot create state dir {state_dir}: {e}");
        std::process::exit(1);
    }

    let credentials_conn = match zbus::blocking::Connection::system() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("cannot connect to system bus: {e}");
            std::process::exit(1);
        }
    };

    // Off by default and not yet verified on real hardware - see
    // docs/limitations.md. tesla-session is bundled in bin_dir alongside
    // tesla-control/tesla-keygen and setcap'd the same way (needs
    // CAP_NET_ADMIN itself once it's the one holding the BLE session).
    //
    // ELECTRICEEL_BLE_BACKEND (default "hci") selects tesla-session's
    // transport: "hci" is go-ble's raw HCI channel (the default, matching
    // a one-shot tesla-control), "bluez" routes through org.bluez so the
    // OS Bluetooth stack - and anything else using the adapter, e.g. a
    // soundbar - is never disturbed. The setting is read here so this
    // process shares one source of truth with the child it spawns.
    let session = if env_flag("ELECTRICEEL_PERSISTENT_SESSION") {
        let ble_backend = env_or("ELECTRICEEL_BLE_BACKEND", "hci");
        if ble_backend != "hci" && ble_backend != "bluez" {
            eprintln!(
                "electric-eel: invalid ELECTRICEEL_BLE_BACKEND {ble_backend:?} (want hci or bluez)"
            );
            std::process::exit(1);
        }
        eprintln!(
            "electric-eel: persistent BLE session enabled (ELECTRICEEL_PERSISTENT_SESSION, backend {ble_backend})"
        );
        Some(SessionClient::new(
            Path::new(&bin_dir).join("tesla-session"),
            &ble_backend,
        ))
    } else {
        None
    };

    let core = match Core::new(bin_dir, state_dir, session) {
        Ok(core) => core,
        Err(e) => {
            eprintln!("electric-eel: {e}");
            std::process::exit(1);
        }
    };
    let helper = Helper::new(core, allowed_callers, credentials_conn);

    let conn = zbus::blocking::connection::Builder::system()
        .and_then(|b| b.serve_at(OBJECT_PATH, helper))
        .and_then(|b| b.name(BUS_NAME))
        .and_then(zbus::blocking::connection::Builder::build);

    // Named _conn, not conn: it's never read again, only kept alive (its
    // Drop would tear down the background dispatch machinery) until the
    // process exits at thread::park() below. The leading underscore tells
    // rustc that's deliberate instead of needing a `let _ = &conn;` no-op
    // to silence the unused-variable lint.
    let _conn = match conn {
        Ok(c) => c,
        Err(e) => {
            eprintln!("cannot start electric-eel-daemon: {e}");
            std::process::exit(1);
        }
    };

    eprintln!("electric-eel-daemon listening on {BUS_NAME} {OBJECT_PATH} ({IFACE_NAME})");
    std::thread::park();
}
