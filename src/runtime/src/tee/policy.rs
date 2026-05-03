//! Attestation verification policy.
//!
//! Defines the rules for accepting or rejecting an SNP attestation report.
//! The verifier checks the report against these policies after validating
//! the cryptographic signature and certificate chain.

use serde::{Deserialize, Serialize};

/// Policy for verifying SNP attestation reports.
///
/// Each field is optional — only set fields are checked. This allows
/// flexible policies from "accept any valid report" to strict
/// production requirements.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AttestationPolicy {
    /// Expected launch measurement (SHA-384 of initial guest memory).
    /// Hex-encoded, 96 characters (48 bytes). If set, the report's
    /// measurement must match exactly.
    #[serde(default)]
    pub expected_measurement: Option<String>,

    /// Minimum TCB version requirements. Each component is checked
    /// independently — the report's value must be >= the policy value.
    #[serde(default)]
    pub min_tcb: Option<MinTcbPolicy>,

    /// Require debug mode to be disabled (bit 0 of guest policy).
    /// Should be `true` for production deployments.
    #[serde(default = "default_true")]
    pub require_no_debug: bool,

    /// Require SMT (Simultaneous Multi-Threading) to be disabled.
    /// Some security-sensitive workloads disable SMT to prevent
    /// side-channel attacks.
    #[serde(default)]
    pub require_no_smt: bool,

    /// Allowed guest policy bitmask. If set, the report's policy
    /// field is ANDed with this mask and must equal the mask.
    #[serde(default)]
    pub allowed_policy_mask: Option<u64>,

    /// Maximum age of the report in seconds. If set, the verifier
    /// rejects reports older than this threshold.
    #[serde(default)]
    pub max_report_age_secs: Option<u64>,
}

impl Default for AttestationPolicy {
    fn default() -> Self {
        Self {
            expected_measurement: None,
            min_tcb: None,
            require_no_debug: true,
            require_no_smt: false,
            allowed_policy_mask: None,
            max_report_age_secs: None,
        }
    }
}

/// Minimum TCB (Trusted Computing Base) version requirements.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MinTcbPolicy {
    /// Minimum boot loader SVN.
    #[serde(default)]
    pub boot_loader: Option<u8>,
    /// Minimum TEE (PSP) SVN.
    #[serde(default)]
    pub tee: Option<u8>,
    /// Minimum SNP firmware SVN.
    #[serde(default)]
    pub snp: Option<u8>,
    /// Minimum CPU microcode SVN.
    #[serde(default)]
    pub microcode: Option<u8>,
}

fn default_true() -> bool {
    true
}

/// Result of policy verification against an attestation report.
#[derive(Debug, Clone)]
pub struct PolicyResult {
    /// Whether all policy checks passed.
    pub passed: bool,
    /// List of policy violations (empty if passed).
    pub violations: Vec<PolicyViolation>,
}

/// A specific policy violation found during verification.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PolicyViolation {
    /// Which policy check failed.
    pub check: String,
    /// Human-readable description of the violation.
    pub reason: String,
}

impl std::fmt::Display for PolicyViolation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.check, self.reason)
    }
}

impl PolicyResult {
    /// Create a passing result.
    pub fn pass() -> Self {
        Self {
            passed: true,
            violations: Vec::new(),
        }
    }

