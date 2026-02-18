//! Attestation, secret injection, and seal/unseal clients over RA-TLS.

use std::path::{Path, PathBuf};

use a3s_box_core::error::{BoxError, Result};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;

use crate::tee::attestation::{AttestationReport, AttestationRequest};

/// Client for requesting attestation reports from the guest VM.
///
/// Sends HTTP POST /attest requests over the Unix socket to the guest agent,
/// which calls the SNP_GET_REPORT ioctl and returns the hardware-signed report.
#[derive(Debug)]
pub struct AttestationClient {
    socket_path: PathBuf,
}

impl AttestationClient {
    /// Connect to the guest agent for attestation requests.
    pub async fn connect(socket_path: &Path) -> Result<Self> {
        let _stream = UnixStream::connect(socket_path).await.map_err(|e| {
            BoxError::AttestationError(format!(
                "Failed to connect to agent at {}: {}",
                socket_path.display(),
                e,
            ))
        })?;

        Ok(Self {
            socket_path: socket_path.to_path_buf(),
        })
    }

    /// Get the socket path this client is connected to.
    pub fn socket_path(&self) -> &Path {
        &self.socket_path
    }

    /// Request an attestation report from the guest VM.
    ///
    /// The guest agent receives the request, calls `SNP_GET_REPORT` via
    /// `/dev/sev-guest`, and returns the hardware-signed report with
    /// the certificate chain.
    ///
    /// # Arguments
    /// * `request` - Attestation request containing the verifier's nonce
    ///
    /// # Returns
    /// * `Ok(AttestationReport)` - Hardware-signed report with cert chain
    /// * `Err(...)` - If the guest agent is unreachable or SNP is unavailable
    pub async fn get_report(&self, request: &AttestationRequest) -> Result<AttestationReport> {
        let body = serde_json::to_string(request).map_err(|e| {
            BoxError::AttestationError(format!("Failed to serialize attestation request: {}", e))
        })?;

        let http_request = format!(
            "POST /attest HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body,
        );

        let mut stream = UnixStream::connect(&self.socket_path).await.map_err(|e| {
            BoxError::AttestationError(format!(
                "Attestation connection failed to {}: {}",
                self.socket_path.display(),
                e,
            ))
        })?;

        stream
            .write_all(http_request.as_bytes())
            .await
            .map_err(|e| {
                BoxError::AttestationError(format!("Attestation request write failed: {}", e))
            })?;

        // Read full response (report + certs can be several KB)
        let mut response = Vec::with_capacity(8192);
        let mut buf = vec![0u8; 8192];
        loop {
            let n = stream.read(&mut buf).await.map_err(|e| {
                BoxError::AttestationError(format!("Attestation response read failed: {}", e))
            })?;
            if n == 0 {
                break;
            }
            response.extend_from_slice(&buf[..n]);
            // Safety limit: 1 MiB (report + full cert chain)
            if response.len() > 1024 * 1024 {
                break;
            }
        }

        let response_str = String::from_utf8_lossy(&response);

        // Find the JSON body after the HTTP headers
        let body_str = response_str
            .find("\r\n\r\n")
            .map(|pos| &response_str[pos + 4..])
            .ok_or_else(|| {
                BoxError::AttestationError(
                    "Malformed attestation response: no HTTP body".to_string(),
                )
            })?;

        // Check for HTTP error status
        if !response_str.starts_with("HTTP/1.1 200") && !response_str.starts_with("HTTP/1.0 200") {
            return Err(BoxError::AttestationError(format!(
                "Attestation request failed: {}",
                body_str.chars().take(200).collect::<String>(),
            )));
        }

        let report: AttestationReport = serde_json::from_str(body_str).map_err(|e| {
            BoxError::AttestationError(format!("Failed to parse attestation response: {}", e))
        })?;

        Ok(report)
    }
}

