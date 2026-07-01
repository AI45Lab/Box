//! Log path helpers.
//!
//! The log PROCESSOR moved to [`a3s_box_core::log`] and now runs in the SHIM —
//! the box's lifetime process — so a detached `run -d` box's logs are not
//! truncated when the launching CLI exits (the processor used to be a
//! `spawn_blocking` task in the ephemeral CLI). This module re-exports the parts
//! the CLI still needs to locate and read the structured log file.

pub use a3s_box_core::log::{is_runtime_console_noise, json_log_path};
