//! C ABI surface for the in-process control core (see `docs/architecture.md`
//! for why). The app links `libelectriceelcore.a` and drives
//! all vehicle/config work through these functions on its own worker thread;
//! the header is generated from this module with cbindgen
//! (`cbindgen --crate electriceelcore --output electriceelcore.h`, or the
//! build.rs hook described in Cargo.toml).
//!
//! Conventions:
//! - An opaque `Core` handle, created once by `core_new` and freed exactly
//!   once by `core_free`. Never shared across threads that call into it
//!   concurrently - the QML client owns a single worker thread (see
//!   `app/src/teslaclient.cpp`), and `Core` uses internal mutexes only to
//!   serialize its own sub-state, not to be a thread-safe handle.
//! - All strings are UTF-8 C strings. Output strings are heap-allocated by
//!   the callee and must be released with `core_string_free`.
//! - `core_new` returns NULL and, when `err_out` is non-NULL, a caller-freed
//!   message on failure. Other functions return a `CoreError` (0 == ok) and
//!   additionally take output slots for their payload.

use std::ffi::{CStr, CString};
use std::os::raw::c_char;
use std::path::PathBuf;

use crate::core::Core;
use crate::session_client::SessionClient;

#[repr(C)]
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum CoreError {
    Ok = 0,
    /// A `String` return slot was NULL, or an input pointer was NULL where a
    /// value was required.
    BadArg = 1,
    /// The payload slot (bool/string) could not be written (e.g. an output
    /// buffer that is NULL for a value the call must return).
    Internal = 2,
}

fn cstr(ptr: *const c_char) -> Option<String> {
    if ptr.is_null() {
        return None;
    }
    // SAFETY: callers pass NUL-terminated UTF-8 strings from CString /
    // QString::toUtf8().constData().
    unsafe { CStr::from_ptr(ptr) }
        .to_str()
        .ok()
        .map(str::to_string)
}

/// Returns a C string that the caller must free with `core_string_free`, or
/// NULL. Used for output strings.
fn into_cstring(s: String) -> *mut c_char {
    CString::new(s).map_or(std::ptr::null_mut(), CString::into_raw)
}

fn err_str(e: &str) -> *mut c_char {
    into_cstring(e.to_string())
}

/// Free a string previously returned by any output slot. NULL is a no-op.
/// # Safety
/// `ptr` must be a pointer returned by one of this module's functions, or NULL.
#[no_mangle]
pub unsafe extern "C" fn core_string_free(ptr: *mut c_char) {
    if !ptr.is_null() {
        // SAFETY: guaranteed by the caller contract above.
        drop(unsafe { CString::from_raw(ptr) });
    }
}

/// The build version of the core, stamped from `CARGO_PKG_VERSION` (same git
/// tag as `APP_VERSION` in harbour-electric-eel.pro). Static storage - do not
/// free.
///
/// # Panics
///
/// Only if `CARGO_PKG_VERSION` contains an interior NUL byte, which the
/// Cargo.toml version field can't express.
#[no_mangle]
pub extern "C" fn core_version() -> *const c_char {
    // Static, NUL-terminated via CStr::as_ptr - safe, never freed.
    CStr::from_bytes_with_nul(concat!(env!("CARGO_PKG_VERSION"), "\0").as_bytes())
        .expect("static version string ends in NUL")
        .as_ptr()
}

