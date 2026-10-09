//! In-process control core for harbour-electric-eel (see `docs/architecture.md`
//! for why it's a staticlib + C ABI instead of a D-Bus daemon).
//!
//! Built two ways:
//! - As a staticlib (`crate-type = ["staticlib"]`) linked into the app,
//!   providing the Rust application entrypoint with `app-entry`. Rust owns
//!   the runtime and threads; Qt only submits actions and renders notifications
//!   through the C ABI in `ffi.rs`.
//! - As an rlib used by the daemon binary (`main.rs`), which with the `dbus`
//!   feature adds `helper.rs`'s D-Bus surface on top of the same `Core`.
//!
//! The daemon-only modules (`helper`, `authorize`) are feature-gated so the
//! app's staticlib carries the zbus MCE client but no daemon server or
//! caller-authorization plumbing.

pub mod child;
pub mod commands;
pub mod config;
pub mod core;
pub mod error;
pub mod ffi;
pub mod keylog;
pub mod session_client;
pub mod share;

#[cfg(feature = "runtime")]
mod cpukeepalive;
#[cfg(feature = "runtime")]
pub mod runtime;

#[cfg(all(feature = "app-entry", not(test)))]
mod app;

#[cfg(feature = "dbus")]
pub mod authorize;
#[cfg(feature = "dbus")]
pub mod helper;
