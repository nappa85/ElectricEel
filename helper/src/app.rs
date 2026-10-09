//! Executable entrypoint supplied by the Rust static library. Qt provides
//! platform setup and a UI callback; Rust owns Core for the whole UI lifetime.
use std::ffi::{c_void, CStr};
use std::os::raw::{c_char, c_int};

use crate::core::Core;
use crate::runtime::Runtime;
use crate::session_client::SessionClient;

extern "C" {
    fn electric_eel_ui_prepare(argc: c_int, argv: *mut *mut c_char) -> *const c_char;
    fn electric_eel_ui_run(core: *mut c_void) -> c_int;
    fn electric_eel_ui_cleanup();
}

#[export_name = "main"]
pub unsafe extern "C" fn app_main(argument_count: c_int, arguments: *mut *mut c_char) -> c_int {
    // Qt resolves Sailjail's writable paths before we load persisted state.
    let state = unsafe { electric_eel_ui_prepare(argument_count, arguments) };
    if state.is_null() {
        unsafe { electric_eel_ui_cleanup() };
        return 1;
    }
    let state = unsafe { CStr::from_ptr(state) }
        .to_string_lossy()
        .into_owned();
    let bin = "/usr/share/harbour-electric-eel/bin";
    let session = SessionClient::new(
        format!("{bin}/tesla-session").into(),
        "bluez",
        state.clone().into(),
    );
    let result = match Core::new(bin.to_string(), state, Some(session)) {
        Ok(core) => {
            let mut runtime = Box::new(Runtime::new(core));
            // Rust starts all work; Qt only submits actions and receives updates.
            let result = unsafe { electric_eel_ui_run(std::ptr::addr_of_mut!(*runtime).cast()) };
            drop(runtime); // Join Rust threads, stop presence and reap Go.
            result
        }
        Err(error) => {
            eprintln!("ElectricEel: core initialization failed: {error}");
            1
        }
    };
    unsafe { electric_eel_ui_cleanup() };
    result
}