/// Create the control core.
///
/// # Arguments
/// - `bin_dir`: directory holding the bundled tesla-session (and, as
///   fallback, tesla-control/tesla-keygen) binaries.
/// - `state_dir`: writable directory for config.json / `private_key.pem` /
///   `public_key.pem`.
/// - `session_bin`: path to the bundled `tesla-session` binary; NULL disables
///   the persistent session (falls back to one-shot binaries).
/// - `ble_backend`: "bluez" or "hci" (spawn flag for tesla-session); NULL is
///   treated as "hci".
/// - `err_out`: receives a caller-freed error message on failure, may be NULL.
///
/// # Safety
/// String arguments must be NUL-terminated valid UTF-8; `err_out` must point
/// to writable memory or be NULL.
#[no_mangle]
pub unsafe extern "C" fn core_new(
    bin_dir: *const c_char,
    state_dir: *const c_char,
    session_bin: *const c_char,
    ble_backend: *const c_char,
    err_out: *mut *mut c_char,
) -> *mut Core {
    let Some(bin_dir) = cstr(bin_dir) else {
        if !err_out.is_null() {
            // SAFETY: caller-owned output slot.
            unsafe { *err_out = err_str("core_new: bin_dir is NULL") };
        }
        return std::ptr::null_mut();
    };
    let Some(state_dir) = cstr(state_dir) else {
        if !err_out.is_null() {
            // SAFETY: caller-owned output slot.
            unsafe { *err_out = err_str("core_new: state_dir is NULL") };
        }
        return std::ptr::null_mut();
    };
    let session = if session_bin.is_null() {
        None
    } else {
        let Some(path) = cstr(session_bin) else {
            if !err_out.is_null() {
                // SAFETY: caller-owned output slot.
                unsafe { *err_out = err_str("core_new: session_bin not UTF-8") };
            }
            return std::ptr::null_mut();
        };
        let backend = cstr(ble_backend).unwrap_or_else(|| "hci".to_string());
        Some(SessionClient::new(PathBuf::from(path), &backend))
    };

    // If the core can't be constructed (bad state dir), the app must know.
    match Core::new(bin_dir, state_dir, session) {
        Ok(core) => Box::into_raw(Box::new(core)),
        Err(e) => {
            if !err_out.is_null() {
                // SAFETY: caller-owned output slot.
                unsafe { *err_out = err_str(&e.to_string()) };
            }
            std::ptr::null_mut()
        }
    }
}

/// # Safety
/// `core` must be a pointer returned by `core_new`, or NULL (no-op). Freed
/// exactly once.
#[no_mangle]
pub unsafe extern "C" fn core_free(core: *mut Core) {
    if !core.is_null() {
        // SAFETY: caller contract above.
        drop(unsafe { Box::from_raw(core) });
    }
}

/// `GetConfig` equivalent: returns the persisted config and key presence.
///
/// Outputs (all optional, NULL = skip; string outputs are caller-freed):
/// vin, model, `key_name`, `connect_timeout_sec`, `command_timeout_sec`, `has_key`,
/// `public_key_pem`.
///
/// # Safety
/// `core` must be valid; string output pointers must be writable or NULL.
#[no_mangle]
pub unsafe extern "C" fn core_get_status(
    core: *mut Core,
    vin: *mut *mut c_char,
    model: *mut *mut c_char,
    key_name: *mut *mut c_char,
    connect_timeout_sec: *mut i32,
    command_timeout_sec: *mut i32,
    has_key: *mut bool,
    public_key_pem: *mut *mut c_char,
) -> CoreError {
    let Some(core) = (unsafe { core.as_ref() }) else {
        return CoreError::BadArg;
    };
    let reply = core.get_config();
    if !vin.is_null() {
        // SAFETY: caller-owned slot.
        unsafe { *vin = into_cstring(reply.0) };
    }
    if !model.is_null() {
        // SAFETY: caller-owned slot.
        unsafe { *model = into_cstring(reply.1) };
    }
    if !key_name.is_null() {
        // SAFETY: caller-owned slot.
        unsafe { *key_name = into_cstring(reply.2) };
    }
    if !connect_timeout_sec.is_null() {
        // SAFETY: caller-owned slot.
        unsafe { *connect_timeout_sec = reply.3 };
    }
    if !command_timeout_sec.is_null() {
        // SAFETY: caller-owned slot.
        unsafe { *command_timeout_sec = reply.4 };
    }
    if !has_key.is_null() {
        // SAFETY: caller-owned slot.
        unsafe { *has_key = reply.5 };
    }
    if !public_key_pem.is_null() {
        // SAFETY: caller-owned slot.
        unsafe { *public_key_pem = into_cstring(reply.6) };
    }
    CoreError::Ok
}

