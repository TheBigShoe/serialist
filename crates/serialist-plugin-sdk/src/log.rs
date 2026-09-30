//! Lines for the app's log, through the world's one import.
//!
//! The host forwards them to its log with the plugin's name, caps how many one call may
//! write, and keeps the last `error` line of a call to show if the call then traps. On
//! targets other than wasm32 these do nothing, so plugin code runs in native tests.

use crate::bindings::LogLevel;

/// Write `message` at `level`.
pub fn log(level: LogLevel, message: &str) {
    #[cfg(target_arch = "wasm32")]
    crate::bindings::log(level, message);
    #[cfg(not(target_arch = "wasm32"))]
    let _ = (level, message);
}

pub fn debug(message: &str) {
    log(LogLevel::Debug, message);
}

pub fn info(message: &str) {
    log(LogLevel::Info, message);
}

pub fn warn(message: &str) {
    log(LogLevel::Warn, message);
}

pub fn error(message: &str) {
    log(LogLevel::Error, message);
}
