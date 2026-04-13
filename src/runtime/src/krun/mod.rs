//! Krun module - libkrun integration for MicroVM management.
//!
//! This module provides a safe wrapper around libkrun FFI bindings
//! for creating and managing MicroVMs.

mod context;

pub use context::KrunContext;

use a3s_box_core::error::{BoxError, Result};

/// Check libkrun FFI call status and convert to Result.
pub fn check_status(fn_name: &str, status: i32) -> Result<()> {
    if status < 0 {
        tracing::error!(status, fn_name, "libkrun call failed");
        Err(BoxError::BoxBootError {
            message: format!("{} failed with status {}", fn_name, status),
            hint: Some("Check libkrun installation and VM configuration".to_string()),
        })
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_check_status_success_zero() {
        assert!(check_status("test_fn", 0).is_ok());
    }

    #[test]
    fn test_check_status_success_positive() {
        assert!(check_status("test_fn", 1).is_ok());
        assert!(check_status("test_fn", 100).is_ok());
    }

    #[test]
    fn test_check_status_failure_negative_one() {
        let result = check_status("create_vm", -1);
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("create_vm"));
        assert!(err.contains("-1"));
    }

    #[test]
    fn test_check_status_failure_negative_large() {
        let result = check_status("init_vm", -50);
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("init_vm"));
        assert!(err.contains("-50"));
    }

    #[test]
    fn test_check_status_error_message_format() {
        let result = check_status("start_vm", -99);
        let err = result.unwrap_err().to_string();
        assert!(err.contains("start_vm failed with status -99"));
    }
}