/// Establish an RA-TLS connection to the guest attestation server.
///
/// Creates a TLS connector with the given attestation policy, connects to the
/// Unix socket, and performs the TLS handshake (which verifies the TEE).
async fn connect_ratls(
    socket_path: &Path,
    policy: crate::tee::AttestationPolicy,
    allow_simulated: bool,
) -> Result<tokio_rustls::client::TlsStream<UnixStream>> {
    let client_config = crate::tee::ratls::create_client_config(policy, allow_simulated)?;
    let connector = tokio_rustls::TlsConnector::from(std::sync::Arc::new(client_config));

    let stream = UnixStream::connect(socket_path).await.map_err(|e| {
        BoxError::AttestationError(format!(
            "Failed to connect to RA-TLS server at {}: {}",
            socket_path.display(),
            e,
        ))
    })?;

    let server_name = rustls::pki_types::ServerName::try_from("localhost")
        .map_err(|e| BoxError::AttestationError(format!("Invalid server name: {}", e)))?;

    connector
        .connect(server_name, stream)
        .await
        .map_err(|e| BoxError::AttestationError(format!("RA-TLS handshake failed: {}", e)))
}

/// Client for verifying TEE attestation via RA-TLS handshake.
///
/// Connects to the guest's RA-TLS attestation server over Unix socket,
/// performs a TLS handshake with a custom certificate verifier that
/// extracts and verifies the SNP report from the server's certificate.
///
/// Attestation verification happens during the TLS handshake — if the
/// handshake succeeds, the TEE is verified.
#[derive(Debug)]
pub struct RaTlsAttestationClient {
    socket_path: PathBuf,
}

impl RaTlsAttestationClient {
    /// Create a new RA-TLS attestation client for the given socket path.
    pub fn new(socket_path: &Path) -> Self {
        Self {
            socket_path: socket_path.to_path_buf(),
        }
    }

    /// Get the socket path.
    pub fn socket_path(&self) -> &Path {
        &self.socket_path
    }

    /// Verify TEE attestation via RA-TLS handshake.
    ///
    /// Connects to the guest attestation server, performs a TLS handshake
    /// with a custom verifier that checks the SNP report embedded in the
    /// server's certificate, and returns the verification result.
    ///
    /// # Arguments
    /// * `policy` - Attestation policy to verify against
    /// * `allow_simulated` - Whether to accept simulated (non-hardware) reports
    pub async fn verify(
        &self,
        policy: crate::tee::AttestationPolicy,
        allow_simulated: bool,
    ) -> Result<crate::tee::VerificationResult> {
        use a3s_box_core::tee::{AttestRequest, AttestRoute};

        let mut tls_stream = connect_ratls(&self.socket_path, policy, allow_simulated).await?;

        // Send a Frame-based status request
        let req = AttestRequest {
            route: AttestRoute::Status,
            payload: serde_json::Value::Null,
        };
        let payload = serde_json::to_vec(&req).map_err(|e| {
            BoxError::AttestationError(format!("Failed to serialize status request: {}", e))
        })?;
        write_tls_frame(&mut tls_stream, 0x01, &payload).await?;

        // Read response frame
        let _response = read_tls_frame(&mut tls_stream).await?;

        // Extract the peer certificate for detailed report info
        let (_, tls_conn) = tls_stream.get_ref();
        let peer_certs = tls_conn.peer_certificates();

        if let Some(certs) = peer_certs {
            if let Some(cert) = certs.first() {
                let report = crate::tee::ratls::extract_report_from_cert(cert.as_ref())?;
                let nonce = if report.report.len() >= 0x90 {
                    &report.report[0x50..0x90]
                } else {
                    &[]
                };
                return crate::tee::verify_attestation(
                    &report,
                    nonce,
                    &crate::tee::AttestationPolicy::default(),
                    allow_simulated,
                );
            }
        }

        // If we got here, TLS handshake succeeded (verifier passed)
        // but we couldn't extract the cert for detailed results
        Ok(crate::tee::VerificationResult {
            verified: true,
            platform: crate::tee::PlatformInfo::default(),
            policy_result: crate::tee::PolicyResult {
                passed: true,
                violations: vec![],
            },
            signature_valid: true,
            cert_chain_valid: true,
            nonce_valid: true,
            report_age_valid: true,
            failures: vec![],
        })
    }
}