/// `SetConfig` equivalent. On `Ok`, `error_message` receives a caller-freed
/// message only when the payload was accepted-but-failed (`ok=false`, e.g. a
/// validation error); on `BadArg`/`Internal` the message explains the ABI
/// failure.
///
/// # Safety
/// `core` must be valid; all strings NUL-terminated UTF-8; `ok`/`error_message`
/// writable.
#[no_mangle]
pub unsafe extern "C" fn core_set_config(
    core: *mut Core,
    vin: *const c_char,
    model: *const c_char,
    key_name: *const c_char,
    connect_timeout_sec: i32,
    command_timeout_sec: i32,
    ok: *mut bool,
    error_message: *mut *mut c_char,
) -> CoreError {
    let Some(core) = (unsafe { core.as_ref() }) else {
        return CoreError::BadArg;
    };
    let Some(vin) = cstr(vin) else {
        return CoreError::BadArg;
    };
    let model = cstr(model).unwrap_or_default();
    let key_name = cstr(key_name).unwrap_or_default();

    match core.set_config(
        &vin,
        &model,
        &key_name,
        connect_timeout_sec,
        command_timeout_sec,
    ) {
        Ok(()) => {
            if !ok.is_null() {
                // SAFETY: caller-owned slot.
                unsafe { *ok = true };
            }
            if !error_message.is_null() {
                // SAFETY: caller-owned slot.
                unsafe { *error_message = into_cstring(String::new()) };
            }
        }
        Err(msg) => {
            if !ok.is_null() {
                // SAFETY: caller-owned slot.
                unsafe { *ok = false };
            }
            if !error_message.is_null() {
                // SAFETY: caller-owned slot.
                unsafe { *error_message = into_cstring(msg.to_string()) };
            }
        }
    }
    CoreError::Ok
}

/// `GenerateKey` equivalent. On success `ok=true` and `public_key_pem` holds the
/// PEM; on a refused generation `ok=false` and `error_message` explains why.
/// ABI failures are `BadArg`/`Internal`.
///
/// # Safety
/// `core` must be valid; `force` is an int 0/1; output pointers writable/NULL.
#[no_mangle]
pub unsafe extern "C" fn core_generate_key(
    core: *mut Core,
    force: bool,
    ok: *mut bool,
    public_key_pem: *mut *mut c_char,
    error_message: *mut *mut c_char,
) -> CoreError {
    let Some(core) = (unsafe { core.as_ref() }) else {
        return CoreError::BadArg;
    };
    match core.generate_key(force) {
        Ok(pubkey) => {
            if !ok.is_null() {
                // SAFETY: caller-owned slot.
                unsafe { *ok = true };
            }
            if !public_key_pem.is_null() {
                // SAFETY: caller-owned slot.
                unsafe { *public_key_pem = into_cstring(pubkey) };
            }
            if !error_message.is_null() {
                // SAFETY: caller-owned slot.
                unsafe { *error_message = into_cstring(String::new()) };
            }
        }
        Err(err) => {
            if !ok.is_null() {
                // SAFETY: caller-owned slot.
                unsafe { *ok = false };
            }
            if !public_key_pem.is_null() {
                // SAFETY: caller-owned slot.
                unsafe { *public_key_pem = into_cstring(String::new()) };
            }
            if !error_message.is_null() {
                // SAFETY: caller-owned slot.
                unsafe { *error_message = into_cstring(err.to_string()) };
            }
        }
    }
    CoreError::Ok
}

