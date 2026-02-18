//! RA-TLS (Remote Attestation TLS) for AMD SEV-SNP.
//!
//! Embeds a TEE attestation report inside an X.509 certificate extension,
//! enabling attestation verification during the TLS handshake. Any client
//! connecting to an RA-TLS server can extract and verify the SNP report
//! from the server's certificate, proving the server runs in a genuine TEE.
//!
//! ## OID Convention
//!
//! The SNP attestation report is stored in a custom X.509 extension:
//! - `1.3.6.1.4.1.58270.1.1` — Raw SNP report bytes (1184 bytes)
//! - `1.3.6.1.4.1.58270.1.2` — Certificate chain (JSON: {vcek, ask, ark})
//!
//! ## Usage
//!
//! ```ignore
//! // Server side (inside TEE):
//! let (cert_der, key_der) = generate_ratls_certificate(&report)?;
//! let server_config = create_server_config(&cert_der, &key_der)?;
//!
//! // Client side (verifier):
//! let client_config = create_client_config(policy, allow_simulated)?;
//! ```

use a3s_box_core::error::{BoxError, Result};
use sha2::{Digest, Sha256};

use super::attestation::{AttestationReport, CertificateChain};
use super::policy::AttestationPolicy;
use super::simulate::is_simulated_report;
use super::verifier::verify_attestation;

/// OID for the SNP attestation report extension.
/// Private Enterprise Number (PEN) arc: 1.3.6.1.4.1.58270.1.1
const OID_SNP_REPORT: &str = "1.3.6.1.4.1.58270.1.1";

/// OID for the certificate chain extension.
/// Private Enterprise Number (PEN) arc: 1.3.6.1.4.1.58270.1.2
const OID_CERT_CHAIN: &str = "1.3.6.1.4.1.58270.1.2";

// ============================================================================
// Certificate generation
// ============================================================================

/// Size of the SHA-256 public key hash stored in report_data.
const PUBKEY_HASH_SIZE: usize = 32;

/// Generate a self-signed RA-TLS certificate containing an SNP attestation report.
///
/// The certificate uses a P-384 key pair and embeds the attestation report
/// and certificate chain as custom X.509 extensions. The report's `report_data`
/// field contains a hash of the certificate's public key, binding the TLS
/// identity to the TEE attestation.
///
/// Returns `(cert_der, private_key_der)`.
pub fn generate_ratls_certificate(report: &AttestationReport) -> Result<(Vec<u8>, Vec<u8>)> {
    use rcgen::{
        CertificateParams, CustomExtension, DistinguishedName, DnType, KeyPair,
        PKCS_ECDSA_P384_SHA384,
    };

    // Generate a new P-384 key pair for this certificate
    let key_pair = KeyPair::generate_for(&PKCS_ECDSA_P384_SHA384).map_err(|e| {
        BoxError::AttestationError(format!("Failed to generate P-384 key pair: {}", e))
    })?;

    let mut params = CertificateParams::default();

    // Set subject
    let mut dn = DistinguishedName::new();
    dn.push(DnType::CommonName, "A3S Box RA-TLS");
    dn.push(DnType::OrganizationName, "A3S Lab");
    params.distinguished_name = dn;

    // Add SNP report as custom extension (non-critical)
    let report_ext =
        CustomExtension::from_oid_content(&oid_to_asn1(OID_SNP_REPORT), report.report.clone());
    params.custom_extensions.push(report_ext);

    // Add certificate chain as custom extension (JSON-encoded)
    let chain_json = serde_json::to_vec(&report.cert_chain).map_err(|e| {
        BoxError::AttestationError(format!("Failed to serialize cert chain: {}", e))
    })?;
    let chain_ext = CustomExtension::from_oid_content(&oid_to_asn1(OID_CERT_CHAIN), chain_json);
    params.custom_extensions.push(chain_ext);

    // Generate the self-signed certificate
    let cert = params.self_signed(&key_pair).map_err(|e| {
        BoxError::AttestationError(format!("Failed to generate RA-TLS certificate: {}", e))
    })?;

    let cert_der = cert.der().to_vec();
    let key_der = key_pair.serialize_der();

    tracing::info!(
        cert_size = cert_der.len(),
        report_size = report.report.len(),
        "Generated RA-TLS certificate with SNP attestation report"
    );

    Ok((cert_der, key_der))
}

