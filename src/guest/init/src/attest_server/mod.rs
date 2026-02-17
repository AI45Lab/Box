//! RA-TLS attestation server for AMD SEV-SNP.
//!
//! Listens on vsock port 4091 and serves TLS connections with an
//! RA-TLS certificate that embeds the SNP attestation report.
//! Clients verify the TEE attestation during the TLS handshake
//! by inspecting the custom X.509 extensions in the server certificate.
//!
//! ## Protocol
//!
//! 1. Server generates a P-384 key pair on startup
//! 2. Server obtains an SNP report with SHA-384(public_key) as report_data
//! 3. Server creates a self-signed X.509 cert embedding the report
//! 4. Client connects, TLS handshake delivers the cert
//! 5. Client's custom verifier extracts and verifies the SNP report
//! 6. After handshake, client sends a simple request, server responds with status

use tracing::info;

#[cfg(target_os = "linux")]
mod frame;
#[cfg(target_os = "linux")]
mod handlers;
#[cfg(target_os = "linux")]
mod snp;

#[cfg(test)]
#[cfg(target_os = "linux")]
mod tests;

// Re-export handler functions for internal use
#[cfg(target_os = "linux")]
pub(crate) use handlers::*;

/// Vsock port for the attestation server.
pub const ATTEST_VSOCK_PORT: u32 = a3s_transport::ports::TEE_CHANNEL;

/// Size of the report_data field in the SNP report request.
#[cfg(target_os = "linux")]
pub(super) const SNP_USER_DATA_SIZE: usize = 64;

/// OID for the SNP attestation report extension.
#[cfg(target_os = "linux")]
const OID_SNP_REPORT: &[u64] = &[1, 3, 6, 1, 4, 1, 58270, 1, 1];

/// OID for the certificate chain extension.
#[cfg(target_os = "linux")]
const OID_CERT_CHAIN: &[u64] = &[1, 3, 6, 1, 4, 1, 58270, 1, 2];

// ============================================================================
// Public entry point
// ============================================================================

/// Run the RA-TLS attestation server on vsock port 4091.
///
/// On Linux with SEV-SNP (or simulation mode), generates an RA-TLS
/// certificate and serves TLS connections. Clients verify the TEE
/// attestation during the TLS handshake.
///
/// On non-Linux platforms, this is a no-op (development stub).
pub fn run_attest_server() -> Result<(), Box<dyn std::error::Error>> {
    info!("Starting RA-TLS attestation server on vsock port {}", ATTEST_VSOCK_PORT);

    #[cfg(target_os = "linux")]
    {
        run_ratls_server()?;
    }

    #[cfg(not(target_os = "linux"))]
    {
        info!("RA-TLS attestation server not available on non-Linux platform (development mode)");
    }

    Ok(())
}

// ============================================================================
// RA-TLS server (Linux only)
// ============================================================================

/// Generate an RA-TLS certificate and serve TLS over vsock.
#[cfg(target_os = "linux")]
fn run_ratls_server() -> Result<(), Box<dyn std::error::Error>> {
    use nix::sys::socket::{
        accept, bind, listen, socket, AddressFamily, Backlog, SockFlag, SockType, VsockAddr,
    };
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    use std::sync::Arc;
    use std::time::Duration;
    use tracing::{error, warn};

    // Step 1: Generate key pair and RA-TLS certificate
    let (tls_config, cert_der, snp_report) = generate_ratls_config()?;
    let tls_config = Arc::new(tls_config);
    let snp_report = Arc::new(snp_report);

    info!(
        cert_size = cert_der.len(),
        "RA-TLS certificate generated, starting TLS listener"
    );

    // Step 2: Bind vsock listener
    let sock_fd = socket(
        AddressFamily::Vsock,
        SockType::Stream,
        SockFlag::SOCK_CLOEXEC,
        None,
    )?;

    let addr = VsockAddr::new(libc::VMADDR_CID_ANY, ATTEST_VSOCK_PORT);
    bind(sock_fd.as_raw_fd(), &addr)?;
    listen(&sock_fd, Backlog::new(4)?)?;

    info!("RA-TLS attestation server listening on vsock port {}", ATTEST_VSOCK_PORT);

    // Step 3: Accept loop
    loop {
        match accept(sock_fd.as_raw_fd()) {
            Ok(client_fd) => {
                let client = unsafe { OwnedFd::from_raw_fd(client_fd) };
                let config = Arc::clone(&tls_config);
                let report = Arc::clone(&snp_report);
                if let Err(e) = handlers::handle_tls_connection(client, config, report) {
                    warn!("RA-TLS connection failed: {}", e);
                }
            }
            Err(e) => {
                error!("RA-TLS accept failed: {}", e);
                std::thread::sleep(Duration::from_millis(100));
            }
        }
    }
}

