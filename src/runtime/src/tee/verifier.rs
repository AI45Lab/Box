//! SNP attestation report verifier.
//!
//! Verifies the cryptographic signature, certificate chain, and policy
//! compliance of an AMD SEV-SNP attestation report. This is the core
//! trust anchor — if verification passes, the report was genuinely
//! produced by AMD hardware running the expected workload.

use a3s_box_core::error::{BoxError, Result};

use super::attestation::{parse_platform_info, AttestationReport, PlatformInfo, SNP_REPORT_SIZE};
use super::policy::{AttestationPolicy, PolicyResult, PolicyViolation};
use super::simulate::is_simulated_report;

/// Result of a complete attestation verification.
#[derive(Debug, Clone)]
pub struct VerificationResult {
    /// Whether the report passed all checks (signature + policy + age).
    pub verified: bool,
    /// Platform info extracted from the report.
    pub platform: PlatformInfo,
    /// Policy check result.
    pub policy_result: PolicyResult,
    /// Signature verification passed.
    pub signature_valid: bool,
    /// Certificate chain verification passed.
    pub cert_chain_valid: bool,
    /// Nonce in report matches the expected nonce.
    pub nonce_valid: bool,
    /// Report age is within the allowed threshold (or age check was skipped).
    pub report_age_valid: bool,
    /// Summary of any failures.
    pub failures: Vec<String>,
}

/// Verify an SNP attestation report.
///
/// This performs the complete verification flow:
/// 1. Parse and validate the report structure
/// 2. Verify the nonce matches (anti-replay)
/// 3. Verify the ECDSA-P384 signature using the VCEK public key
/// 4. Verify the certificate chain (VCEK → ASK → ARK)
/// 5. Check the report against the attestation policy
/// 6. Check report age (if `nonce_issued_at` and `max_report_age_secs` are set)
///
/// If `allow_simulated` is true and the report has the simulated version
/// marker (0xA3), signature and cert chain verification are skipped.
/// Nonce and policy checks still apply.
///
/// # Arguments
/// * `report` - The attestation report from the guest
/// * `expected_nonce` - The nonce that was sent in the request
/// * `policy` - The verification policy to check against
/// * `allow_simulated` - Whether to accept simulated (non-hardware) reports
pub fn verify_attestation(
    report: &AttestationReport,
    expected_nonce: &[u8],
    policy: &AttestationPolicy,
    allow_simulated: bool,
) -> Result<VerificationResult> {
    verify_attestation_with_time(report, expected_nonce, policy, allow_simulated, None)
}

/// Verify an SNP attestation report with optional replay protection.
///
/// Same as [`verify_attestation`], but accepts `nonce_issued_at` — the Unix
/// timestamp (seconds) when the nonce was generated. When combined with
/// `policy.max_report_age_secs`, this rejects stale reports that could be
/// replayed by an attacker.
///
/// # Arguments
/// * `report` - The attestation report from the guest
/// * `expected_nonce` - The nonce that was sent in the request
/// * `policy` - The verification policy to check against
/// * `allow_simulated` - Whether to accept simulated (non-hardware) reports
/// * `nonce_issued_at` - Unix timestamp (seconds) when the nonce was created.
///   If `None`, report age checking is skipped even if `max_report_age_secs` is set.
pub fn verify_attestation_with_time(
    report: &AttestationReport,
    expected_nonce: &[u8],
    policy: &AttestationPolicy,
    allow_simulated: bool,
    nonce_issued_at: Option<u64>,
) -> Result<VerificationResult> {
    let mut failures = Vec::new();

    // 1. Parse report structure
    let platform = parse_platform_info(&report.report).ok_or_else(|| {
        BoxError::AttestationError(format!(
            "Invalid SNP report: expected {} bytes, got {}",
            SNP_REPORT_SIZE,
            report.report.len()
        ))
    })?;

    // Check if this is a simulated report
    let simulated = is_simulated_report(&report.report);
    if simulated && !allow_simulated {
        return Err(BoxError::AttestationError(
            "Simulated report rejected: allow_simulated is false".to_string(),
        ));
    }
    if simulated {
        tracing::warn!("Accepting simulated TEE report (not hardware-attested)");
    }

    // 2. Verify nonce (report_data field at offset 0x50, 64 bytes)
    let nonce_valid = verify_nonce(&report.report, expected_nonce);
    if !nonce_valid {
        failures.push("Nonce mismatch: report_data does not contain expected nonce".to_string());
    }

    // 3. Verify ECDSA-P384 signature (skip for simulated reports)
    let signature_valid = if simulated {
        true
    } else {
        let valid = verify_report_signature(&report.report, &report.cert_chain.vcek);
        if !valid {
            failures.push("Signature verification failed".to_string());
        }
        valid
    };

    // 4. Verify certificate chain (skip for simulated reports)
    let cert_chain_valid = if simulated {
        true
    } else {
        let valid = verify_cert_chain(
            &report.cert_chain.vcek,
            &report.cert_chain.ask,
            &report.cert_chain.ark,
        );
        if !valid {
            failures.push("Certificate chain verification failed".to_string());
        }
        valid
    };

    // 5. Check policy
    let policy_result = check_policy(&platform, policy);
    if !policy_result.passed {
        for v in &policy_result.violations {
            failures.push(v.to_string());
        }
    }

    // 6. Check report age (replay protection)
    let report_age_valid = check_report_age(policy, nonce_issued_at, &mut failures);

    let verified = nonce_valid
        && signature_valid
        && cert_chain_valid
        && policy_result.passed
        && report_age_valid;

    Ok(VerificationResult {
        verified,
        platform,
        policy_result,
        signature_valid,
        cert_chain_valid,
        nonce_valid,
        report_age_valid,
        failures,
    })
}