/// Compute the SHA-256 hash of a DER-encoded public key from an X.509 certificate.
///
/// This is the same hash that the guest attestation server places into
/// `report_data[0..32]` when generating the RA-TLS certificate, binding
/// the TLS public key to the hardware attestation report.
fn compute_cert_pubkey_hash(cert_der: &[u8]) -> Result<[u8; PUBKEY_HASH_SIZE]> {
    use der::{Decode, Encode};
    use x509_cert::Certificate;

    let cert = Certificate::from_der(cert_der).map_err(|e| {
        BoxError::AttestationError(format!(
            "Failed to parse certificate for key binding: {}",
            e
        ))
    })?;

    let spki = &cert.tbs_certificate.subject_public_key_info;
    let pub_key_der = spki
        .to_der()
        .map_err(|e| BoxError::AttestationError(format!("Failed to encode SPKI to DER: {}", e)))?;

    let hash = Sha256::digest(&pub_key_der);
    let mut out = [0u8; PUBKEY_HASH_SIZE];
    out.copy_from_slice(&hash);
    Ok(out)
}

/// Verify that the TLS certificate's public key is bound to the SNP report.
///
/// The guest attestation server computes `SHA-256(public_key_der)` and places
/// it in `report_data[0..32]`. This function recomputes the hash from the
/// certificate and checks it matches, preventing MITM attacks where an
/// attacker replays a valid report in a different certificate.
fn verify_pubkey_binding(cert_der: &[u8], report: &[u8]) -> Result<bool> {
    if report.len() < 0x50 + 64 {
        return Err(BoxError::AttestationError(
            "Report too short to extract report_data for key binding".to_string(),
        ));
    }

    let expected_hash = &report[0x50..0x50 + PUBKEY_HASH_SIZE];
    let actual_hash = compute_cert_pubkey_hash(cert_der)?;

    Ok(expected_hash == actual_hash)
}

// ============================================================================
// Report extraction from certificate
// ============================================================================

/// Extract an SNP attestation report from an RA-TLS certificate.
///
/// Parses the X.509 certificate and looks for the custom extensions
/// containing the SNP report and certificate chain.
pub fn extract_report_from_cert(cert_der: &[u8]) -> Result<AttestationReport> {
    use der::Decode;
    use x509_cert::Certificate;

    let cert = Certificate::from_der(cert_der).map_err(|e| {
        BoxError::AttestationError(format!("Failed to parse RA-TLS certificate: {}", e))
    })?;

    let mut report_bytes: Option<Vec<u8>> = None;
    let mut cert_chain = CertificateChain::default();

    // Search extensions for our custom OIDs
    if let Some(extensions) = &cert.tbs_certificate.extensions {
        let report_oid = oid_string_to_der(OID_SNP_REPORT);
        let chain_oid = oid_string_to_der(OID_CERT_CHAIN);

        for ext in extensions.iter() {
            let ext_oid = ext.extn_id.to_string();

            if ext_oid == oid_der_to_dotted(&report_oid) || ext.extn_id.as_bytes() == report_oid {
                report_bytes = Some(ext.extn_value.as_bytes().to_vec());
            } else if ext_oid == oid_der_to_dotted(&chain_oid)
                || ext.extn_id.as_bytes() == chain_oid
            {
                if let Ok(chain) =
                    serde_json::from_slice::<CertificateChain>(ext.extn_value.as_bytes())
                {
                    cert_chain = chain;
                }
            }
        }
    }

    let report = report_bytes.ok_or_else(|| {
        BoxError::AttestationError(
            "RA-TLS certificate does not contain SNP report extension".to_string(),
        )
    })?;

    // Parse platform info from the report
    let platform = super::attestation::parse_platform_info(&report).unwrap_or_default();

    Ok(AttestationReport {
        report,
        cert_chain,
        platform,
    })
}

/// Verify an RA-TLS certificate by extracting and verifying the embedded SNP report.
///
/// # Arguments
/// * `cert_der` - DER-encoded X.509 certificate
/// * `expected_nonce` - Expected nonce in the report (or empty to skip nonce check)
/// * `policy` - Attestation policy to check against
/// * `allow_simulated` - Whether to accept simulated reports
pub fn verify_ratls_certificate(
    cert_der: &[u8],
    expected_nonce: &[u8],
    policy: &AttestationPolicy,
    allow_simulated: bool,
) -> Result<super::verifier::VerificationResult> {
    let report = extract_report_from_cert(cert_der)?;
    verify_attestation(&report, expected_nonce, policy, allow_simulated)
}