/// A secret to inject into the TEE.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SecretEntry {
    /// Secret name (used as filename in /run/secrets/ and env var name).
    pub name: String,
    /// Secret value.
    pub value: String,
    /// Whether to set as environment variable in the guest (default: true).
    #[serde(default = "default_true")]
    pub set_env: bool,
}

fn default_true() -> bool {
    true
}

/// Response from the guest after secret injection.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct SecretInjectionResult {
    /// Number of secrets successfully injected.
    pub injected: usize,
    /// Any non-fatal errors encountered.
    #[serde(default)]
    pub errors: Vec<String>,
}

/// Client for injecting secrets into the TEE via RA-TLS.
///
/// Connects to the guest's RA-TLS attestation server, verifies the TEE
/// during the TLS handshake, then sends secrets over the encrypted channel.
/// The guest stores secrets in `/run/secrets/` (tmpfs) and optionally
/// sets them as environment variables.
#[derive(Debug)]
pub struct SecretInjector {
    socket_path: PathBuf,
}

impl SecretInjector {
    /// Create a new secret injector for the given attestation socket.
    pub fn new(socket_path: &Path) -> Self {
        Self {
            socket_path: socket_path.to_path_buf(),
        }
    }

    /// Inject secrets into the TEE via RA-TLS.
    ///
    /// 1. Connects to the guest attestation server
    /// 2. TLS handshake verifies the TEE (attestation in cert)
    /// 3. Sends secrets over the verified encrypted channel (Frame protocol)
    /// 4. Guest stores secrets in /run/secrets/ and sets env vars
    ///
    /// # Arguments
    /// * `secrets` - List of secrets to inject
    /// * `policy` - Attestation policy for TEE verification
    /// * `allow_simulated` - Whether to accept simulated TEE reports
    pub async fn inject(
        &self,
        secrets: &[SecretEntry],
        policy: crate::tee::AttestationPolicy,
        allow_simulated: bool,
    ) -> Result<SecretInjectionResult> {
        use a3s_box_core::tee::{AttestRequest, AttestRoute};

        if secrets.is_empty() {
            return Ok(SecretInjectionResult {
                injected: 0,
                errors: vec![],
            });
        }

        // Build RA-TLS connection (attestation verified during handshake)
        let mut tls_stream = connect_ratls(&self.socket_path, policy, allow_simulated).await?;

        // Build and send Frame-based secret injection request
        let req = AttestRequest {
            route: AttestRoute::Secrets,
            payload: serde_json::json!({ "secrets": secrets }),
        };
        let payload = serde_json::to_vec(&req).map_err(|e| {
            BoxError::AttestationError(format!("Failed to serialize secrets request: {}", e))
        })?;
        write_tls_frame(&mut tls_stream, 0x01, &payload).await?;

        // Read response frame
        let (frame_type, response_data) = read_tls_frame(&mut tls_stream).await?;

        if frame_type == 0x04 {
            let msg = String::from_utf8_lossy(&response_data);
            return Err(BoxError::AttestationError(format!(
                "Secret injection failed: {}",
                msg,
            )));
        }

        let result: SecretInjectionResult =
            serde_json::from_slice(&response_data).map_err(|e| {
                BoxError::AttestationError(format!("Failed to parse injection response: {}", e))
            })?;

        Ok(result)
    }
}

/// Result of a seal operation from the guest.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct SealResult {
    /// Sealed blob (base64-encoded): nonce || ciphertext || tag.
    pub blob: String,
    /// Policy used for sealing.
    pub policy: String,
    /// Context used for key derivation.
    pub context: String,
}