    /// Create a result from a list of violations.
    pub fn from_violations(violations: Vec<PolicyViolation>) -> Self {
        Self {
            passed: violations.is_empty(),
            violations,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_policy() {
        let policy = AttestationPolicy::default();
        assert!(policy.expected_measurement.is_none());
        assert!(policy.min_tcb.is_none());
        assert!(policy.require_no_debug); // default true
        assert!(!policy.require_no_smt);
        assert!(policy.allowed_policy_mask.is_none());
        assert!(policy.max_report_age_secs.is_none());
    }

    #[test]
    fn test_policy_serialization() {
        let policy = AttestationPolicy {
            expected_measurement: Some("ab".repeat(48)),
            require_no_debug: true,
            require_no_smt: true,
            min_tcb: Some(MinTcbPolicy {
                snp: Some(8),
                microcode: Some(115),
                ..Default::default()
            }),
            ..Default::default()
        };
        let json = serde_json::to_string(&policy).unwrap();
        let parsed: AttestationPolicy = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.expected_measurement, policy.expected_measurement);
        assert!(parsed.require_no_smt);
        assert_eq!(parsed.min_tcb.unwrap().snp, Some(8));
    }

    #[test]
    fn test_policy_result_pass() {
        let result = PolicyResult::pass();
        assert!(result.passed);
        assert!(result.violations.is_empty());
    }

    #[test]
    fn test_policy_result_with_violations() {
        let violations = vec![
            PolicyViolation {
                check: "measurement".to_string(),
                reason: "Mismatch".to_string(),
            },
            PolicyViolation {
                check: "debug".to_string(),
                reason: "Debug mode enabled".to_string(),
            },
        ];
        let result = PolicyResult::from_violations(violations);
        assert!(!result.passed);
        assert_eq!(result.violations.len(), 2);
    }

    #[test]
    fn test_policy_violation_display() {
        let v = PolicyViolation {
            check: "tcb".to_string(),
            reason: "SNP version too low".to_string(),
        };
        assert_eq!(v.to_string(), "tcb: SNP version too low");
    }

    #[test]
    fn test_min_tcb_policy_default() {
        let tcb = MinTcbPolicy::default();
        assert!(tcb.boot_loader.is_none());
        assert!(tcb.tee.is_none());
        assert!(tcb.snp.is_none());
        assert!(tcb.microcode.is_none());
    }

    #[test]
    fn test_min_tcb_policy_serde_roundtrip() {
        let tcb = MinTcbPolicy {
            boot_loader: Some(3),
            tee: Some(0),
            snp: Some(8),
            microcode: Some(115),
        };
        let json = serde_json::to_string(&tcb).unwrap();
        let parsed: MinTcbPolicy = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.boot_loader, Some(3));
        assert_eq!(parsed.tee, Some(0));
        assert_eq!(parsed.snp, Some(8));
        assert_eq!(parsed.microcode, Some(115));
    }

    #[test]
    fn test_policy_violation_debug() {
        let v = PolicyViolation {
            check: "tcb".to_string(),
            reason: "SNP version too low".to_string(),
        };
        let debug_str = format!("{:?}", v);
        assert!(debug_str.contains("check: \"tcb\""));
        assert!(debug_str.contains("reason: \"SNP version too low\""));
    }

    #[test]
    fn test_policy_result_debug() {
        let result = PolicyResult::pass();
        let debug_str = format!("{:?}", result);
        assert!(debug_str.contains("passed: true"));
    }

    #[test]
    fn test_policy_result_from_violations_empty() {
        let result = PolicyResult::from_violations(vec![]);
        assert!(result.passed);
        assert!(result.violations.is_empty());
    }

    #[test]
    fn test_attestation_policy_clone() {
        let policy = AttestationPolicy {
            expected_measurement: Some("abc123".to_string()),
            min_tcb: Some(MinTcbPolicy {
                snp: Some(8),
                ..Default::default()
            }),
            require_no_debug: true,
            require_no_smt: true,
            allowed_policy_mask: Some(0xFFFF),
            max_report_age_secs: Some(3600),
        };
        let cloned = policy.clone();
        assert_eq!(cloned.expected_measurement, policy.expected_measurement);
        assert_eq!(cloned.require_no_debug, policy.require_no_debug);
        assert_eq!(cloned.require_no_smt, policy.require_no_smt);
        assert_eq!(cloned.allowed_policy_mask, policy.allowed_policy_mask);
        assert_eq!(cloned.max_report_age_secs, policy.max_report_age_secs);
    }

    #[test]
    fn test_attestation_policy_debug() {
        let policy = AttestationPolicy::default();
        let debug_str = format!("{:?}", policy);
        assert!(debug_str.contains("AttestationPolicy"));
    }
}
