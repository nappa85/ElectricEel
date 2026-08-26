//! In-process control core for harbour-electric-eel (see `BLUEZ_BACKEND_PLAN.md`
//! for why it's a staticlib + C ABI instead of a D-Bus daemon).
//!
//! Built two ways:
//! - As a staticlib (`crate-type = ["staticlib"]`) linked into the app,
//!   driven through the C ABI in `ffi.rs` by a worker thread in
//!   `app/src/teslaclient.cpp`.
//! - As an rlib used by the daemon binary (`main.rs`), which with the `dbus`
//!   feature adds `helper.rs`'s D-Bus surface on top of the same `Core`.
//!
//! The daemon-only modules (`helper`, `authorize`) are feature-gated so the
//! app's staticlib carries neither zbus nor the caller-authorization
//! plumbing.

pub mod commands;
pub mod config;
pub mod core;
pub mod error;
pub mod ffi;
pub mod keylog;
pub mod session_client;

#[cfg(feature = "dbus")]
pub mod authorize;
#[cfg(feature = "dbus")]
pub mod helper;