/// Result of an unseal operation from the guest.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct UnsealResult {
    /// Decrypted data (base64-encoded).
    pub data: String,
}

/// Client for seal/unseal operations in the TEE via RA-TLS.
///
/// Connects to the guest's RA-TLS attestation server, verifies the TEE
/// during the TLS handshake, then sends seal/unseal requests over the
/// encrypted channel. The guest performs the actual crypto using keys
/// derived from its TEE identity (measurement + chip_id).
#[derive(Debug)]
pub struct SealClient {
    socket_path: PathBuf,
}

impl SealClient {
    /// Create a new seal client for the given attestation socket.
    pub fn new(socket_path: &Path) -> Self {
        Self {
            socket_path: socket_path.to_path_buf(),
        }
    }

    /// Seal data inside the TEE via RA-TLS.
    ///
    /// 1. Connects to the guest attestation server
    /// 2. TLS handshake verifies the TEE
    /// 3. Sends plaintext (base64) over the encrypted channel (Frame protocol)
    /// 4. Guest encrypts with AES-256-GCM bound to TEE identity
    ///
    /// # Arguments
    /// * `data` - Raw data to seal
    /// * `context` - Application-specific context for key derivation
    /// * `policy` - Sealing policy name ("MeasurementAndChip", "MeasurementOnly", "ChipOnly")
    /// * `attestation_policy` - Attestation policy for TEE verification
    /// * `allow_simulated` - Whether to accept simulated TEE reports
    pub async fn seal(
        &self,
        data: &[u8],
        context: &str,
        policy: &str,
        attestation_policy: crate::tee::AttestationPolicy,
        allow_simulated: bool,
    ) -> Result<SealResult> {
        use a3s_box_core::tee::{AttestRequest, AttestRoute};
        use base64::Engine;

        let mut tls_stream =
            connect_ratls(&self.socket_path, attestation_policy, allow_simulated).await?;

        let req = AttestRequest {
            route: AttestRoute::Seal,
            payload: serde_json::json!({
                "data": base64::engine::general_purpose::STANDARD.encode(data),
                "context": context,
                "policy": policy,
            }),
        };
        let payload = serde_json::to_vec(&req).map_err(|e| {
            BoxError::AttestationError(format!("Failed to serialize seal request: {}", e))
        })?;
        write_tls_frame(&mut tls_stream, 0x01, &payload).await?;

        let (frame_type, response_data) = read_tls_frame(&mut tls_stream).await?;

        if frame_type == 0x04 {
            let msg = String::from_utf8_lossy(&response_data);
            return Err(BoxError::AttestationError(format!(
                "Seal request failed: {}",
                msg,
            )));
        }

        let result: SealResult = serde_json::from_slice(&response_data).map_err(|e| {
            BoxError::AttestationError(format!("Failed to parse seal response: {}", e))
        })?;

        Ok(result)
    }

