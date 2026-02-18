//! TEE (Trusted Execution Environment) types and detection.
//!
//! Provides:
//! - [`TeeCapability`] — detected TEE hardware/simulation status
//! - [`detect_tee()`] — probe the current environment for TEE support
//! - [`AttestRequest`] / [`AttestRoute`] — RA-TLS attestation protocol types
//!
//! The attest server runs inside the guest TEE and communicates with
//! host-side clients over TLS (RA-TLS). Inside the TLS tunnel, messages
//! use the `a3s-transport` Frame wire format:
//!
//! - Client sends a [`Data`] frame with JSON [`AttestRequest`]
//! - Server responds with a [`Data`] frame (JSON response) or [`Error`] frame

use serde::{Deserialize, Serialize};

/// Vsock port for the attestation server.
pub const ATTEST_VSOCK_PORT: u32 = a3s_transport::ports::TEE_CHANNEL;

// ---------------------------------------------------------------------------
// TEE self-detection API
// ---------------------------------------------------------------------------

/// The type of TEE environment detected.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TeeType {
    /// AMD SEV-SNP (real hardware).
    SevSnp,
    /// Simulation mode (`A3S_TEE_SIMULATE` env var).
    Simulated,
}

/// Result of probing the current environment for TEE support.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TeeCapability {
    /// Whether a TEE environment is available.
    pub available: bool,
    /// The type of TEE detected (if any).
    pub tee_type: Option<TeeType>,
    /// Whether `/dev/sev-guest` exists (ioctl interface for attestation reports).
    pub sev_guest_device: bool,
    /// Whether `/dev/sev` exists (SEV driver loaded).
    pub sev_device: bool,
    /// Whether simulation mode is active.
    pub simulated: bool,
}

/// Detect TEE capability in the current environment.
///
/// Checks (in order):
/// 1. `A3S_TEE_SIMULATE` env var → simulation mode
/// 2. `/dev/sev-guest` → AMD SEV-SNP with guest attestation support
/// 3. `/dev/sev` → AMD SEV driver loaded
///
/// # Example
///
/// ```rust
/// use a3s_box_core::tee::detect_tee;
///
/// let cap = detect_tee();
/// if cap.available {
///     println!("TEE type: {:?}", cap.tee_type);
/// }
/// ```
pub fn detect_tee() -> TeeCapability {
    let simulated = std::env::var("A3S_TEE_SIMULATE").is_ok();
    let sev_guest_device = std::path::Path::new("/dev/sev-guest").exists();
    let sev_device = std::path::Path::new("/dev/sev").exists();

    let (available, tee_type) = if simulated {
        (true, Some(TeeType::Simulated))
    } else if sev_guest_device || sev_device {
        (true, Some(TeeType::SevSnp))
    } else {
        (false, None)
    };

    TeeCapability {
        available,
        tee_type,
        sev_guest_device,
        sev_device,
        simulated,
    }
}

/// Check if this environment has TEE support (hardware or simulated).
///
/// Convenience wrapper around [`detect_tee()`].
pub fn is_tee_available() -> bool {
    detect_tee().available
}

/// Request sent inside the TLS tunnel (JSON payload of a Data frame).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AttestRequest {
    /// Route determines which handler processes the request.
    pub route: AttestRoute,
    /// JSON-encoded payload specific to the route.
    #[serde(default)]
    pub payload: serde_json::Value,
}

/// Routes available on the attest server (replaces HTTP path routing).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AttestRoute {
    /// Get TEE status.
    Status,
    /// Inject secrets into the guest.
    Secrets,
    /// Seal data bound to TEE identity.
    Seal,
    /// Unseal previously sealed data.
    Unseal,
    /// Forward a message to the local agent for processing.
    Process,
}

#[cfg(test)]
mod tests {
    use super::*;

    // -- TEE detection tests --

    #[test]
    fn test_detect_tee_returns_capability() {
        let cap = detect_tee();
        // On dev machines without SEV hardware and without A3S_TEE_SIMULATE,
        // TEE should not be available (unless the test runner sets the env var).
        assert_eq!(cap.available, cap.tee_type.is_some());
    }

    #[test]
    fn test_is_tee_available_matches_detect() {
        let cap = detect_tee();
        assert_eq!(is_tee_available(), cap.available);
    }

    #[test]
    fn test_tee_capability_serde_roundtrip() {
        let cap = TeeCapability {
            available: true,
            tee_type: Some(TeeType::SevSnp),
            sev_guest_device: true,
            sev_device: true,
            simulated: false,
        };
        let json = serde_json::to_string(&cap).unwrap();
        let parsed: TeeCapability = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, cap);
    }

    #[test]
    fn test_tee_capability_simulated() {
        let cap = TeeCapability {
            available: true,
            tee_type: Some(TeeType::Simulated),
            sev_guest_device: false,
            sev_device: false,
            simulated: true,
        };
        let json = serde_json::to_string(&cap).unwrap();
        assert!(json.contains("\"simulated\""));
        let parsed: TeeCapability = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.tee_type, Some(TeeType::Simulated));
    }

    #[test]
    fn test_tee_capability_none() {
        let cap = TeeCapability {
            available: false,
            tee_type: None,
            sev_guest_device: false,
            sev_device: false,
            simulated: false,
        };
        assert!(!cap.available);
        assert!(cap.tee_type.is_none());
    }

    #[test]
    fn test_tee_type_serde() {
        assert_eq!(
            serde_json::to_string(&TeeType::SevSnp).unwrap(),
            "\"sev_snp\""
        );
        assert_eq!(
            serde_json::to_string(&TeeType::Simulated).unwrap(),
            "\"simulated\""
        );
    }

    // -- Attest protocol tests --

    #[test]
    fn test_attest_vsock_port() {
        assert_eq!(ATTEST_VSOCK_PORT, 4091);
    }

    #[test]
    fn test_attest_request_serde_roundtrip() {
        let req = AttestRequest {
            route: AttestRoute::Status,
            payload: serde_json::Value::Null,
        };
        let json = serde_json::to_string(&req).unwrap();
        let parsed: AttestRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.route, AttestRoute::Status);
    }

    #[test]
    fn test_attest_route_variants() {
        let routes = [
            (AttestRoute::Status, "\"status\""),
            (AttestRoute::Secrets, "\"secrets\""),
            (AttestRoute::Seal, "\"seal\""),
            (AttestRoute::Unseal, "\"unseal\""),
            (AttestRoute::Process, "\"process\""),
        ];
        for (route, expected) in routes {
            let json = serde_json::to_string(&route).unwrap();
            assert_eq!(json, expected);
        }
    }

    #[test]
    fn test_attest_request_with_payload() {
        let req = AttestRequest {
            route: AttestRoute::Seal,
            payload: serde_json::json!({"data": "base64data", "context": "test"}),
        };
        let json = serde_json::to_string(&req).unwrap();
        let parsed: AttestRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.route, AttestRoute::Seal);
        assert_eq!(parsed.payload["context"], "test");
    }

    #[test]
    fn test_attest_request_default_payload() {
        let json = r#"{"route":"status"}"#;
        let req: AttestRequest = serde_json::from_str(json).unwrap();
        assert_eq!(req.route, AttestRoute::Status);
        assert!(req.payload.is_null());
    }
}