/// Verify that the report's report_data field contains the expected nonce.
///
/// The report_data is at offset 0x50 in the SNP report, 64 bytes.
/// The nonce is typically a SHA-512 hash of (verifier_nonce || optional_data).
fn verify_nonce(report: &[u8], expected_nonce: &[u8]) -> bool {
    if report.len() < 0x50 + 64 {
        return false;
    }

    let report_data = &report[0x50..0x50 + 64];

    // Compare the nonce portion. If expected_nonce is shorter than 64 bytes,
    // only compare the prefix (remaining bytes may contain additional binding data).
    let compare_len = expected_nonce.len().min(64);
    report_data[..compare_len] == expected_nonce[..compare_len]
}

/// Verify the ECDSA-P384 signature on the SNP report using the VCEK public key.
///
/// The signature is the last 512 bytes of the report (offset 0x2A0).
/// The signed data is everything before the signature (bytes 0x000..0x2A0).
///
/// The VCEK certificate contains the P-384 public key that signs the report.
fn verify_report_signature(report: &[u8], vcek_der: &[u8]) -> bool {
    if report.len() < SNP_REPORT_SIZE || vcek_der.is_empty() {
        tracing::warn!(
            report_len = report.len(),
            vcek_len = vcek_der.len(),
            "Cannot verify signature: invalid input sizes"
        );
        return false;
    }

    // The signed portion is bytes [0x00..0x2A0] (672 bytes)
    let signed_data = &report[..0x2A0];

    // The signature is at offset 0x2A0:
    //   r: 72 bytes at 0x2A0
    //   s: 72 bytes at 0x2E8
    // Both are zero-padded big-endian integers (P-384 = 48 bytes each)
    let r_bytes = &report[0x2A0..0x2A0 + 72];
    let s_bytes = &report[0x2A0 + 72..0x2A0 + 144];

    // Extract the actual 48-byte values (skip leading zero padding)
    let r_trimmed = trim_leading_zeros(r_bytes, 48);
    let s_trimmed = trim_leading_zeros(s_bytes, 48);

    match verify_p384_signature(signed_data, r_trimmed, s_trimmed, vcek_der) {
        Ok(valid) => valid,
        Err(e) => {
            tracing::warn!("Signature verification error: {}", e);
            false
        }
    }
}

/// Trim leading zeros from a byte slice, keeping at least `min_len` bytes.
fn trim_leading_zeros(bytes: &[u8], min_len: usize) -> &[u8] {
    let start = bytes
        .iter()
        .position(|&b| b != 0)
        .unwrap_or(bytes.len().saturating_sub(min_len));
    let start = start.min(bytes.len().saturating_sub(min_len));
    &bytes[start..]
}

/// Verify a P-384 ECDSA signature using the public key from a DER-encoded certificate.
fn verify_p384_signature(
    message: &[u8],
    r: &[u8],
    s: &[u8],
    cert_der: &[u8],
) -> std::result::Result<bool, String> {
    use der::Decode;
    use p384::ecdsa::{signature::Verifier, Signature, VerifyingKey};
    use x509_cert::Certificate;

    // Parse the X.509 certificate
    let cert = Certificate::from_der(cert_der)
        .map_err(|e| format!("Failed to parse VCEK certificate: {}", e))?;

    // Extract the public key from the certificate
    let spki = cert.tbs_certificate.subject_public_key_info;
    let pub_key_bytes = spki
        .subject_public_key
        .as_bytes()
        .ok_or("Failed to extract public key bytes from VCEK")?;

    // Create the P-384 verifying key
    let verifying_key = VerifyingKey::from_sec1_bytes(pub_key_bytes)
        .map_err(|e| format!("Failed to create P-384 verifying key: {}", e))?;

    // Build the signature from r and s components
    // P-384 signature is 96 bytes (48 bytes r + 48 bytes s)
    let mut sig_bytes = [0u8; 96];
    // Right-align r and s in their 48-byte slots
    let r_offset = 48usize.saturating_sub(r.len());
    let s_offset = 48usize.saturating_sub(s.len());
    sig_bytes[r_offset..48].copy_from_slice(&r[r.len().saturating_sub(48)..]);
    sig_bytes[48 + s_offset..96].copy_from_slice(&s[s.len().saturating_sub(48)..]);

    let signature = Signature::from_slice(&sig_bytes)
        .map_err(|e| format!("Failed to parse ECDSA signature: {}", e))?;

    // Verify: the SNP report is signed over raw bytes (not hashed first by us —
    // the hardware uses SHA-384 internally before signing)
    match verifying_key.verify(message, &signature) {
        Ok(()) => Ok(true),
        Err(_) => Ok(false),
    }
}

