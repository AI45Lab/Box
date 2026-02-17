//! Host-guest communication clients over Unix socket.
//!
//! - `AgentClient`: Health-checking the guest agent (port 4088).
//! - `ExecClient`: Executing commands in the guest (port 4089).
//!
//! Agent-level operations (sessions, generation, skills) are handled
//! by the a3s-code crate, not the Box runtime.

use std::path::{Path, PathBuf};

use a3s_box_core::error::{BoxError, Result};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;

use crate::tee::attestation::{AttestationReport, AttestationRequest};

/// Client for communicating with the guest agent over Unix socket.
///
/// This client only supports connection testing. Agent-level operations
/// (sessions, generation, skills) belong in the a3s-code crate.
/// Health checking is done via `ExecClient::heartbeat()` on the exec server.
pub struct AgentClient {
    socket_path: PathBuf,
}

impl AgentClient {
    /// Connect to the guest agent via Unix socket.
    ///
    /// Verifies the socket is connectable but does not perform a health check.
    pub async fn connect(socket_path: &Path) -> Result<Self> {
        // Verify we can connect to the socket
        let _stream = UnixStream::connect(socket_path).await.map_err(|e| {
            BoxError::Other(format!(
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
}

/// Client for executing commands in the guest over Unix socket.
///
/// Uses the Frame wire protocol: sends a Data frame with JSON ExecRequest,
/// receives a Data frame with JSON ExecOutput.
#[derive(Debug)]
pub struct ExecClient {
    socket_path: PathBuf,
}

impl ExecClient {
    /// Connect to the exec server via Unix socket.
    ///
    /// Verifies the socket is connectable.
    pub async fn connect(socket_path: &Path) -> Result<Self> {
        let _stream = UnixStream::connect(socket_path).await.map_err(|e| {
            BoxError::ExecError(format!(
                "Failed to connect to exec server at {}: {}",
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

    /// Execute a command in the guest.
    ///
    /// Sends a Data frame with JSON ExecRequest, reads a Data frame with JSON ExecOutput.
    pub async fn exec_command(
        &self,
        request: &a3s_box_core::exec::ExecRequest,
    ) -> Result<a3s_box_core::exec::ExecOutput> {
        let payload = serde_json::to_vec(request)
            .map_err(|e| BoxError::ExecError(format!("Failed to serialize exec request: {}", e)))?;

        let mut stream = UnixStream::connect(&self.socket_path).await.map_err(|e| {
            BoxError::ExecError(format!(
                "Exec connection failed to {}: {}",
                self.socket_path.display(),
                e,
            ))
        })?;

        // Send request as Data frame
        let request_frame = a3s_transport::Frame::data(payload);
        let encoded = request_frame.encode().map_err(|e| {
            BoxError::ExecError(format!("Failed to encode exec request frame: {}", e))
        })?;
        stream
            .write_all(&encoded)
            .await
            .map_err(|e| BoxError::ExecError(format!("Exec request write failed: {}", e)))?;

        // Read response frame
        let (r, _w) = tokio::io::split(stream);
        let mut reader = a3s_transport::FrameReader::new(r);
        let frame = reader
            .read_frame()
            .await
            .map_err(|e| BoxError::ExecError(format!("Exec response read failed: {}", e)))?
            .ok_or_else(|| BoxError::ExecError("Exec server closed without response".to_string()))?;

        match frame.frame_type {
            a3s_transport::FrameType::Data => {
                let output: a3s_box_core::exec::ExecOutput =
                    serde_json::from_slice(&frame.payload).map_err(|e| {
                        BoxError::ExecError(format!("Failed to parse exec response: {}", e))
                    })?;
                Ok(output)
            }
            a3s_transport::FrameType::Error => {
                let msg = String::from_utf8_lossy(&frame.payload);
                Err(BoxError::ExecError(format!("Exec server error: {}", msg)))
            }
            _ => Err(BoxError::ExecError(format!(
                "Unexpected frame type: {:?}",
                frame.frame_type
            ))),
        }
    }

    /// Execute a command in streaming mode.
    ///
    /// Sends a Data frame with JSON ExecRequest (streaming=true), then reads
    /// multiple frames: ExecChunk frames for stdout/stderr data, and a final
    /// ExecExit frame with the exit code.
    ///
    /// Returns a `StreamingExec` handle for reading events.
    pub async fn exec_stream(
        &self,
        request: &a3s_box_core::exec::ExecRequest,
    ) -> Result<StreamingExec> {
        let mut req = request.clone();
        req.streaming = true;

        let payload = serde_json::to_vec(&req)
            .map_err(|e| BoxError::ExecError(format!("Failed to serialize exec request: {}", e)))?;

        let stream = UnixStream::connect(&self.socket_path).await.map_err(|e| {
            BoxError::ExecError(format!(
                "Exec connection failed to {}: {}",
                self.socket_path.display(),
                e,
            ))
        })?;

        // Send request as Data frame
        let (r, mut w) = tokio::io::split(stream);
        let request_frame = a3s_transport::Frame::data(payload);
        let encoded = request_frame.encode().map_err(|e| {
            BoxError::ExecError(format!("Failed to encode exec request frame: {}", e))
        })?;
        w.write_all(&encoded)
            .await
            .map_err(|e| BoxError::ExecError(format!("Exec request write failed: {}", e)))?;

        let reader = a3s_transport::FrameReader::new(r);
        let started = std::time::Instant::now();

        Ok(StreamingExec {
            reader,
            started,
            stdout_bytes: 0,
            stderr_bytes: 0,
            done: false,
        })
    }

    /// Transfer a file to/from the guest.
    ///
    /// Sends a Data frame with JSON FileRequest, reads a Data frame with JSON FileResponse.
    pub async fn file_transfer(
        &self,
        request: &a3s_box_core::exec::FileRequest,
    ) -> Result<a3s_box_core::exec::FileResponse> {
        let payload = serde_json::to_vec(request)
            .map_err(|e| BoxError::ExecError(format!("Failed to serialize file request: {}", e)))?;

        let mut stream = UnixStream::connect(&self.socket_path).await.map_err(|e| {
            BoxError::ExecError(format!(
                "Exec connection failed to {}: {}",
                self.socket_path.display(),
                e,
            ))
        })?;

        let request_frame = a3s_transport::Frame::data(payload);
        let encoded = request_frame.encode().map_err(|e| {
            BoxError::ExecError(format!("Failed to encode file request frame: {}", e))
        })?;
        stream
            .write_all(&encoded)
            .await
            .map_err(|e| BoxError::ExecError(format!("File request write failed: {}", e)))?;

        let (r, _w) = tokio::io::split(stream);
        let mut reader = a3s_transport::FrameReader::new(r);
        let frame = reader
            .read_frame()
            .await
            .map_err(|e| BoxError::ExecError(format!("File response read failed: {}", e)))?
            .ok_or_else(|| BoxError::ExecError("Exec server closed without response".to_string()))?;

        match frame.frame_type {
            a3s_transport::FrameType::Data => {
                let response: a3s_box_core::exec::FileResponse =
                    serde_json::from_slice(&frame.payload).map_err(|e| {
                        BoxError::ExecError(format!("Failed to parse file response: {}", e))
                    })?;
                Ok(response)
            }
            a3s_transport::FrameType::Error => {
                let msg = String::from_utf8_lossy(&frame.payload);
                Err(BoxError::ExecError(format!("File transfer error: {}", msg)))
            }
            _ => Err(BoxError::ExecError(format!(
                "Unexpected frame type: {:?}",
                frame.frame_type
            ))),
        }
    }

    /// Send a Heartbeat frame and wait for a Heartbeat response.
    ///
    /// Returns `true` if the exec server responds, `false` otherwise.
    pub async fn heartbeat(&self) -> Result<bool> {
        let mut stream = match UnixStream::connect(&self.socket_path).await {
            Ok(s) => s,
            Err(_) => return Ok(false),
        };

        let frame = a3s_transport::Frame::heartbeat();
        let encoded = match frame.encode() {
            Ok(e) => e,
            Err(_) => return Ok(false),
        };

        if stream.write_all(&encoded).await.is_err() {
            return Ok(false);
        }

        let (r, _w) = tokio::io::split(stream);
        let mut reader = a3s_transport::FrameReader::new(r);
        match reader.read_frame().await {
            Ok(Some(f)) if f.frame_type == a3s_transport::FrameType::Heartbeat => Ok(true),
            _ => Ok(false),
        }
    }
}

/// Handle for reading streaming exec events.
///
/// Reads frames from the exec server: Data frames contain `ExecChunk` (stdout/stderr),
/// Control frames contain `ExecExit` (final exit code).
pub struct StreamingExec {
    reader: a3s_transport::FrameReader<tokio::io::ReadHalf<tokio::net::UnixStream>>,
    started: std::time::Instant,
    stdout_bytes: u64,
    stderr_bytes: u64,
    done: bool,
}

impl StreamingExec {
    /// Read the next event from the stream.
    ///
    /// Returns `None` when the command has exited and all output has been read.
    pub async fn next_event(&mut self) -> Result<Option<a3s_box_core::exec::ExecEvent>> {
        use a3s_box_core::exec::{ExecChunk, ExecEvent, ExecExit};

        if self.done {
            return Ok(None);
        }

        let frame = match self.reader.read_frame().await {
            Ok(Some(f)) => f,
            Ok(None) => {
                self.done = true;
                return Ok(None);
            }
            Err(e) => {
                self.done = true;
                return Err(BoxError::ExecError(format!(
                    "Streaming exec read failed: {}",
                    e
                )));
            }
        };

        match frame.frame_type {
            a3s_transport::FrameType::Data => {
                // Data frame = ExecChunk (stdout/stderr)
                let chunk: ExecChunk = serde_json::from_slice(&frame.payload).map_err(|e| {
                    BoxError::ExecError(format!("Failed to parse exec chunk: {}", e))
                })?;
                match chunk.stream {
                    a3s_box_core::exec::StreamType::Stdout => {
                        self.stdout_bytes += chunk.data.len() as u64;
                    }
                    a3s_box_core::exec::StreamType::Stderr => {
                        self.stderr_bytes += chunk.data.len() as u64;
                    }
                }
                Ok(Some(ExecEvent::Chunk(chunk)))
            }
            a3s_transport::FrameType::Control => {
                // Control frame = ExecExit
                let exit: ExecExit = serde_json::from_slice(&frame.payload).map_err(|e| {
                    BoxError::ExecError(format!("Failed to parse exec exit: {}", e))
                })?;
                self.done = true;
                Ok(Some(ExecEvent::Exit(exit)))
            }
            a3s_transport::FrameType::Error => {
                let msg = String::from_utf8_lossy(&frame.payload);
                self.done = true;
                Err(BoxError::ExecError(format!(
                    "Streaming exec error: {}",
                    msg
                )))
            }
            _ => Err(BoxError::ExecError(format!(
                "Unexpected frame type in stream: {:?}",
                frame.frame_type
            ))),
        }
    }

    /// Collect all remaining output and return the final result with metrics.
    ///
    /// Consumes the stream, buffering all stdout/stderr until the command exits.
    pub async fn collect(mut self) -> Result<(a3s_box_core::exec::ExecOutput, a3s_box_core::exec::ExecMetrics)> {
        use a3s_box_core::exec::{ExecEvent, ExecMetrics, ExecOutput};

        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let mut exit_code = -1;

        while let Some(event) = self.next_event().await? {
            match event {
                ExecEvent::Chunk(chunk) => match chunk.stream {
                    a3s_box_core::exec::StreamType::Stdout => stdout.extend_from_slice(&chunk.data),
                    a3s_box_core::exec::StreamType::Stderr => stderr.extend_from_slice(&chunk.data),
                },
                ExecEvent::Exit(exit) => {
                    exit_code = exit.exit_code;
                }
            }
        }

        let metrics = ExecMetrics {
            duration_ms: self.started.elapsed().as_millis() as u64,
            peak_memory_bytes: None,
            stdout_bytes: self.stdout_bytes,
            stderr_bytes: self.stderr_bytes,
        };

        let output = ExecOutput {
            stdout,
            stderr,
            exit_code,
        };

        Ok((output, metrics))
    }

    /// Whether the stream has finished (command exited or connection closed).
    pub fn is_done(&self) -> bool {
        self.done
    }

    /// Get execution metrics so far.
    pub fn metrics(&self) -> a3s_box_core::exec::ExecMetrics {
        a3s_box_core::exec::ExecMetrics {
            duration_ms: self.started.elapsed().as_millis() as u64,
            peak_memory_bytes: None,
            stdout_bytes: self.stdout_bytes,
            stderr_bytes: self.stderr_bytes,
        }
    }
}

impl std::fmt::Debug for StreamingExec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StreamingExec")
            .field("done", &self.done)
            .field("stdout_bytes", &self.stdout_bytes)
            .field("stderr_bytes", &self.stderr_bytes)
            .finish()
    }
}

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

    connector.connect(server_name, stream).await.map_err(|e| {
        BoxError::AttestationError(format!("RA-TLS handshake failed: {}", e))
    })
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
            policy_result: crate::tee::PolicyResult { passed: true, violations: vec![] },
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
                BoxError::AttestationError(format!(
                    "Failed to parse injection response: {}",
                    e
                ))
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
    stream.write_all(&header).await.map_err(|e| {
        BoxError::AttestationError(format!("TLS frame header write failed: {}", e))
    })?;
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

/// Client for interactive PTY sessions in the guest over Unix socket.
///
/// Connects to the PTY server (vsock port 4090) and provides async
/// frame-based communication for bidirectional terminal I/O.
/// Uses `a3s_transport::FrameReader`/`FrameWriter` for wire I/O.
#[derive(Debug)]
pub struct PtyClient {
    reader: a3s_transport::FrameReader<tokio::io::ReadHalf<tokio::net::UnixStream>>,
    writer: a3s_transport::FrameWriter<tokio::io::WriteHalf<tokio::net::UnixStream>>,
}

impl PtyClient {
    /// Connect to the PTY server via Unix socket.
    pub async fn connect(socket_path: &Path) -> Result<Self> {
        let stream = tokio::net::UnixStream::connect(socket_path)
            .await
            .map_err(|e| {
                BoxError::ExecError(format!(
                    "Failed to connect to PTY server at {}: {}",
                    socket_path.display(),
                    e,
                ))
            })?;

        let (r, w) = tokio::io::split(stream);
        Ok(Self {
            reader: a3s_transport::FrameReader::new(r),
            writer: a3s_transport::FrameWriter::new(w),
        })
    }

    /// Send a PtyRequest to start an interactive session.
    pub async fn send_request(&mut self, req: &a3s_box_core::pty::PtyRequest) -> Result<()> {
        let payload = serde_json::to_vec(req)
            .map_err(|e| BoxError::ExecError(format!("Failed to serialize PtyRequest: {}", e)))?;
        self.write_raw_frame(a3s_box_core::pty::FRAME_PTY_REQUEST, &payload)
            .await
    }

    /// Send terminal data to the guest.
    pub async fn send_data(&mut self, data: &[u8]) -> Result<()> {
        self.write_raw_frame(a3s_box_core::pty::FRAME_PTY_DATA, data)
            .await
    }

    /// Send a terminal resize notification.
    pub async fn send_resize(&mut self, cols: u16, rows: u16) -> Result<()> {
        let resize = a3s_box_core::pty::PtyResize { cols, rows };
        let payload = serde_json::to_vec(&resize)
            .map_err(|e| BoxError::ExecError(format!("Failed to serialize PtyResize: {}", e)))?;
        self.write_raw_frame(a3s_box_core::pty::FRAME_PTY_RESIZE, &payload)
            .await
    }

    /// Read the next frame from the guest.
    ///
    /// Returns `Ok(None)` on EOF (guest disconnected).
    pub async fn read_frame(&mut self) -> Result<Option<(u8, Vec<u8>)>> {
        match self.reader.read_frame().await {
            Ok(Some(frame)) => Ok(Some((frame.frame_type as u8, frame.payload))),
            Ok(None) => Ok(None),
            Err(e) => Err(BoxError::ExecError(format!("PTY frame read failed: {}", e))),
        }
    }

    /// Split the client into read and write halves for concurrent I/O.
    pub fn into_split(
        self,
    ) -> (
        a3s_transport::FrameReader<tokio::io::ReadHalf<tokio::net::UnixStream>>,
        a3s_transport::FrameWriter<tokio::io::WriteHalf<tokio::net::UnixStream>>,
    ) {
        (self.reader, self.writer)
    }

    /// Write a raw PTY frame using the transport writer.
    async fn write_raw_frame(&mut self, frame_type: u8, payload: &[u8]) -> Result<()> {
        // PTY uses custom frame type bytes (0x01-0x05) that map to transport FrameType
        let ft = a3s_transport::FrameType::try_from(frame_type)
            .unwrap_or(a3s_transport::FrameType::Data);
        let frame = a3s_transport::Frame {
            frame_type: ft,
            payload: payload.to_vec(),
        };
        self.writer.write_frame(&frame).await.map_err(|e| {
            BoxError::ExecError(format!("PTY frame write failed: {}", e))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::UnixListener;

    #[tokio::test]
    async fn test_agent_connect_nonexistent_socket() {
        let result = AgentClient::connect(Path::new("/tmp/nonexistent-a3s-test.sock")).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_exec_connect_nonexistent_socket() {
        let result = ExecClient::connect(Path::new("/tmp/nonexistent-a3s-exec-test.sock")).await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(matches!(err, BoxError::ExecError(_)));
    }

    #[tokio::test]
    async fn test_attestation_connect_nonexistent_socket() {
        let result =
            AttestationClient::connect(Path::new("/tmp/nonexistent-a3s-attest-test.sock")).await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(matches!(err, BoxError::AttestationError(_)));
    }

    #[tokio::test]
    async fn test_agent_connect_and_socket_path() {
        let tmp = tempfile::TempDir::new().unwrap();
        let sock_path = tmp.path().join("agent.sock");
        let _listener = UnixListener::bind(&sock_path).unwrap();

        let client = AgentClient::connect(&sock_path).await.unwrap();
        assert_eq!(client.socket_path(), sock_path);
    }

    #[tokio::test]
    async fn test_exec_connect_and_socket_path() {
        let tmp = tempfile::TempDir::new().unwrap();
        let sock_path = tmp.path().join("exec.sock");
        let _listener = UnixListener::bind(&sock_path).unwrap();

        let client = ExecClient::connect(&sock_path).await.unwrap();
        assert_eq!(client.socket_path(), sock_path);
    }

    #[tokio::test]
    async fn test_attestation_connect_and_socket_path() {
        let tmp = tempfile::TempDir::new().unwrap();
        let sock_path = tmp.path().join("attest.sock");
        let _listener = UnixListener::bind(&sock_path).unwrap();

        let client = AttestationClient::connect(&sock_path).await.unwrap();
        assert_eq!(client.socket_path(), sock_path);
    }

    #[tokio::test]
    async fn test_exec_heartbeat_with_echo_server() {
        let tmp = tempfile::TempDir::new().unwrap();
        let sock_path = tmp.path().join("hb_echo.sock");
        let listener = UnixListener::bind(&sock_path).unwrap();

        tokio::spawn(async move {
            // Accept connect verification
            let (stream, _) = listener.accept().await.unwrap();
            drop(stream);
            // Accept heartbeat connection and echo back
            let (mut stream, _) = listener.accept().await.unwrap();
            // Read frame header
            let mut header = [0u8; 5];
            stream.read_exact(&mut header).await.unwrap();
            let len = u32::from_be_bytes([header[1], header[2], header[3], header[4]]) as usize;
            let mut payload = vec![0u8; len];
            if len > 0 {
                stream.read_exact(&mut payload).await.unwrap();
            }
            // Respond with Heartbeat frame
            let response = a3s_transport::Frame::heartbeat();
            let encoded = response.encode().unwrap();
            stream.write_all(&encoded).await.unwrap();
        });

        tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;

        let client = ExecClient::connect(&sock_path).await.unwrap();
        let result = client.heartbeat().await.unwrap();
        assert!(result);
    }

    #[tokio::test]
    async fn test_exec_heartbeat_no_response() {
        let tmp = tempfile::TempDir::new().unwrap();
        let sock_path = tmp.path().join("hb_close.sock");
        let listener = UnixListener::bind(&sock_path).unwrap();

        tokio::spawn(async move {
            // Accept connect verification
            let (stream, _) = listener.accept().await.unwrap();
            drop(stream);
            // Accept heartbeat connection, read request, then close
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut buf = vec![0u8; 1024];
            let _ = stream.read(&mut buf).await;
            drop(stream);
        });

        tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;

        let client = ExecClient::connect(&sock_path).await.unwrap();
        let result = client.heartbeat().await.unwrap();
        assert!(!result);
    }

    #[tokio::test]
    async fn test_exec_heartbeat_nonexistent_socket() {
        // heartbeat() on a non-connectable socket should return false, not error
        let client = ExecClient {
            socket_path: PathBuf::from("/tmp/nonexistent-hb-test.sock"),
        };
        let result = client.heartbeat().await.unwrap();
        assert!(!result);
    }

    #[tokio::test]
    async fn test_pty_client_connect_nonexistent() {
        let result = PtyClient::connect(Path::new("/tmp/nonexistent-pty-test.sock")).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_pty_frame_roundtrip() {
        let tmp = tempfile::TempDir::new().unwrap();
        let sock_path = tmp.path().join("pty.sock");
        let listener = UnixListener::bind(&sock_path).unwrap();

        let sock_path_clone = sock_path.clone();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            // Read a frame: [type:1][len:4][payload]
            let mut header = [0u8; 5];
            stream.read_exact(&mut header).await.unwrap();
            let frame_type = header[0];
            let len = u32::from_be_bytes([header[1], header[2], header[3], header[4]]) as usize;
            let mut payload = vec![0u8; len];
            if len > 0 {
                stream.read_exact(&mut payload).await.unwrap();
            }
            // Echo it back
            stream.write_all(&header).await.unwrap();
            stream.write_all(&payload).await.unwrap();
            (frame_type, payload)
        });

        let mut client = PtyClient::connect(&sock_path_clone).await.unwrap();
        client.send_data(b"hello world").await.unwrap();

        let frame = client.read_frame().await.unwrap().unwrap();
        assert_eq!(frame.0, a3s_box_core::pty::FRAME_PTY_DATA);
        assert_eq!(&frame.1[..], b"hello world");

        let (server_type, server_payload) = server.await.unwrap();
        assert_eq!(server_type, a3s_box_core::pty::FRAME_PTY_DATA);
        assert_eq!(&server_payload[..], b"hello world");
    }

    #[tokio::test]
    async fn test_pty_send_resize() {
        let tmp = tempfile::TempDir::new().unwrap();
        let sock_path = tmp.path().join("pty_resize.sock");
        let listener = UnixListener::bind(&sock_path).unwrap();

        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut header = [0u8; 5];
            stream.read_exact(&mut header).await.unwrap();
            let frame_type = header[0];
            let len = u32::from_be_bytes([header[1], header[2], header[3], header[4]]) as usize;
            let mut payload = vec![0u8; len];
            stream.read_exact(&mut payload).await.unwrap();

            assert_eq!(frame_type, a3s_box_core::pty::FRAME_PTY_RESIZE);
            let resize: a3s_box_core::pty::PtyResize = serde_json::from_slice(&payload).unwrap();
            assert_eq!(resize.cols, 120);
            assert_eq!(resize.rows, 40);
        });

        let mut client = PtyClient::connect(&sock_path).await.unwrap();
        client.send_resize(120, 40).await.unwrap();
    }

    #[tokio::test]
    async fn test_pty_read_frame_eof() {
        let tmp = tempfile::TempDir::new().unwrap();
        let sock_path = tmp.path().join("pty_eof.sock");
        let listener = UnixListener::bind(&sock_path).unwrap();

        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            drop(stream); // Close immediately → EOF
        });

        let mut client = PtyClient::connect(&sock_path).await.unwrap();
        let frame = client.read_frame().await.unwrap();
        assert!(frame.is_none()); // EOF
    }

    #[tokio::test]
    async fn test_exec_client_exec_command() {
        let tmp = tempfile::TempDir::new().unwrap();
        let sock_path = tmp.path().join("exec_cmd.sock");
        let listener = UnixListener::bind(&sock_path).unwrap();

        tokio::spawn(async move {
            // Accept connect verification
            let (stream, _) = listener.accept().await.unwrap();
            drop(stream);
            // Accept exec request — read Frame, respond with Frame
            let (stream, _) = listener.accept().await.unwrap();
            let (r, w) = tokio::io::split(stream);
            let mut reader = a3s_transport::FrameReader::new(r);
            let mut writer = a3s_transport::FrameWriter::new(w);

            // Read request frame
            let _frame = reader.read_frame().await.unwrap().unwrap();

            // Send response as Data frame
            let output = a3s_box_core::exec::ExecOutput {
                stdout: b"hello\n".to_vec(),
                stderr: vec![],
                exit_code: 0,
            };
            let payload = serde_json::to_vec(&output).unwrap();
            writer.write_data(&payload).await.unwrap();
        });

        tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;

        let client = ExecClient::connect(&sock_path).await.unwrap();
        let req = a3s_box_core::exec::ExecRequest {
            cmd: vec!["echo".to_string(), "hello".to_string()],
            env: vec![],
            working_dir: None,
            user: None,
            stdin: None,
            timeout_ns: 0,
            streaming: false,
        };
        let output = client.exec_command(&req).await.unwrap();
        assert_eq!(output.exit_code, 0);
        assert_eq!(&output.stdout[..], b"hello\n");
        assert!(output.stderr.is_empty());
    }

    #[tokio::test]
    async fn test_exec_client_malformed_response() {
        let tmp = tempfile::TempDir::new().unwrap();
        let sock_path = tmp.path().join("exec_bad.sock");
        let listener = UnixListener::bind(&sock_path).unwrap();

        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            drop(stream);
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut buf = vec![0u8; 4096];
            let _ = stream.read(&mut buf).await;
            // Send garbage — not a valid frame
            stream.write_all(b"garbage").await.unwrap();
            drop(stream);
        });

        tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;

        let client = ExecClient::connect(&sock_path).await.unwrap();
        let req = a3s_box_core::exec::ExecRequest {
            cmd: vec!["test".to_string()],
            env: vec![],
            working_dir: None,
            user: None,
            stdin: None,
            timeout_ns: 0,
            streaming: false,
        };
        let result = client.exec_command(&req).await;
        assert!(result.is_err());
    }
}
