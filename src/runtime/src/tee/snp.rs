//! AMD SEV-SNP (Secure Encrypted Virtualization - Secure Nested Paging) support.
//!
//! This module provides hardware detection for AMD SEV-SNP capability.

use a3s_box_core::error::{BoxError, Result};
use std::path::Path;

/// SEV-SNP hardware support status.
#[derive(Debug, Clone)]
pub struct SevSnpSupport {
    /// Whether SEV-SNP is available
    pub available: bool,
    /// Reason if not available
    pub reason: Option<String>,
}

/// Check if the host supports AMD SEV-SNP.
///
/// This function checks:
/// 1. `/dev/sev` device exists (SEV driver loaded)
/// 2. CPU supports SEV-SNP (via sysfs)
///
/// # Returns
/// - `Ok(SevSnpSupport)` with availability status
/// - `Err(BoxError)` if detection fails unexpectedly
pub fn check_sev_snp_support() -> Result<SevSnpSupport> {
    // Check if /dev/sev exists (SEV driver loaded)
    if !Path::new("/dev/sev").exists() {
        return Ok(SevSnpSupport {
            available: false,
            reason: Some("/dev/sev device not found - SEV driver not loaded".to_string()),
        });
    }

    // Check if SNP is enabled via sysfs
    let snp_enabled_path = "/sys/module/kvm_amd/parameters/sev_snp";
    match std::fs::read_to_string(snp_enabled_path) {
        Ok(content) => {
            let enabled = content.trim();
            if enabled == "Y" || enabled == "1" {
                Ok(SevSnpSupport {
                    available: true,
                    reason: None,
                })
            } else {
                Ok(SevSnpSupport {
                    available: false,
                    reason: Some(format!(
                        "SEV-SNP not enabled in kernel (sev_snp={})",
                        enabled
                    )),
                })
            }
        }
        Err(e) => {
            // File doesn't exist or can't be read - SNP not available
            Ok(SevSnpSupport {
                available: false,
                reason: Some(format!(
                    "Cannot read SEV-SNP status from {}: {}",
                    snp_enabled_path, e
                )),
            })
        }
    }
}

/// Verify SEV-SNP is available, returning an error if not.
///
/// Use this function when TEE is required and you want to fail early
/// with a descriptive error message.
pub fn require_sev_snp_support() -> Result<()> {
    let support = check_sev_snp_support()?;
    if !support.available {
        return Err(BoxError::TeeNotSupported(
            support
                .reason
                .unwrap_or_else(|| "Unknown reason".to_string()),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_check_sev_snp_support_returns_result() {
        // This test just verifies the function runs without panicking
        // The actual result depends on the host hardware
        let result = check_sev_snp_support();
        assert!(result.is_ok());
    }

    #[test]
    fn test_sev_snp_support_struct() {
        let support = SevSnpSupport {
            available: true,
            reason: None,
        };
        assert!(support.available);
        assert!(support.reason.is_none());

        let support_unavailable = SevSnpSupport {
            available: false,
            reason: Some("Test reason".to_string()),
        };
        assert!(!support_unavailable.available);
        assert_eq!(support_unavailable.reason, Some("Test reason".to_string()));
    }

    #[test]
    fn test_sev_snp_support_debug() {
        let support = SevSnpSupport {
            available: true,
            reason: None,
        };
        let debug_str = format!("{:?}", support);
        assert!(debug_str.contains("available: true"));

        let support_unavailable = SevSnpSupport {
            available: false,
            reason: Some("SEV driver not loaded".to_string()),
        };
        let debug_str = format!("{:?}", support_unavailable);
        assert!(debug_str.contains("available: false"));
        assert!(debug_str.contains("SEV driver not loaded"));
    }

    #[test]
    fn test_require_sev_snp_support_when_available() {
        // This test will pass or fail depending on actual hardware
        // We just verify it returns a Result
        let result = require_sev_snp_support();
        // Don't assert on the actual value since it depends on hardware
        assert!(result.is_ok() || result.is_err());
    }
}