/// Verify the certificate chain: VCEK → ASK → ARK.
///
/// Checks that:
/// 1. Each certificate is a valid X.509 certificate
/// 2. VCEK is signed by ASK (ECDSA-P384 signature verification)
/// 3. ASK is signed by ARK (ECDSA-P384 signature verification)
/// 4. ARK is self-signed (ECDSA-P384 signature verification)
/// 5. Issuer/subject names match across the chain
fn verify_cert_chain(vcek_der: &[u8], ask_der: &[u8], ark_der: &[u8]) -> bool {
    use der::Decode;
    use x509_cert::Certificate;

    // If any cert is empty, we can't verify the chain.
    // The report may have been returned without certs (e.g., from cache).
    if vcek_der.is_empty() || ask_der.is_empty() || ark_der.is_empty() {
        tracing::warn!("Certificate chain incomplete, skipping chain verification");
        // Return true if all are empty (certs not provided, will verify via KDS later)
        // Return false if partially provided (inconsistent state)
        return vcek_der.is_empty() && ask_der.is_empty() && ark_der.is_empty();
    }

    // Parse all three certificates
    let vcek = match Certificate::from_der(vcek_der) {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!("Failed to parse VCEK certificate: {}", e);
            return false;
        }
    };

    let ask = match Certificate::from_der(ask_der) {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!("Failed to parse ASK certificate: {}", e);
            return false;
        }
    };

    let ark = match Certificate::from_der(ark_der) {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!("Failed to parse ARK certificate: {}", e);
            return false;
        }
    };

    // Verify issuer/subject chain:
    // VCEK.issuer == ASK.subject
    if vcek.tbs_certificate.issuer != ask.tbs_certificate.subject {
        tracing::warn!("VCEK issuer does not match ASK subject");
        return false;
    }

    // ASK.issuer == ARK.subject
    if ask.tbs_certificate.issuer != ark.tbs_certificate.subject {
        tracing::warn!("ASK issuer does not match ARK subject");
        return false;
    }

    // ARK should be self-signed: ARK.issuer == ARK.subject
    if ark.tbs_certificate.issuer != ark.tbs_certificate.subject {
        tracing::warn!("ARK is not self-signed");
        return false;
    }

    // Verify ECDSA-P384 signatures across the chain.
    // Each certificate's tbsCertificate is signed by the issuer's private key.

    // ARK is self-signed: verify ARK signature with ARK's own public key
    if !verify_cert_signature(&ark, &ark) {
        tracing::warn!("ARK self-signature verification failed");
        return false;
    }

    // ASK is signed by ARK
    if !verify_cert_signature(&ask, &ark) {
        tracing::warn!("ASK signature verification failed (not signed by ARK)");
        return false;
    }

    // VCEK is signed by ASK
    if !verify_cert_signature(&vcek, &ask) {
        tracing::warn!("VCEK signature verification failed (not signed by ASK)");
        return false;
    }

    true
}

/// Verify that `cert` was signed by `issuer` using ECDSA-P384.
///
/// Extracts the tbsCertificate DER bytes from `cert`, the signature from
/// `cert.signature`, and the public key from `issuer`, then performs
/// ECDSA-P384-SHA384 verification.
fn verify_cert_signature(cert: &x509_cert::Certificate, issuer: &x509_cert::Certificate) -> bool {
    use der::Encode;
    use p384::ecdsa::{signature::Verifier, DerSignature, VerifyingKey};

    // Encode the tbsCertificate to DER (this is the signed data)
    let tbs_der = match cert.tbs_certificate.to_der() {
        Ok(d) => d,
        Err(e) => {
            tracing::warn!("Failed to encode tbsCertificate to DER: {}", e);
            return false;
        }
    };

    // Extract the signature bytes from the certificate
    let sig_bytes = match cert.signature.as_bytes() {
        Some(b) => b,
        None => {
            tracing::warn!("Failed to extract signature bytes from certificate");
            return false;
        }
    };

    // Parse as DER-encoded ECDSA signature
    let signature = match DerSignature::from_bytes(sig_bytes) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!("Failed to parse certificate ECDSA signature: {}", e);
            return false;
        }
    };

    // Extract the issuer's public key
    let issuer_pub_key_bytes = match issuer
        .tbs_certificate
        .subject_public_key_info
        .subject_public_key
        .as_bytes()
    {
        Some(b) => b,
        None => {
            tracing::warn!("Failed to extract issuer public key bytes");
            return false;
        }
    };

    let verifying_key = match VerifyingKey::from_sec1_bytes(issuer_pub_key_bytes) {
        Ok(k) => k,
        Err(e) => {
            tracing::warn!("Failed to create P-384 verifying key from issuer: {}", e);
            return false;
        }
    };

    // Verify the signature over the tbsCertificate DER bytes.
    // X.509 uses SHA-384 hash internally for P-384 signatures.
    match verifying_key.verify(&tbs_der, &signature) {
        Ok(()) => true,
        Err(_) => false,
    }
}