/// Pair equivalent. Same shape as `core_generate_key`.
///
/// # Safety
/// `core` must be valid; output pointers writable/NULL.
#[no_mangle]
pub unsafe extern "C" fn core_pair(
    core: *mut Core,
    ok: *mut bool,
    stdout_out: *mut *mut c_char,
    error_message: *mut *mut c_char,
) -> CoreError {
    let Some(core) = (unsafe { core.as_ref() }) else {
        return CoreError::BadArg;
    };
    match core.pair() {
        Ok((success, output, err)) => {
            if !ok.is_null() {
                // SAFETY: caller-owned slot.
                unsafe { *ok = success };
            }
            if !stdout_out.is_null() {
                // SAFETY: caller-owned slot.
                unsafe { *stdout_out = into_cstring(output) };
            }
            if !error_message.is_null() {
                // SAFETY: caller-owned slot.
                unsafe { *error_message = into_cstring(err) };
            }
            CoreError::Ok
        }
        Err(e) => {
            if !ok.is_null() {
                // SAFETY: caller-owned slot.
                unsafe { *ok = false };
            }
            if !error_message.is_null() {
                // SAFETY: caller-owned slot.
                unsafe { *error_message = err_str(&e.to_string()) };
            }
            CoreError::Ok
        }
    }
}

/// Starts automatic phone-key presence mode. `active` reports whether the
/// service is running; an inactive result may carry a caller-freed reason.
///
/// # Safety
/// `core` must be valid; output pointers writable or NULL.
#[no_mangle]
pub unsafe extern "C" fn core_start_phone_key(
    core: *mut Core,
    active: *mut bool,
    error_message: *mut *mut c_char,
) -> CoreError {
    let Some(core) = (unsafe { core.as_ref() }) else {
        return CoreError::BadArg;
    };
    match core.start_phone_key() {
        Ok(()) => {
            if !active.is_null() {
                unsafe { *active = true };
            }
            if !error_message.is_null() {
                unsafe { *error_message = into_cstring(String::new()) };
            }
        }
        Err(e) => {
            if !active.is_null() {
                unsafe { *active = false };
            }
            if !error_message.is_null() {
                unsafe { *error_message = into_cstring(e.to_string()) };
            }
        }
    }
    CoreError::Ok
}

/// Notifies the core that the device resumed from system suspend.
///
/// Kills any idle `tesla-session` child whose `org.bluez` system-bus socket
/// is likely stale after the freezer, clears queued stale events, and
/// best-effort restarts phone-key presence if the current config is paired.
/// Idempotent. Never fails except for a NULL handle.
///
/// # Safety
/// `core` must be a pointer returned by `core_new`, or NULL.
#[no_mangle]
pub unsafe extern "C" fn core_handle_resume(core: *mut Core) -> CoreError {
    let Some(core) = (unsafe { core.as_ref() }) else {
        return CoreError::BadArg;
    };
    core.handle_resume();
    CoreError::Ok
}

/// Pops one queued phone-key event without blocking.
///
/// # Safety
/// `core` must be valid; output pointers writable or NULL. Returned strings
/// must be released with `core_string_free`.
#[no_mangle]
pub unsafe extern "C" fn core_poll_phone_key_event(
    core: *mut Core,
    has_event: *mut bool,
    kind: *mut *mut c_char,
    vin: *mut *mut c_char,
    time: *mut *mut c_char,
    error_message: *mut *mut c_char,
) -> CoreError {
    let Some(core) = (unsafe { core.as_ref() }) else {
        return CoreError::BadArg;
    };
    let event = core.poll_phone_key_event();
    if !has_event.is_null() {
        unsafe { *has_event = event.is_some() };
    }
    if let Some(event) = event {
        if !kind.is_null() {
            unsafe { *kind = into_cstring(event.kind) };
        }
        if !vin.is_null() {
            unsafe { *vin = into_cstring(event.vin) };
        }
        if !time.is_null() {
            unsafe { *time = into_cstring(event.time) };
        }
        if !error_message.is_null() {
            unsafe { *error_message = into_cstring(event.error) };
        }
    }
    CoreError::Ok
}