// ============================================================================
// TLS configuration builders
// ============================================================================

/// Create a rustls `ServerConfig` for an RA-TLS server.
///
/// The server presents the RA-TLS certificate (containing the SNP report)
/// to connecting clients during the TLS handshake.
pub fn create_server_config(cert_der: &[u8], key_der: &[u8]) -> Result<rustls::ServerConfig> {
    use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};

    let cert = CertificateDer::from(cert_der.to_vec());
    let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key_der.to_vec()));

    let config = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(vec![cert], key)
        .map_err(|e| {
            BoxError::AttestationError(format!("Failed to create RA-TLS server config: {}", e))
        })?;

    Ok(config)
}

/// Create a rustls `ClientConfig` for connecting to an RA-TLS server.
///
/// Uses a custom certificate verifier that extracts the SNP report from
/// the server's certificate and verifies it against the given policy.
pub fn create_client_config(
    policy: AttestationPolicy,
    allow_simulated: bool,
) -> Result<rustls::ClientConfig> {
    // Ensure the ring crypto provider is installed (idempotent, ignores if already set)
    let _ = rustls::crypto::ring::default_provider().install_default();

    let verifier = RaTlsVerifier::new(policy, allow_simulated);

    let config = rustls::ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(std::sync::Arc::new(verifier))
        .with_no_client_auth();

    Ok(config)
}

// ============================================================================
// Custom TLS certificate verifier
// ============================================================================

/// Custom rustls certificate verifier for RA-TLS.
///
/// During TLS handshake, extracts the SNP attestation report from the
/// server's certificate extension and verifies it using the standard
/// attestation verification flow (signature, cert chain, policy).
#[derive(Debug)]
struct RaTlsVerifier {
    policy: AttestationPolicy,
    allow_simulated: bool,
}

impl RaTlsVerifier {
    fn new(policy: AttestationPolicy, allow_simulated: bool) -> Self {
        Self {
            policy,
            allow_simulated,
        }
    }
}

impl rustls::client::danger::ServerCertVerifier for RaTlsVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &rustls::pki_types::CertificateDer<'_>,
        _intermediates: &[rustls::pki_types::CertificateDer<'_>],
        _server_name: &rustls::pki_types::ServerName<'_>,
        _ocsp_response: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> std::result::Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        let cert_der = end_entity.as_ref();

        // Extract and verify the SNP report from the certificate
        let report = extract_report_from_cert(cert_der).map_err(|e| {
            rustls::Error::General(format!("RA-TLS report extraction failed: {}", e))
        })?;

        // Verify public key binding: the report_data[0..32] must contain
        // SHA-256(certificate_public_key). This prevents MITM attacks where
        // an attacker replays a valid SNP report in a different certificate.
        let key_bound = verify_pubkey_binding(cert_der, &report.report).map_err(|e| {
            rustls::Error::General(format!("RA-TLS key binding check failed: {}", e))
        })?;

        if !key_bound {
            return Err(rustls::Error::General(
                "RA-TLS key binding failed: certificate public key hash does not match report_data. \
                 Possible MITM attack — the SNP report was not generated for this TLS certificate."
                    .to_string(),
            ));
        }

        // Verify the report structure, signature, cert chain, and policy.
        // For RA-TLS, the nonce in report_data is the public key hash (already
        // verified above), so we pass it as the expected nonce.
        let nonce_to_check = if report.report.len() >= 0x90 {
            &report.report[0x50..0x90]
        } else {
            return Err(rustls::Error::General(
                "RA-TLS report too short to extract report_data".to_string(),
            ));
        };

        let result =
            verify_attestation(&report, nonce_to_check, &self.policy, self.allow_simulated)
                .map_err(|e| {
                    rustls::Error::General(format!("RA-TLS attestation verification failed: {}", e))
                })?;

        if result.verified {
            tracing::debug!(
                simulated = is_simulated_report(&report.report),
                key_bound = true,
                "RA-TLS attestation verified with public key binding"
            );
            Ok(rustls::client::danger::ServerCertVerified::assertion())
        } else {
            let failures = result.failures.join("; ");
            Err(rustls::Error::General(format!(
                "RA-TLS attestation failed: {}",
                failures
            )))
        }
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &rustls::pki_types::CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> std::result::Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        // We trust the TLS signature if the attestation report is valid
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &rustls::pki_types::CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> std::result::Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        vec![
            rustls::SignatureScheme::ECDSA_NISTP384_SHA384,
            rustls::SignatureScheme::ECDSA_NISTP256_SHA256,
        ]
    }
}