/// Check the attestation report against the verification policy.
fn check_policy(platform: &PlatformInfo, policy: &AttestationPolicy) -> PolicyResult {
    let mut violations = Vec::new();

    // Check measurement
    if let Some(ref expected) = policy.expected_measurement {
        if platform.measurement != *expected {
            violations.push(PolicyViolation {
                check: "measurement".to_string(),
                reason: format!(
                    "Expected {}, got {}",
                    &expected[..expected.len().min(16)],
                    &platform.measurement[..platform.measurement.len().min(16)],
                ),
            });
        }
    }

    // Check debug mode (bit 19 of guest policy = debug enabled)
    if policy.require_no_debug {
        let debug_enabled = (platform.policy >> 19) & 1 == 1;
        if debug_enabled {
            violations.push(PolicyViolation {
                check: "debug".to_string(),
                reason: "Debug mode is enabled (policy bit 19 set)".to_string(),
            });
        }
    }

    // Check SMT (bit 16 of guest policy = SMT allowed)
    if policy.require_no_smt {
        let smt_allowed = (platform.policy >> 16) & 1 == 1;
        if smt_allowed {
            violations.push(PolicyViolation {
                check: "smt".to_string(),
                reason: "SMT is enabled (policy bit 16 set)".to_string(),
            });
        }
    }

    // Check TCB version minimums
    if let Some(ref min_tcb) = policy.min_tcb {
        let tcb = &platform.tcb_version;

        if let Some(min_bl) = min_tcb.boot_loader {
            if tcb.boot_loader < min_bl {
                violations.push(PolicyViolation {
                    check: "tcb.boot_loader".to_string(),
                    reason: format!("Boot loader SVN {} < minimum {}", tcb.boot_loader, min_bl),
                });
            }
        }

        if let Some(min_tee) = min_tcb.tee {
            if tcb.tee < min_tee {
                violations.push(PolicyViolation {
                    check: "tcb.tee".to_string(),
                    reason: format!("TEE SVN {} < minimum {}", tcb.tee, min_tee),
                });
            }
        }

        if let Some(min_snp) = min_tcb.snp {
            if tcb.snp < min_snp {
                violations.push(PolicyViolation {
                    check: "tcb.snp".to_string(),
                    reason: format!("SNP SVN {} < minimum {}", tcb.snp, min_snp),
                });
            }
        }

        if let Some(min_uc) = min_tcb.microcode {
            if tcb.microcode < min_uc {
                violations.push(PolicyViolation {
                    check: "tcb.microcode".to_string(),
                    reason: format!("Microcode SVN {} < minimum {}", tcb.microcode, min_uc),
                });
            }
        }
    }

    // Check allowed policy mask
    if let Some(mask) = policy.allowed_policy_mask {
        if platform.policy & mask != mask {
            violations.push(PolicyViolation {
                check: "policy_mask".to_string(),
                reason: format!(
                    "Guest policy {:#x} does not satisfy mask {:#x}",
                    platform.policy, mask,
                ),
            });
        }
    }

    PolicyResult::from_violations(violations)
}