/// Run a command. `args` is a NULL-terminated array of NUL-terminated UTF-8
/// strings (an empty array = a single NULL element). Results are written to
/// the optional output slots (`ok`, `out_stdout`, `out_stderr`,
/// `out_exit_code`); a *hard* failure (unknown command, busy adapter, refused
/// for policy) is reported through `error_message` (caller-freed) rather than
/// as command output. On an ABI-level failure (bad args, dead core) the
/// function returns `BadArg`/`Internal` and no outputs are valid.
///
/// # Safety
/// `core` must be valid; `args` must be a NULL-terminated array; string
/// output pointers writable/NULL.
#[no_mangle]
pub unsafe extern "C" fn core_run(
    core: *mut Core,
    cmd: *const c_char,
    args: *const *const c_char,
    ok: *mut bool,
    out_stdout: *mut *mut c_char,
    out_stderr: *mut *mut c_char,
    out_exit_code: *mut i32,
    error_message: *mut *mut c_char,
) -> CoreError {
    let Some(core) = (unsafe { core.as_ref() }) else {
        return CoreError::BadArg;
    };
    let Some(cmd) = cstr(cmd) else {
        return CoreError::BadArg;
    };

    // Walk the NULL-terminated argv array.
    let mut cmd_args: Vec<String> = Vec::new();
    if !args.is_null() {
        let mut i = 0usize;
        loop {
            // SAFETY: the array is NULL-terminated (caller contract).
            let p = unsafe { *args.add(i) };
            if p.is_null() {
                break;
            }
            let Some(arg) = cstr(p) else {
                return CoreError::BadArg;
            };
            cmd_args.push(arg);
            i += 1;
        }
    }

    match core.run(&cmd, &cmd_args) {
        Ok((success, so, se, rc)) => {
            if !ok.is_null() {
                // SAFETY: caller-owned slot.
                unsafe { *ok = success };
            }
            if !out_stdout.is_null() {
                // SAFETY: caller-owned slot.
                unsafe { *out_stdout = into_cstring(so) };
            }
            if !out_stderr.is_null() {
                // SAFETY: caller-owned slot.
                unsafe { *out_stderr = into_cstring(se) };
            }
            if !out_exit_code.is_null() {
                // SAFETY: caller-owned slot.
                unsafe { *out_exit_code = rc };
            }
            CoreError::Ok
        }
        Err(e) => {
            if !ok.is_null() {
                // SAFETY: caller-owned slot.
                unsafe { *ok = false };
            }
            if !error_message.is_null() {
                // SAFETY: caller-owned slot.
                unsafe { *error_message = err_str(&e.to_string()) };
            }
            CoreError::Ok
        }
    }
}

/// Preview a navigation share without sending: parses `text` and reports
/// (kind, value1, value2) = ("gps", lat, lon) or ("address", text, "").
/// A parse failure is `ok=false` + `error_message`, same soft shape as
/// `core_generate_key`.
///
/// # Safety
/// `core` must be valid; strings NUL-terminated UTF-8; outputs writable/NULL.
#[no_mangle]
pub unsafe extern "C" fn core_preview_destination(
    core: *mut Core,
    text: *const c_char,
    ok: *mut bool,
    kind: *mut *mut c_char,
    value1: *mut *mut c_char,
    value2: *mut *mut c_char,
    error_message: *mut *mut c_char,
) -> CoreError {
    let Some(_) = (unsafe { core.as_ref() }) else {
        return CoreError::BadArg;
    };
    let Some(text) = cstr(text) else {
        return CoreError::BadArg;
    };
    match Core::preview_destination(&text) {
        Ok((k, v1, v2)) => {
            if !ok.is_null() {
                // SAFETY: caller-owned slot.
                unsafe { *ok = true };
            }
            if !kind.is_null() {
                // SAFETY: caller-owned slot.
                unsafe { *kind = into_cstring(k) };
            }
            if !value1.is_null() {
                // SAFETY: caller-owned slot.
                unsafe { *value1 = into_cstring(v1) };
            }
            if !value2.is_null() {
                // SAFETY: caller-owned slot.
                unsafe { *value2 = into_cstring(v2) };
            }
            if !error_message.is_null() {
                // SAFETY: caller-owned slot.
                unsafe { *error_message = into_cstring(String::new()) };
            }
        }
        Err(e) => {
            if !ok.is_null() {
                // SAFETY: caller-owned slot.
                unsafe { *ok = false };
            }
            if !error_message.is_null() {
                // SAFETY: caller-owned slot.
                unsafe { *error_message = into_cstring(e.to_string()) };
            }
        }
    }
    CoreError::Ok
}