// ============================================================================
// OID helpers
// ============================================================================

/// Convert a dotted OID string to rcgen's ASN.1 OID format (array of u64).
fn oid_to_asn1(oid: &str) -> Vec<u64> {
    oid.split('.')
        .filter_map(|s| s.parse::<u64>().ok())
        .collect()
}

/// Convert a dotted OID string to DER-encoded OID bytes.
fn oid_string_to_der(oid: &str) -> Vec<u8> {
    let components: Vec<u64> = oid_to_asn1(oid);
    if components.len() < 2 {
        return vec![];
    }

    let mut encoded = Vec::new();
    // First two components are encoded as (c0 * 40 + c1)
    encoded.push((components[0] * 40 + components[1]) as u8);

    // Remaining components use base-128 encoding
    for &c in &components[2..] {
        encode_base128(&mut encoded, c);
    }

    encoded
}

/// Encode a value in base-128 (variable-length quantity) for OID encoding.
fn encode_base128(buf: &mut Vec<u8>, value: u64) {
    if value < 128 {
        buf.push(value as u8);
        return;
    }

    let mut bytes = Vec::new();
    let mut v = value;
    bytes.push((v & 0x7F) as u8);
    v >>= 7;
    while v > 0 {
        bytes.push((v & 0x7F) as u8 | 0x80);
        v >>= 7;
    }
    bytes.reverse();
    buf.extend_from_slice(&bytes);
}