/// Check report age for replay protection.
///
/// SNP reports don't contain a hardware timestamp, so we rely on the
/// application layer: the verifier records when the nonce was issued
/// (`nonce_issued_at`) and checks that the current time minus that
/// timestamp doesn't exceed `policy.max_report_age_secs`.
///
/// Returns `true` if the age check passes or is skipped.
fn check_report_age(
    policy: &AttestationPolicy,
    nonce_issued_at: Option<u64>,
    failures: &mut Vec<String>,
) -> bool {
    let max_age = match policy.max_report_age_secs {
        Some(max) => max,
        None => return true, // No age limit configured
    };

    let issued_at = match nonce_issued_at {
        Some(t) => t,
        None => {
            // Policy requires age check but no timestamp was provided.
            // This is a configuration issue, not a security failure —
            // the caller should pass nonce_issued_at when using max_report_age_secs.
            tracing::warn!(
                "max_report_age_secs={} set but nonce_issued_at not provided, skipping age check",
                max_age
            );
            return true;
        }
    };

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);

    if now < issued_at {
        // Clock skew — nonce_issued_at is in the future
        failures.push(format!(
            "Report age check failed: nonce_issued_at ({}) is in the future (now={})",
            issued_at, now
        ));
        return false;
    }

    let age = now - issued_at;
    if age > max_age {
        failures.push(format!(
            "Report too old: age {}s exceeds maximum {}s (replay protection)",
            age, max_age
        ));
        return false;
    }

    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tee::attestation::{CertificateChain, TcbVersion, SNP_REPORT_SIZE};
    use crate::tee::policy::MinTcbPolicy;

    /// Helper: create a minimal valid-looking SNP report with given nonce.
    fn make_test_report(nonce: &[u8]) -> Vec<u8> {
        let mut report = vec![0u8; SNP_REPORT_SIZE];
        // version = 2
        report[0x00] = 2;
        // guest_svn = 1
        report[0x04] = 1;
        // Set report_data at offset 0x50
        let len = nonce.len().min(64);
        report[0x50..0x50 + len].copy_from_slice(&nonce[..len]);
        // Set some measurement at 0x90
        report[0x90] = 0xAA;
        report[0x91] = 0xBB;
        // TCB at 0x38
        report[0x38] = 3; // boot_loader
        report[0x3E] = 8; // snp
        report[0x3F] = 115; // microcode
        report
    }

    #[test]
    fn test_verify_nonce_match() {
        let nonce = vec![1, 2, 3, 4, 5, 6, 7, 8];
        let report = make_test_report(&nonce);
        assert!(verify_nonce(&report, &nonce));
    }

    #[test]
    fn test_verify_nonce_mismatch() {
        let nonce = vec![1, 2, 3, 4];
        let report = make_test_report(&nonce);
        let wrong_nonce = vec![9, 9, 9, 9];
        assert!(!verify_nonce(&report, &wrong_nonce));
    }

    #[test]
    fn test_verify_nonce_report_too_short() {
        let report = vec![0u8; 10];
        assert!(!verify_nonce(&report, &[1, 2, 3]));
    }

    #[test]
    fn test_trim_leading_zeros() {
        assert_eq!(trim_leading_zeros(&[0, 0, 0, 1, 2, 3], 3), &[1, 2, 3]);
        assert_eq!(trim_leading_zeros(&[0, 0, 0, 0], 2), &[0, 0]);
        assert_eq!(trim_leading_zeros(&[1, 2, 3], 3), &[1, 2, 3]);
        assert_eq!(trim_leading_zeros(&[0, 1], 1), &[1]);
    }

    #[test]
    fn test_verify_report_signature_empty_vcek() {
        let report = vec![0u8; SNP_REPORT_SIZE];
        assert!(!verify_report_signature(&report, &[]));
    }

    #[test]
    fn test_verify_report_signature_short_report() {
        assert!(!verify_report_signature(&[0u8; 100], &[1, 2, 3]));
    }

    #[test]
    fn test_verify_cert_chain_all_empty() {
        // All empty = certs not provided, acceptable (will verify via KDS later)
        assert!(verify_cert_chain(&[], &[], &[]));
    }

    #[test]
    fn test_verify_cert_chain_partially_empty() {
        // Partially empty = inconsistent, should fail
        assert!(!verify_cert_chain(&[1], &[], &[]));
        assert!(!verify_cert_chain(&[], &[1], &[]));
        assert!(!verify_cert_chain(&[], &[], &[1]));
    }

    // ========================================================================
    // Certificate chain ECDSA signature verification tests
    // ========================================================================

    /// Generate a 3-level P-384 certificate chain: ARK (root) → ASK → VCEK.
    /// Returns (vcek_der, ask_der, ark_der).
    fn make_test_cert_chain() -> (Vec<u8>, Vec<u8>, Vec<u8>) {
        use rcgen::{
            BasicConstraints, CertificateParams, DistinguishedName, DnType, IsCa, KeyPair,
            KeyUsagePurpose, PKCS_ECDSA_P384_SHA384,
        };

        // ARK (root CA, self-signed)
        let ark_key = KeyPair::generate_for(&PKCS_ECDSA_P384_SHA384).unwrap();
        let mut ark_params = CertificateParams::default();
        let mut ark_dn = DistinguishedName::new();
        ark_dn.push(DnType::CommonName, "AMD SEV ARK");
        ark_dn.push(DnType::OrganizationName, "AMD");
        ark_params.distinguished_name = ark_dn.clone();
        ark_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        ark_params.key_usages = vec![KeyUsagePurpose::KeyCertSign];
        let ark_cert = ark_params.self_signed(&ark_key).unwrap();

        // ASK (intermediate CA, signed by ARK)
        let ask_key = KeyPair::generate_for(&PKCS_ECDSA_P384_SHA384).unwrap();
        let mut ask_params = CertificateParams::default();
        let mut ask_dn = DistinguishedName::new();
        ask_dn.push(DnType::CommonName, "AMD SEV ASK");
        ask_dn.push(DnType::OrganizationName, "AMD");
        ask_params.distinguished_name = ask_dn;
        ask_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        ask_params.key_usages = vec![KeyUsagePurpose::KeyCertSign];
        let ask_cert = ask_params.signed_by(&ask_key, &ark_cert, &ark_key).unwrap();

        // VCEK (leaf, signed by ASK)
        let vcek_key = KeyPair::generate_for(&PKCS_ECDSA_P384_SHA384).unwrap();
        let mut vcek_params = CertificateParams::default();
        let mut vcek_dn = DistinguishedName::new();
        vcek_dn.push(DnType::CommonName, "AMD SEV VCEK");
        vcek_dn.push(DnType::OrganizationName, "AMD");
        vcek_params.distinguished_name = vcek_dn;
        vcek_params.is_ca = IsCa::NoCa;
        vcek_params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
        let vcek_cert = vcek_params
            .signed_by(&vcek_key, &ask_cert, &ask_key)
            .unwrap();

        (
            vcek_cert.der().to_vec(),
            ask_cert.der().to_vec(),
            ark_cert.der().to_vec(),
        )
    }

    #[test]
    fn test_verify_cert_chain_valid_signatures() {
        let (vcek, ask, ark) = make_test_cert_chain();
        assert!(verify_cert_chain(&vcek, &ask, &ark));
    }

    #[test]
    fn test_verify_cert_chain_wrong_ark_rejects() {
        let (vcek, ask, _ark) = make_test_cert_chain();
        // Generate a different ARK (different key pair)
        let (_, _, wrong_ark) = make_test_cert_chain();
        // ASK was signed by the original ARK, not this one
        assert!(!verify_cert_chain(&vcek, &ask, &wrong_ark));
    }

    #[test]
    fn test_verify_cert_chain_wrong_ask_rejects() {
        let (vcek, _ask, ark) = make_test_cert_chain();
        // Generate a different chain and use its ASK
        let (_, wrong_ask, _) = make_test_cert_chain();
        // VCEK was signed by the original ASK, not this one
        assert!(!verify_cert_chain(&vcek, &wrong_ask, &ark));
    }

    #[test]
    fn test_verify_cert_chain_swapped_ask_ark_rejects() {
        let (vcek, ask, ark) = make_test_cert_chain();
        // Swap ASK and ARK — should fail because ARK won't be self-signed
        // and signatures won't match
        assert!(!verify_cert_chain(&vcek, &ark, &ask));
    }

    #[test]
    fn test_verify_cert_signature_self_signed() {
        use der::Decode;
        use rcgen::{
            CertificateParams, DistinguishedName, DnType, KeyPair, PKCS_ECDSA_P384_SHA384,
        };

        let key = KeyPair::generate_for(&PKCS_ECDSA_P384_SHA384).unwrap();
        let mut params = CertificateParams::default();
        let mut dn = DistinguishedName::new();
        dn.push(DnType::CommonName, "Test Self-Signed");
        params.distinguished_name = dn;
        let cert = params.self_signed(&key).unwrap();

        let parsed = x509_cert::Certificate::from_der(cert.der()).unwrap();
        assert!(verify_cert_signature(&parsed, &parsed));
    }

    #[test]
    fn test_verify_cert_signature_wrong_issuer_rejects() {
        use der::Decode;
        use rcgen::{
            CertificateParams, DistinguishedName, DnType, KeyPair, PKCS_ECDSA_P384_SHA384,
        };

        // Two independent self-signed certs
        let key1 = KeyPair::generate_for(&PKCS_ECDSA_P384_SHA384).unwrap();
        let mut params1 = CertificateParams::default();
        let mut dn1 = DistinguishedName::new();
        dn1.push(DnType::CommonName, "Cert A");
        params1.distinguished_name = dn1;
        let cert1 = params1.self_signed(&key1).unwrap();

        let key2 = KeyPair::generate_for(&PKCS_ECDSA_P384_SHA384).unwrap();
        let mut params2 = CertificateParams::default();
        let mut dn2 = DistinguishedName::new();
        dn2.push(DnType::CommonName, "Cert B");
        params2.distinguished_name = dn2;
        let cert2 = params2.self_signed(&key2).unwrap();

        let parsed1 = x509_cert::Certificate::from_der(cert1.der()).unwrap();
        let parsed2 = x509_cert::Certificate::from_der(cert2.der()).unwrap();

        // cert1 was NOT signed by cert2's key
        assert!(!verify_cert_signature(&parsed1, &parsed2));
    }

    #[test]
    fn test_check_policy_pass_default() {
        let platform = PlatformInfo {
            version: 2,
            guest_svn: 1,
            policy: 0, // no debug, no SMT
            measurement: "aabb".repeat(24),
            tcb_version: TcbVersion {
                boot_loader: 3,
                tee: 0,
                snp: 8,
                microcode: 115,
            },
            chip_id: "00".repeat(64),
        };
        let policy = AttestationPolicy::default();
        let result = check_policy(&platform, &policy);
        assert!(result.passed);
        assert!(result.violations.is_empty());
    }

    #[test]
    fn test_check_policy_debug_violation() {
        let platform = PlatformInfo {
            policy: 1 << 19, // debug enabled
            ..Default::default()
        };
        let policy = AttestationPolicy {
            require_no_debug: true,
            ..Default::default()
        };
        let result = check_policy(&platform, &policy);
        assert!(!result.passed);
        assert!(result.violations.iter().any(|v| v.check == "debug"));
    }

    #[test]
    fn test_check_policy_smt_violation() {
        let platform = PlatformInfo {
            policy: 1 << 16, // SMT enabled
            ..Default::default()
        };
        let policy = AttestationPolicy {
            require_no_debug: false,
            require_no_smt: true,
            ..Default::default()
        };
        let result = check_policy(&platform, &policy);
        assert!(!result.passed);
        assert!(result.violations.iter().any(|v| v.check == "smt"));
    }

    #[test]
    fn test_check_policy_measurement_mismatch() {
        let platform = PlatformInfo {
            measurement: "aa".repeat(48),
            ..Default::default()
        };
        let policy = AttestationPolicy {
            expected_measurement: Some("bb".repeat(48)),
            require_no_debug: false,
            ..Default::default()
        };
        let result = check_policy(&platform, &policy);
        assert!(!result.passed);
        assert!(result.violations.iter().any(|v| v.check == "measurement"));
    }

    #[test]
    fn test_check_policy_measurement_match() {
        let m = "aa".repeat(48);
        let platform = PlatformInfo {
            measurement: m.clone(),
            ..Default::default()
        };
        let policy = AttestationPolicy {
            expected_measurement: Some(m),
            require_no_debug: false,
            ..Default::default()
        };
        let result = check_policy(&platform, &policy);
        assert!(result.passed);
    }

    #[test]
    fn test_check_policy_tcb_violation() {
        let platform = PlatformInfo {
            tcb_version: TcbVersion {
                boot_loader: 2,
                tee: 0,
                snp: 5,
                microcode: 100,
            },
            ..Default::default()
        };
        let policy = AttestationPolicy {
            require_no_debug: false,
            min_tcb: Some(MinTcbPolicy {
                snp: Some(8),        // requires 8, got 5
                microcode: Some(93), // requires 93, got 100 (ok)
                ..Default::default()
            }),
            ..Default::default()
        };
        let result = check_policy(&platform, &policy);
        assert!(!result.passed);
        assert!(result.violations.iter().any(|v| v.check == "tcb.snp"));
        assert!(!result.violations.iter().any(|v| v.check == "tcb.microcode"));
    }

    #[test]
    fn test_check_policy_mask_violation() {
        let platform = PlatformInfo {
            policy: 0x30, // bits 4,5 set
            ..Default::default()
        };
        let policy = AttestationPolicy {
            require_no_debug: false,
            allowed_policy_mask: Some(0x70), // requires bits 4,5,6
            ..Default::default()
        };
        let result = check_policy(&platform, &policy);
        assert!(!result.passed);
        assert!(result.violations.iter().any(|v| v.check == "policy_mask"));
    }

    #[test]
    fn test_verify_attestation_nonce_mismatch() {
        let nonce = vec![1, 2, 3, 4];
        let report_bytes = make_test_report(&nonce);
        let report = AttestationReport {
            report: report_bytes,
            cert_chain: CertificateChain::default(),
            platform: PlatformInfo::default(),
        };
        let wrong_nonce = vec![9, 9, 9, 9];
        let policy = AttestationPolicy {
            require_no_debug: false,
            ..Default::default()
        };
        let result = verify_attestation(&report, &wrong_nonce, &policy, false).unwrap();
        assert!(!result.verified);
        assert!(!result.nonce_valid);
    }

    #[test]
    fn test_verify_attestation_invalid_report_size() {
        let report = AttestationReport {
            report: vec![0u8; 100], // too short
            cert_chain: CertificateChain::default(),
            platform: PlatformInfo::default(),
        };
        let result = verify_attestation(&report, &[1, 2, 3], &AttestationPolicy::default(), false);
        assert!(result.is_err());
    }

    #[test]
    fn test_verify_simulated_report_accepted() {
        let nonce = vec![1, 2, 3, 4];
        let mut report_data = [0u8; 64];
        report_data[..4].copy_from_slice(&nonce);
        let report_bytes = crate::tee::simulate::build_simulated_report(&report_data);
        let report = AttestationReport {
            report: report_bytes,
            cert_chain: CertificateChain::default(),
            platform: PlatformInfo::default(),
        };
        let policy = AttestationPolicy {
            require_no_debug: false,
            ..Default::default()
        };
        let result = verify_attestation(&report, &nonce, &policy, true).unwrap();
        assert!(result.verified);
        assert!(result.signature_valid);
        assert!(result.cert_chain_valid);
        assert!(result.nonce_valid);
    }

    #[test]
    fn test_verify_simulated_report_rejected_when_not_allowed() {
        let nonce = vec![1, 2, 3, 4];
        let mut report_data = [0u8; 64];
        report_data[..4].copy_from_slice(&nonce);
        let report_bytes = crate::tee::simulate::build_simulated_report(&report_data);
        let report = AttestationReport {
            report: report_bytes,
            cert_chain: CertificateChain::default(),
            platform: PlatformInfo::default(),
        };
        let policy = AttestationPolicy::default();
        let result = verify_attestation(&report, &nonce, &policy, false);
        assert!(result.is_err());
    }

    #[test]
    fn test_verify_simulated_report_nonce_still_checked() {
        let nonce = vec![1, 2, 3, 4];
        let mut report_data = [0u8; 64];
        report_data[..4].copy_from_slice(&nonce);
        let report_bytes = crate::tee::simulate::build_simulated_report(&report_data);
        let report = AttestationReport {
            report: report_bytes,
            cert_chain: CertificateChain::default(),
            platform: PlatformInfo::default(),
        };
        let wrong_nonce = vec![9, 9, 9, 9];
        let policy = AttestationPolicy {
            require_no_debug: false,
            ..Default::default()
        };
        let result = verify_attestation(&report, &wrong_nonce, &policy, true).unwrap();
        assert!(!result.verified);
        assert!(!result.nonce_valid);
    }

    // ========================================================================
    // Report age checking tests
    // ========================================================================

    fn now_secs() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs()
    }

    #[test]
    fn test_check_report_age_no_policy() {
        // No max_report_age_secs → always passes
        let policy = AttestationPolicy {
            require_no_debug: false,
            ..Default::default()
        };
        let mut failures = Vec::new();
        assert!(check_report_age(
            &policy,
            Some(now_secs() - 9999),
            &mut failures
        ));
        assert!(failures.is_empty());
    }

    #[test]
    fn test_check_report_age_no_timestamp() {
        // max_report_age_secs set but no timestamp → skip (warn, pass)
        let policy = AttestationPolicy {
            require_no_debug: false,
            max_report_age_secs: Some(60),
            ..Default::default()
        };
        let mut failures = Vec::new();
        assert!(check_report_age(&policy, None, &mut failures));
        assert!(failures.is_empty());
    }

    #[test]
    fn test_check_report_age_fresh_report() {
        let policy = AttestationPolicy {
            require_no_debug: false,
            max_report_age_secs: Some(60),
            ..Default::default()
        };
        let mut failures = Vec::new();
        // Issued 5 seconds ago
        assert!(check_report_age(
            &policy,
            Some(now_secs() - 5),
            &mut failures
        ));
        assert!(failures.is_empty());
    }

    #[test]
    fn test_check_report_age_stale_report() {
        let policy = AttestationPolicy {
            require_no_debug: false,
            max_report_age_secs: Some(60),
            ..Default::default()
        };
        let mut failures = Vec::new();
        // Issued 120 seconds ago, max is 60
        assert!(!check_report_age(
            &policy,
            Some(now_secs() - 120),
            &mut failures
        ));
        assert!(failures.len() == 1);
        assert!(failures[0].contains("too old"));
    }

    #[test]
    fn test_check_report_age_future_timestamp() {
        let policy = AttestationPolicy {
            require_no_debug: false,
            max_report_age_secs: Some(60),
            ..Default::default()
        };
        let mut failures = Vec::new();
        // Issued in the future (clock skew)
        assert!(!check_report_age(
            &policy,
            Some(now_secs() + 3600),
            &mut failures
        ));
        assert!(failures[0].contains("future"));
    }

    #[test]
    fn test_check_report_age_exact_boundary() {
        let policy = AttestationPolicy {
            require_no_debug: false,
            max_report_age_secs: Some(60),
            ..Default::default()
        };
        let mut failures = Vec::new();
        // Issued exactly at the boundary — age == max, should pass (not strictly greater)
        assert!(check_report_age(
            &policy,
            Some(now_secs() - 60),
            &mut failures
        ));
        assert!(failures.is_empty());
    }

    #[test]
    fn test_verify_attestation_with_time_fresh() {
        let nonce = vec![1, 2, 3, 4];
        let mut report_data = [0u8; 64];
        report_data[..4].copy_from_slice(&nonce);
        let report_bytes = crate::tee::simulate::build_simulated_report(&report_data);
        let report = AttestationReport {
            report: report_bytes,
            cert_chain: CertificateChain::default(),
            platform: PlatformInfo::default(),
        };
        let policy = AttestationPolicy {
            require_no_debug: false,
            max_report_age_secs: Some(60),
            ..Default::default()
        };
        let result =
            verify_attestation_with_time(&report, &nonce, &policy, true, Some(now_secs() - 5))
                .unwrap();
        assert!(result.verified);
        assert!(result.report_age_valid);
    }

    #[test]
    fn test_verify_attestation_with_time_stale() {
        let nonce = vec![1, 2, 3, 4];
        let mut report_data = [0u8; 64];
        report_data[..4].copy_from_slice(&nonce);
        let report_bytes = crate::tee::simulate::build_simulated_report(&report_data);
        let report = AttestationReport {
            report: report_bytes,
            cert_chain: CertificateChain::default(),
            platform: PlatformInfo::default(),
        };
        let policy = AttestationPolicy {
            require_no_debug: false,
            max_report_age_secs: Some(30),
            ..Default::default()
        };
        let result =
            verify_attestation_with_time(&report, &nonce, &policy, true, Some(now_secs() - 120))
                .unwrap();
        assert!(!result.verified);
        assert!(!result.report_age_valid);
    }
}