    /// Unseal data inside the TEE via RA-TLS.
    ///
    /// 1. Connects to the guest attestation server
    /// 2. TLS handshake verifies the TEE
    /// 3. Sends sealed blob over the encrypted channel (Frame protocol)
    /// 4. Guest decrypts with the TEE-bound key
    ///
    /// # Arguments
    /// * `blob` - Base64-encoded sealed blob
    /// * `context` - Context used during sealing
    /// * `policy` - Sealing policy used during sealing
    /// * `attestation_policy` - Attestation policy for TEE verification
    /// * `allow_simulated` - Whether to accept simulated TEE reports
    pub async fn unseal(
        &self,
        blob: &str,
        context: &str,
        policy: &str,
        attestation_policy: crate::tee::AttestationPolicy,
        allow_simulated: bool,
    ) -> Result<Vec<u8>> {
        use a3s_box_core::tee::{AttestRequest, AttestRoute};
        use base64::Engine;

        let mut tls_stream =
            connect_ratls(&self.socket_path, attestation_policy, allow_simulated).await?;

        let req = AttestRequest {
            route: AttestRoute::Unseal,
            payload: serde_json::json!({
                "blob": blob,
                "context": context,
                "policy": policy,
            }),
        };
        let payload = serde_json::to_vec(&req).map_err(|e| {
            BoxError::AttestationError(format!("Failed to serialize unseal request: {}", e))
        })?;
        write_tls_frame(&mut tls_stream, 0x01, &payload).await?;

        let (frame_type, response_data) = read_tls_frame(&mut tls_stream).await?;

        if frame_type == 0x04 {
            let msg = String::from_utf8_lossy(&response_data);
            return Err(BoxError::AttestationError(format!(
                "Unseal request failed: {}",
                msg,
            )));
        }

        let result: UnsealResult = serde_json::from_slice(&response_data).map_err(|e| {
            BoxError::AttestationError(format!("Failed to parse unseal response: {}", e))
        })?;

        let plaintext = base64::engine::general_purpose::STANDARD
            .decode(&result.data)
            .map_err(|e| {
                BoxError::AttestationError(format!("Failed to decode unsealed data: {}", e))
            })?;

        Ok(plaintext)
    }
}

// ============================================================================
// TLS Frame helpers (used by RA-TLS clients)
// ============================================================================

/// Write a frame over an async TLS stream.
/// Wire format: [type:u8][length:u32 BE][payload]
async fn write_tls_frame<S>(stream: &mut S, frame_type: u8, payload: &[u8]) -> Result<()>
where
    S: tokio::io::AsyncWriteExt + Unpin,
{
    let len = payload.len() as u32;
    let mut header = [0u8; 5];
    header[0] = frame_type;
    header[1..5].copy_from_slice(&len.to_be_bytes());
    stream
        .write_all(&header)
        .await
        .map_err(|e| BoxError::AttestationError(format!("TLS frame header write failed: {}", e)))?;
    if !payload.is_empty() {
        stream.write_all(payload).await.map_err(|e| {
            BoxError::AttestationError(format!("TLS frame payload write failed: {}", e))
        })?;
    }
    Ok(())
}

/// Read a frame from an async TLS stream.
/// Returns (frame_type, payload). Treats unexpected EOF after handshake as empty response.
async fn read_tls_frame<S>(stream: &mut S) -> Result<(u8, Vec<u8>)>
where
    S: tokio::io::AsyncReadExt + Unpin,
{
    let mut header = [0u8; 5];
    match stream.read_exact(&mut header).await {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
            tracing::debug!("RA-TLS peer closed without sending response frame");
            return Ok((0x01, Vec::new()));
        }
        Err(e) => {
            return Err(BoxError::AttestationError(format!(
                "TLS frame header read failed: {}",
                e
            )));
        }
    }
    let frame_type = header[0];
    let len = u32::from_be_bytes([header[1], header[2], header[3], header[4]]) as usize;
    let mut payload = vec![0u8; len];
    if len > 0 {
        stream.read_exact(&mut payload).await.map_err(|e| {
            BoxError::AttestationError(format!("TLS frame payload read failed: {}", e))
        })?;
    }
    Ok((frame_type, payload))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::UnixListener;

    #[tokio::test]
    async fn test_attestation_connect_nonexistent_socket() {
        let result =
            AttestationClient::connect(Path::new("/tmp/nonexistent-a3s-attest-test.sock")).await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(matches!(err, BoxError::AttestationError(_)));
    }

    #[tokio::test]
    async fn test_attestation_connect_and_socket_path() {
        let tmp = tempfile::TempDir::new().unwrap();
        let sock_path = tmp.path().join("attest.sock");
        let _listener = UnixListener::bind(&sock_path).unwrap();

        let client = AttestationClient::connect(&sock_path).await.unwrap();
        assert_eq!(client.socket_path(), sock_path);
    }
}