// ============================================================================
// RA-TLS certificate generation
// ============================================================================

/// Generate a rustls ServerConfig with an RA-TLS certificate.
///
/// 1. Generate a P-384 key pair
/// 2. Hash the public key to create report_data
/// 3. Get an SNP report (or simulated) with that report_data
/// 4. Embed the report in a self-signed X.509 certificate
/// 5. Build a rustls ServerConfig
///
/// Returns (ServerConfig, cert_der, report_bytes).
#[cfg(target_os = "linux")]
fn generate_ratls_config() -> Result<(rustls::ServerConfig, Vec<u8>, Vec<u8>), Box<dyn std::error::Error>> {
    use rcgen::{
        CertificateParams, CustomExtension, DistinguishedName, DnType, KeyPair,
        PKCS_ECDSA_P384_SHA384,
    };
    use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
    use sha2::{Digest, Sha256};

    // Generate P-384 key pair
    let key_pair = KeyPair::generate_for(&PKCS_ECDSA_P384_SHA384)
        .map_err(|e| format!("Failed to generate key pair: {}", e))?;

    // Hash public key to create report_data (first 64 bytes of SHA-256)
    let pub_key_der = key_pair.public_key_der();
    let hash = Sha256::digest(&pub_key_der);
    let mut report_data = [0u8; SNP_USER_DATA_SIZE];
    let copy_len = hash.len().min(SNP_USER_DATA_SIZE);
    report_data[..copy_len].copy_from_slice(&hash[..copy_len]);

    // Get attestation report
    let (report_bytes, cert_chain_json) = if handlers::is_simulate_mode() {
        info!("Generating simulated RA-TLS attestation report");
        let report = handlers::build_simulated_report(&report_data);
        let chain_json = b"{}".to_vec();
        (report, chain_json)
    } else {
        info!("Requesting hardware SNP report for RA-TLS certificate");
        let resp = snp::get_snp_report(&report_data)
            .map_err(|e| format!("Failed to get SNP report: {}", e))?;
        let chain_json = serde_json::to_vec(&resp.cert_chain)
            .unwrap_or_else(|_| b"{}".to_vec());
        (resp.report, chain_json)
    };

    // Build X.509 certificate with SNP report extensions
    let mut params = CertificateParams::default();
    let mut dn = DistinguishedName::new();
    dn.push(DnType::CommonName, "A3S Box RA-TLS");
    dn.push(DnType::OrganizationName, "A3S Lab");
    params.distinguished_name = dn;

    // Add SNP report as custom extension
    let snp_report = report_bytes.clone();
    let report_ext = CustomExtension::from_oid_content(OID_SNP_REPORT, report_bytes);
    params.custom_extensions.push(report_ext);

    // Add certificate chain as custom extension
    let chain_ext = CustomExtension::from_oid_content(OID_CERT_CHAIN, cert_chain_json);
    params.custom_extensions.push(chain_ext);

    // Self-sign
    let cert = params.self_signed(&key_pair)
        .map_err(|e| format!("Failed to generate RA-TLS certificate: {}", e))?;

    let cert_der = cert.der().to_vec();
    let key_der = key_pair.serialize_der();

    // Build rustls ServerConfig
    let _ = rustls::crypto::ring::default_provider().install_default();

    let tls_cert = CertificateDer::from(cert_der.clone());
    let tls_key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key_der));

    let config = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(vec![tls_cert], tls_key)
        .map_err(|e| format!("Failed to create TLS config: {}", e))?;

    Ok((config, cert_der, snp_report))
}