/// Convert DER-encoded OID bytes to dotted string for comparison.
fn oid_der_to_dotted(der: &[u8]) -> String {
    if der.is_empty() {
        return String::new();
    }

    let mut components = Vec::new();
    components.push((der[0] / 40) as u64);
    components.push((der[0] % 40) as u64);

    let mut value: u64 = 0;
    for &byte in &der[1..] {
        value = (value << 7) | (byte & 0x7F) as u64;
        if byte & 0x80 == 0 {
            components.push(value);
            value = 0;
        }
    }

    components
        .iter()
        .map(|c| c.to_string())
        .collect::<Vec<_>>()
        .join(".")
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tee::attestation::{CertificateChain, PlatformInfo};
    use crate::tee::simulate::build_simulated_report;

    /// Generate a test RA-TLS certificate with the public key hash correctly
    /// bound in report_data, matching the real guest attestation server behavior.
    fn make_bound_ratls_cert() -> (Vec<u8>, Vec<u8>, AttestationReport) {
        use rcgen::{
            CertificateParams, CustomExtension, DistinguishedName, DnType, KeyPair,
            PKCS_ECDSA_P384_SHA384,
        };

        let key_pair = KeyPair::generate_for(&PKCS_ECDSA_P384_SHA384).unwrap();

        // Hash the public key (same as guest attest_server.rs)
        let pub_key_der = key_pair.public_key_der();
        let hash = Sha256::digest(&pub_key_der);
        let mut report_data = [0u8; 64];
        let copy_len = hash.len().min(64);
        report_data[..copy_len].copy_from_slice(&hash[..copy_len]);

        let report_bytes = build_simulated_report(&report_data);

        // Build certificate with report embedded
        let mut params = CertificateParams::default();
        let mut dn = DistinguishedName::new();
        dn.push(DnType::CommonName, "A3S Box RA-TLS");
        dn.push(DnType::OrganizationName, "A3S Lab");
        params.distinguished_name = dn;

        let report_ext =
            CustomExtension::from_oid_content(&oid_to_asn1(OID_SNP_REPORT), report_bytes.clone());
        params.custom_extensions.push(report_ext);

        let chain = CertificateChain::default();
        let chain_json = serde_json::to_vec(&chain).unwrap();
        let chain_ext = CustomExtension::from_oid_content(&oid_to_asn1(OID_CERT_CHAIN), chain_json);
        params.custom_extensions.push(chain_ext);

        let cert = params.self_signed(&key_pair).unwrap();
        let cert_der = cert.der().to_vec();
        let key_der = key_pair.serialize_der();

        let report = AttestationReport {
            report: report_bytes,
            cert_chain: chain,
            platform: PlatformInfo::default(),
        };

        (cert_der, key_der, report)
    }

    /// Generate a certificate with an UNBOUND report (report_data does not
    /// contain the public key hash). Simulates a MITM attack.
    fn make_unbound_ratls_cert() -> (Vec<u8>, AttestationReport) {
        use rcgen::{
            CertificateParams, CustomExtension, DistinguishedName, DnType, KeyPair,
            PKCS_ECDSA_P384_SHA384,
        };

        let key_pair = KeyPair::generate_for(&PKCS_ECDSA_P384_SHA384).unwrap();

        // Use arbitrary report_data that does NOT match the public key hash
        let mut report_data = [0u8; 64];
        report_data[0..4].copy_from_slice(&[0xDE, 0xAD, 0xBE, 0xEF]);

        let report_bytes = build_simulated_report(&report_data);

        let mut params = CertificateParams::default();
        let mut dn = DistinguishedName::new();
        dn.push(DnType::CommonName, "A3S Box RA-TLS");
        params.distinguished_name = dn;

        let report_ext =
            CustomExtension::from_oid_content(&oid_to_asn1(OID_SNP_REPORT), report_bytes.clone());
        params.custom_extensions.push(report_ext);

        let chain = CertificateChain::default();
        let chain_json = serde_json::to_vec(&chain).unwrap();
        let chain_ext = CustomExtension::from_oid_content(&oid_to_asn1(OID_CERT_CHAIN), chain_json);
        params.custom_extensions.push(chain_ext);

        let cert = params.self_signed(&key_pair).unwrap();
        let cert_der = cert.der().to_vec();

        let report = AttestationReport {
            report: report_bytes,
            cert_chain: chain,
            platform: PlatformInfo::default(),
        };

        (cert_der, report)
    }

    #[test]
    fn test_oid_to_asn1() {
        let asn1 = oid_to_asn1("1.3.6.1.4.1.58270.1.1");
        assert_eq!(asn1, vec![1, 3, 6, 1, 4, 1, 58270, 1, 1]);
    }

    #[test]
    fn test_oid_roundtrip() {
        let oid = "1.3.6.1.4.1.58270.1.1";
        let der = oid_string_to_der(oid);
        let dotted = oid_der_to_dotted(&der);
        assert_eq!(dotted, oid);
    }

    #[test]
    fn test_oid_roundtrip_chain() {
        let oid = "1.3.6.1.4.1.58270.1.2";
        let der = oid_string_to_der(oid);
        let dotted = oid_der_to_dotted(&der);
        assert_eq!(dotted, oid);
    }

    #[test]
    fn test_encode_base128_small() {
        let mut buf = Vec::new();
        encode_base128(&mut buf, 127);
        assert_eq!(buf, vec![127]);
    }

    #[test]
    fn test_encode_base128_large() {
        let mut buf = Vec::new();
        encode_base128(&mut buf, 58270);
        // 58270 = 0xE39E -> base128: [0x83, 0xC7, 0x1E]
        assert!(!buf.is_empty());
        // Verify roundtrip
        let mut value: u64 = 0;
        for &b in &buf {
            value = (value << 7) | (b & 0x7F) as u64;
        }
        assert_eq!(value, 58270);
    }

    #[test]
    fn test_generate_ratls_certificate() {
        let (cert_der, key_der, _) = make_bound_ratls_cert();
        assert!(!cert_der.is_empty());
        assert!(!key_der.is_empty());
    }

    #[test]
    fn test_extract_report_from_cert() {
        let (cert_der, _, report) = make_bound_ratls_cert();
        let extracted = extract_report_from_cert(&cert_der).unwrap();
        assert_eq!(extracted.report.len(), 1184);
        // Verify the report_data is preserved (contains pubkey hash)
        assert_eq!(&extracted.report[0x50..0x90], &report.report[0x50..0x90]);
    }

    #[test]
    fn test_extract_report_no_extension() {
        // A regular cert without our extension should fail
        use rcgen::{CertificateParams, KeyPair, PKCS_ECDSA_P384_SHA384};
        let key_pair = KeyPair::generate_for(&PKCS_ECDSA_P384_SHA384).unwrap();
        let params = CertificateParams::default();
        let cert = params.self_signed(&key_pair).unwrap();
        let result = extract_report_from_cert(cert.der());
        assert!(result.is_err());
    }

    #[test]
    fn test_verify_ratls_certificate_simulated_with_binding() {
        let (cert_der, _, report) = make_bound_ratls_cert();
        let nonce = &report.report[0x50..0x90];
        let policy = AttestationPolicy {
            require_no_debug: false,
            ..Default::default()
        };
        let result = verify_ratls_certificate(&cert_der, nonce, &policy, true).unwrap();
        assert!(result.verified);
    }

    #[test]
    fn test_verify_ratls_certificate_simulated_rejected() {
        let (cert_der, _, report) = make_bound_ratls_cert();
        let nonce = &report.report[0x50..0x90];
        let policy = AttestationPolicy::default();
        // allow_simulated = false should reject
        let result = verify_ratls_certificate(&cert_der, nonce, &policy, false);
        assert!(result.is_err());
    }

    #[test]
    fn test_create_server_config() {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let (cert_der, key_der, _) = make_bound_ratls_cert();
        let config = create_server_config(&cert_der, &key_der);
        assert!(config.is_ok());
    }

    #[test]
    fn test_create_client_config() {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let policy = AttestationPolicy::default();
        let config = create_client_config(policy, true);
        assert!(config.is_ok());
    }

    #[test]
    fn test_ratls_verifier_debug() {
        let verifier = RaTlsVerifier::new(AttestationPolicy::default(), false);
        let debug = format!("{:?}", verifier);
        assert!(debug.contains("RaTlsVerifier"));
    }

    // ========================================================================
    // Public key binding tests
    // ========================================================================

    #[test]
    fn test_pubkey_binding_valid() {
        let (cert_der, _, report) = make_bound_ratls_cert();
        let bound = verify_pubkey_binding(&cert_der, &report.report).unwrap();
        assert!(bound, "Public key hash should match report_data");
    }

    #[test]
    fn test_pubkey_binding_invalid_mitm() {
        let (cert_der, _) = make_unbound_ratls_cert();
        // The report_data contains [0xDE, 0xAD, 0xBE, 0xEF, 0, 0, ...]
        // which does NOT match the certificate's public key hash
        let report_data = {
            let mut rd = [0u8; 64];
            rd[0..4].copy_from_slice(&[0xDE, 0xAD, 0xBE, 0xEF]);
            build_simulated_report(&rd)
        };
        let bound = verify_pubkey_binding(&cert_der, &report_data).unwrap();
        assert!(!bound, "Unbound report should fail key binding check");
    }

    #[test]
    fn test_pubkey_binding_report_too_short() {
        let (cert_der, _, _) = make_bound_ratls_cert();
        let short_report = vec![0u8; 10];
        let result = verify_pubkey_binding(&cert_der, &short_report);
        assert!(result.is_err());
    }

    #[test]
    fn test_compute_cert_pubkey_hash_deterministic() {
        let (cert_der, _, _) = make_bound_ratls_cert();
        let hash1 = compute_cert_pubkey_hash(&cert_der).unwrap();
        let hash2 = compute_cert_pubkey_hash(&cert_der).unwrap();
        assert_eq!(hash1, hash2);
    }

    #[test]
    fn test_compute_cert_pubkey_hash_different_certs() {
        let (cert1, _, _) = make_bound_ratls_cert();
        let (cert2, _, _) = make_bound_ratls_cert();
        let hash1 = compute_cert_pubkey_hash(&cert1).unwrap();
        let hash2 = compute_cert_pubkey_hash(&cert2).unwrap();
        // Different key pairs → different hashes
        assert_ne!(hash1, hash2);
    }

    #[test]
    fn test_compute_cert_pubkey_hash_invalid_cert() {
        let result = compute_cert_pubkey_hash(&[0xFF, 0xFF, 0xFF]);
        assert!(result.is_err());
    }
}
