//! Simulated TEE attestation for development and testing.
//!
//! When `A3S_TEE_SIMULATE=1` is set, the guest attestation server generates
//! fake SNP reports with correct field layout but no hardware signature.
//! The host verifier can accept these with `allow_simulated: true`.

/// Environment variable to enable TEE simulation mode.
pub const TEE_SIMULATE_ENV: &str = "A3S_TEE_SIMULATE";

/// Check if TEE simulation mode is enabled via environment variable.
pub fn is_simulate_mode() -> bool {
    std::env::var(TEE_SIMULATE_ENV)
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false)
}

/// Simulated SNP report version marker.
/// Real SNP reports use version 2; simulated reports use 0xA3 to distinguish.
pub const SIMULATED_REPORT_VERSION: u32 = 0xA3;

/// Simulated chip ID (all 0xA3 bytes, clearly fake).
pub const SIMULATED_CHIP_ID: [u8; 64] = [0xA3; 64];

/// Build a simulated 1184-byte SNP report with the given report_data.
///
/// The report has correct field layout per AMD SEV-SNP ABI spec (Table 21)
/// but uses a marker version (0xA3) and zero signature to indicate simulation.
/// Nonce, measurement, TCB, and policy fields are populated normally so that
/// policy checks still work.
pub fn build_simulated_report(report_data: &[u8; 64]) -> Vec<u8> {
    const SNP_REPORT_SIZE: usize = 1184;
    let mut report = vec![0u8; SNP_REPORT_SIZE];

    // version at 0x00 (4 bytes LE) — use simulated marker
    report[0x00..0x04].copy_from_slice(&SIMULATED_REPORT_VERSION.to_le_bytes());

    // guest_svn at 0x04 (4 bytes LE)
    report[0x04..0x08].copy_from_slice(&1u32.to_le_bytes());

    // policy at 0x08 (8 bytes LE) — no debug, no SMT
    report[0x08..0x10].copy_from_slice(&0u64.to_le_bytes());

    // current_tcb at 0x38 (8 bytes)
    report[0x38] = 3; // boot_loader
    report[0x39] = 0; // tee
    report[0x3E] = 8; // snp
    report[0x3F] = 115; // microcode

    // report_data at 0x50 (64 bytes)
    report[0x50..0x90].copy_from_slice(report_data);

    // measurement at 0x90 (48 bytes) — deterministic fake measurement
    for i in 0..48 {
        report[0x90 + i] = (i as u8).wrapping_mul(0xA3);
    }

    // chip_id at 0x1A0 (64 bytes)
    report[0x1A0..0x1E0].copy_from_slice(&SIMULATED_CHIP_ID);

    // signature at 0x2A0 (512 bytes) — left as zeros (simulation marker)

    report
}

/// Check if an SNP report is a simulated report (version == 0xA3).
pub fn is_simulated_report(report: &[u8]) -> bool {
    if report.len() < 4 {
        return false;
    }
    let version = u32::from_le_bytes(report[0x00..0x04].try_into().unwrap_or([0; 4]));
    version == SIMULATED_REPORT_VERSION
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_simulate_mode_default() {
        // Should be false when env is not set (or set to something else)
        // We can't reliably test env vars in unit tests without side effects,
        // so just test the function exists and returns a bool
        let _ = is_simulate_mode();
    }

    #[test]
    fn test_build_simulated_report_size() {
        let data = [0u8; 64];
        let report = build_simulated_report(&data);
        assert_eq!(report.len(), 1184);
    }

    #[test]
    fn test_build_simulated_report_version() {
        let data = [0u8; 64];
        let report = build_simulated_report(&data);
        let version = u32::from_le_bytes(report[0x00..0x04].try_into().unwrap());
        assert_eq!(version, SIMULATED_REPORT_VERSION);
    }

    #[test]
    fn test_build_simulated_report_contains_nonce() {
        let mut data = [0u8; 64];
        data[0] = 0xDE;
        data[1] = 0xAD;
        data[2] = 0xBE;
        data[3] = 0xEF;
        let report = build_simulated_report(&data);
        assert_eq!(report[0x50], 0xDE);
        assert_eq!(report[0x51], 0xAD);
        assert_eq!(report[0x52], 0xBE);
        assert_eq!(report[0x53], 0xEF);
    }

    #[test]
    fn test_build_simulated_report_tcb() {
        let data = [0u8; 64];
        let report = build_simulated_report(&data);
        assert_eq!(report[0x38], 3); // boot_loader
        assert_eq!(report[0x3E], 8); // snp
        assert_eq!(report[0x3F], 115); // microcode
    }

    #[test]
    fn test_build_simulated_report_chip_id() {
        let data = [0u8; 64];
        let report = build_simulated_report(&data);
        assert_eq!(&report[0x1A0..0x1E0], &SIMULATED_CHIP_ID);
    }

    #[test]
    fn test_build_simulated_report_zero_signature() {
        let data = [0u8; 64];
        let report = build_simulated_report(&data);
        // Signature at 0x2A0, 512 bytes — should all be zero
        assert!(report[0x2A0..].iter().all(|&b| b == 0));
    }

    #[test]
    fn test_is_simulated_report_true() {
        let data = [0u8; 64];
        let report = build_simulated_report(&data);
        assert!(is_simulated_report(&report));
    }

    #[test]
    fn test_is_simulated_report_false() {
        let mut report = vec![0u8; 1184];
        report[0x00..0x04].copy_from_slice(&2u32.to_le_bytes()); // real version
        assert!(!is_simulated_report(&report));
    }

    #[test]
    fn test_is_simulated_report_too_short() {
        assert!(!is_simulated_report(&[0u8; 2]));
    }

    #[test]
    fn test_simulated_report_version_constant() {
        assert_eq!(SIMULATED_REPORT_VERSION, 0xA3);
    }
}