/// Send shared text to the car navigation over BLE (session child).
/// Same output shape as `core_pair`.
///
/// # Safety
/// `core` must be valid; strings NUL-terminated UTF-8; outputs writable/NULL.
#[no_mangle]
pub unsafe extern "C" fn core_share_destination(
    core: *mut Core,
    text: *const c_char,
    ok: *mut bool,
    stdout_out: *mut *mut c_char,
    error_message: *mut *mut c_char,
) -> CoreError {
    let Some(core) = (unsafe { core.as_ref() }) else {
        return CoreError::BadArg;
    };
    let Some(text) = cstr(text) else {
        return CoreError::BadArg;
    };
    match core.share_destination(&text) {
        Ok((success, out, err)) => {
            if !ok.is_null() {
                // SAFETY: caller-owned slot.
                unsafe { *ok = success };
            }
            if !stdout_out.is_null() {
                // SAFETY: caller-owned slot.
                unsafe { *stdout_out = into_cstring(out) };
            }
            if !error_message.is_null() {
                // SAFETY: caller-owned slot.
                unsafe { *error_message = into_cstring(err) };
            }
            CoreError::Ok
        }
        Err(e) => {
            if !ok.is_null() {
                // SAFETY: caller-owned slot.
                unsafe { *ok = false };
            }
            if !error_message.is_null() {
                // SAFETY: caller-owned slot.
                unsafe { *error_message = err_str(&e.to_string()) };
            }
            CoreError::Ok
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::raw::c_char;
    use std::ptr;

    fn tmp_core() -> (*mut Core, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!("electric-eel-ffitest-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let bin_dir = dir.join("bin");
        std::fs::create_dir_all(&bin_dir).unwrap();
        let bin_dir = CString::new(bin_dir.to_string_lossy().as_bytes()).unwrap();
        let state_dir = CString::new(dir.to_string_lossy().as_bytes()).unwrap();
        // SAFETY: owned, NUL-terminated strings; err slot valid.
        let core = unsafe {
            core_new(
                bin_dir.as_ptr(),
                state_dir.as_ptr(),
                ptr::null(),
                ptr::null(),
                ptr::null_mut(),
            )
        };
        (core, dir)
    }

    #[test]
    fn test_version_is_c_string() {
        // SAFETY: static, never freed.
        let v = unsafe { CStr::from_ptr(core_version()) }.to_str().unwrap();
        assert!(v.contains('.'));
    }

    #[test]
    fn test_core_new_free_null_bin_dir() {
        // SAFETY: NULL bin_dir, valid err slot.
        let mut err: *mut c_char = ptr::null_mut();
        let core = unsafe {
            core_new(
                ptr::null(),
                ptr::null(),
                ptr::null(),
                ptr::null(),
                ptr::addr_of_mut!(err),
            )
        };
        assert!(core.is_null());
        assert!(!err.is_null());
        // SAFETY: from core_new's err slot.
        unsafe { core_string_free(err) };
    }

    #[test]
    fn test_get_status_roundtrip() {
        let (core, _dir) = tmp_core();
        assert!(!core.is_null());
        // SAFETY: valid core, all output slots valid.
        let mut vin: *mut c_char = ptr::null_mut();
        let mut model: *mut c_char = ptr::null_mut();
        let mut key_name: *mut c_char = ptr::null_mut();
        let mut ct: i32 = 0;
        let mut cmdt: i32 = 0;
        let mut has_key = false;
        let mut pem: *mut c_char = ptr::null_mut();
        let rc = unsafe {
            core_get_status(
                core,
                ptr::addr_of_mut!(vin),
                ptr::addr_of_mut!(model),
                ptr::addr_of_mut!(key_name),
                ptr::addr_of_mut!(ct),
                ptr::addr_of_mut!(cmdt),
                ptr::addr_of_mut!(has_key),
                ptr::addr_of_mut!(pem),
            )
        };
        assert_eq!(rc, CoreError::Ok);
        assert_eq!(ct, 20, "default connect timeout");
        assert!(!has_key, "no key by default");
        // SAFETY: strings owned by the callee, freed by us.
        unsafe {
            core_string_free(vin);
            core_string_free(model);
            core_string_free(key_name);
            core_string_free(pem);
        }
    }

    #[test]
    fn test_set_config_and_run_unknown_command() {
        let (core, _dir) = tmp_core();
        assert!(!core.is_null());
        let vin = CString::new("5YJ3E1EA0PF000000").unwrap();
        let model = CString::new("").unwrap();
        let key_name = CString::new("harbour-electric-eel").unwrap();
        // SAFETY: valid core + strings, ok slot valid.
        let mut ok = false;
        let mut msg: *mut c_char = ptr::null_mut();
        let rc = unsafe {
            core_set_config(
                core,
                vin.as_ptr(),
                model.as_ptr(),
                key_name.as_ptr(),
                20,
                5,
                ptr::addr_of_mut!(ok),
                ptr::addr_of_mut!(msg),
            )
        };
        assert_eq!(rc, CoreError::Ok);
        assert!(ok);
        // SAFETY: owned by callee.
        unsafe { core_string_free(msg) };

        // Unknown command -> ABI Ok with ok=false and an error_message,
        // mirroring the daemon's HelperError::UnknownCommand.
        let cmd = CString::new("definitely-not-a-command").unwrap();
        let mut run_ok = true;
        let mut run_stdout: *mut c_char = ptr::null_mut();
        let mut run_stderr: *mut c_char = ptr::null_mut();
        let mut run_code = 0;
        let mut run_err: *mut c_char = ptr::null_mut();
        // SAFETY: valid core, NULL-terminated empty argv.
        let rc = unsafe {
            core_run(
                core,
                cmd.as_ptr(),
                ptr::null(),
                ptr::addr_of_mut!(run_ok),
                ptr::addr_of_mut!(run_stdout),
                ptr::addr_of_mut!(run_stderr),
                ptr::addr_of_mut!(run_code),
                ptr::addr_of_mut!(run_err),
            )
        };
        assert_eq!(rc, CoreError::Ok);
        assert!(!run_ok, "unknown command must be refused");
        assert!(!run_err.is_null(), "error message should carry the reason");
        // SAFETY: owned by callee.
        unsafe {
            core_string_free(run_stdout);
            core_string_free(run_stderr);
            core_string_free(run_err);
            core_free(core);
        }
    }

    #[test]
    fn test_phone_key_start_and_empty_event_queue() {
        let (core, _dir) = tmp_core();
        assert!(!core.is_null());
        let mut active = true;
        let mut error: *mut c_char = ptr::null_mut();
        let rc = unsafe {
            core_start_phone_key(core, ptr::addr_of_mut!(active), ptr::addr_of_mut!(error))
        };
        assert_eq!(rc, CoreError::Ok);
        assert!(!active);
        assert!(!error.is_null());

        let mut has_event = true;
        let rc = unsafe {
            core_poll_phone_key_event(
                core,
                ptr::addr_of_mut!(has_event),
                ptr::null_mut(),
                ptr::null_mut(),
                ptr::null_mut(),
                ptr::null_mut(),
            )
        };
        assert_eq!(rc, CoreError::Ok);
        assert!(!has_event);
        unsafe {
            core_string_free(error);
            core_free(core);
        }
    }

    #[test]
    fn test_handle_resume_idempotent() {
        let (core, _dir) = tmp_core();
        assert!(!core.is_null());
        let rc = unsafe { core_handle_resume(core) };
        assert_eq!(rc, CoreError::Ok);
        // second call must remain idempotent
        let rc = unsafe { core_handle_resume(core) };
        assert_eq!(rc, CoreError::Ok);
        // null handle is BadArg
        let rc = unsafe { core_handle_resume(ptr::null_mut()) };
        assert_eq!(rc, CoreError::BadArg);
        unsafe { core_free(core) };
    }
}
