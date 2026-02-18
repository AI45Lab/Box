//! Request handlers for RA-TLS attestation server.

use std::io::Write;
use tracing::{debug, info, warn};

use super::frame::{read_frame, send_data_response, send_error_response};

/// Directory where injected secrets are stored (tmpfs, never persisted to disk).
pub(super) const SECRETS_DIR: &str = "/run/secrets";

/// HKDF salt — must match runtime/src/tee/sealed.rs.
pub(super) const HKDF_SALT: &[u8] = b"a3s-sealed-storage-v1";

/// Simulated SNP report version marker (0xA3 = "A3S").
const SIMULATED_REPORT_VERSION: u32 = 0xA3;

// ============================================================================
// TLS connection handler
// ============================================================================

/// Handle a single TLS connection over vsock.
///
/// Performs the TLS handshake (which delivers the RA-TLS certificate),
/// then reads a Frame-based request and routes it:
/// - `status` — Returns TEE status
/// - `secrets` — Receives and stores secrets
/// - `seal` — Seal data bound to TEE identity
/// - `unseal` — Unseal previously sealed data
/// - `process` — Forward to local agent
#[cfg(target_os = "linux")]
pub(super) fn handle_tls_connection(
    fd: std::os::fd::OwnedFd,
    config: std::sync::Arc<rustls::ServerConfig>,
    snp_report: std::sync::Arc<Vec<u8>>,
) -> Result<(), Box<dyn std::error::Error>> {
    use a3s_box_core::tee::{AttestRequest, AttestRoute};
    use std::os::fd::{AsRawFd, FromRawFd};

    let raw_fd = fd.as_raw_fd();
    let tcp_stream = unsafe { std::net::TcpStream::from_raw_fd(raw_fd) };

    let conn = rustls::ServerConnection::new(config)
        .map_err(|e| format!("TLS connection init failed: {}", e))?;

    let mut tls = rustls::StreamOwned::new(conn, tcp_stream);

    // Read a Frame from the TLS stream
    match read_frame(&mut tls) {
        Ok(Some(frame)) => {
            if frame.0 != 0x01 {
                // Not a Data frame — send error
                debug!("RA-TLS received non-data frame type: 0x{:02x}", frame.0);
                send_error_response(&mut tls, "Expected Data frame");
            } else {
                // Parse the JSON request envelope
                match serde_json::from_slice::<AttestRequest>(&frame.1) {
                    Ok(req) => {
                        debug!("RA-TLS request: route={:?}", req.route);
                        match req.route {
                            AttestRoute::Secrets => {
                                handle_secret_injection(&req.payload, &mut tls);
                            }
                            AttestRoute::Seal => {
                                handle_seal_request(&req.payload, &snp_report, &mut tls);
                            }
                            AttestRoute::Unseal => {
                                handle_unseal_request(&req.payload, &snp_report, &mut tls);
                            }
                            AttestRoute::Process => {
                                handle_process_request(&req.payload, &mut tls);
                            }
                            AttestRoute::Status => {
                                send_data_response(&mut tls, b"{\"status\":\"ok\",\"tee\":true}");
                            }
                        }
                    }
                    Err(e) => {
                        debug!("RA-TLS invalid request JSON: {}", e);
                        send_error_response(&mut tls, &format!("Invalid request JSON: {}", e));
                    }
                }
            }
        }
        Ok(None) => {
            debug!("RA-TLS client disconnected after handshake");
        }
        Err(e) => {
            debug!("RA-TLS frame read error: {}", e);
        }
    }

    // Prevent double-close: OwnedFd and TcpStream both own the fd
    std::mem::forget(fd);
    Ok(())
}

// ============================================================================
// Secret injection
// ============================================================================

/// Secret injection request from the host.
#[cfg(target_os = "linux")]
#[derive(serde::Deserialize)]
struct SecretInjectionRequest {
    /// Secrets to inject as key-value pairs.
    secrets: Vec<SecretEntry>,
}

/// A single secret entry.
#[cfg(target_os = "linux")]
#[derive(serde::Deserialize)]
struct SecretEntry {
    /// Secret name (used as filename and env var name).
    name: String,
    /// Secret value.
    value: String,
    /// Whether to set as environment variable (default: true).
    #[serde(default = "default_true")]
    set_env: bool,
}

#[cfg(target_os = "linux")]
fn default_true() -> bool {
    true
}

/// Secret injection response.
#[cfg(target_os = "linux")]
#[derive(serde::Serialize)]
struct SecretInjectionResponse {
    /// Number of secrets injected.
    injected: usize,
    /// Any errors encountered (non-fatal).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    errors: Vec<String>,
}

/// Handle a secrets request: store secrets to /run/secrets/ and set env vars.
#[cfg(target_os = "linux")]
fn handle_secret_injection(payload: &serde_json::Value, tls: &mut impl Write) {
    let req: SecretInjectionRequest = match serde_json::from_value(payload.clone()) {
        Ok(r) => r,
        Err(e) => {
            send_error_response(tls, &format!("Invalid secrets payload: {}", e));
            return;
        }
    };

    let mut injected = 0;
    let mut errors = Vec::new();

    // Ensure secrets directory exists
    if let Err(e) = std::fs::create_dir_all(SECRETS_DIR) {
        send_error_response(tls, &format!("Failed to create secrets dir: {}", e));
        return;
    }

    for entry in &req.secrets {
        // Validate name (alphanumeric, underscore, dash, dot only)
        if !is_valid_secret_name(&entry.name) {
            errors.push(format!("Invalid secret name: {}", entry.name));
            continue;
        }

        // Write to /run/secrets/<name>
        let path = format!("{}/{}", SECRETS_DIR, entry.name);
        match std::fs::write(&path, entry.value.as_bytes()) {
            Ok(()) => {
                // Set restrictive permissions (owner read only)
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o400));
                }

                // Set environment variable if requested
                if entry.set_env {
                    std::env::set_var(&entry.name, &entry.value);
                }

                injected += 1;
                info!("Secret injected: {}", entry.name);
            }
            Err(e) => {
                errors.push(format!("Failed to write {}: {}", entry.name, e));
            }
        }
    }

    let response = SecretInjectionResponse { injected, errors };
    let body = serde_json::to_vec(&response).unwrap_or_else(|_| b"{\"injected\":0}".to_vec());
    send_data_response(tls, &body);
}

/// Validate a secret name: alphanumeric, underscore, dash, dot only.
pub(super) fn is_valid_secret_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 256
        && !name.contains('/')
        && !name.contains('\0')
        && !name.starts_with('.')
        && name
            .chars()
            .all(|c| c.is_alphanumeric() || c == '_' || c == '-' || c == '.')
}

// ============================================================================
// Message processing (POST /process)
// ============================================================================

/// Process request from the host via SafeClaw.
#[cfg(target_os = "linux")]
#[derive(serde::Deserialize)]
struct ProcessRequest {
    /// Session identifier.
    session_id: String,
    /// Message content to process.
    content: String,
    /// Request type: "process_message", "init_session", "terminate_session".
    #[serde(default = "default_request_type")]
    request_type: String,
}

#[cfg(target_os = "linux")]
fn default_request_type() -> String {
    "process_message".to_string()
}

/// Process response returned to the host.
#[cfg(target_os = "linux")]
#[derive(serde::Serialize)]
struct ProcessResponse {
    /// Session identifier.
    session_id: String,
    /// Response content from the TEE-resident agent.
    content: String,
    /// Whether processing succeeded.
    success: bool,
    /// Error message if processing failed.
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

/// Handle a process request: forward message to the local agent for processing.
///
/// The guest agent runs as a separate process inside the TEE. This handler
/// receives messages from the host (via RA-TLS), forwards them to the agent,
/// and returns the agent's response.
#[cfg(target_os = "linux")]
fn handle_process_request(payload: &serde_json::Value, tls: &mut impl Write) {
    let req: ProcessRequest = match serde_json::from_value(payload.clone()) {
        Ok(r) => r,
        Err(e) => {
            send_error_response(tls, &format!("Invalid process payload: {}", e));
            return;
        }
    };

    info!(
        session_id = %req.session_id,
        request_type = %req.request_type,
        content_len = req.content.len(),
        "Processing message in TEE"
    );

    // Forward to the local agent process via localhost HTTP.
    // The agent listens on 127.0.0.1:8080 inside the guest.
    let response = match forward_to_agent(&req) {
        Ok(content) => ProcessResponse {
            session_id: req.session_id,
            content,
            success: true,
            error: None,
        },
        Err(e) => {
            warn!("Agent processing failed: {}", e);
            ProcessResponse {
                session_id: req.session_id,
                content: String::new(),
                success: false,
                error: Some(e),
            }
        }
    };

    let body = serde_json::to_vec(&response)
        .unwrap_or_else(|_| b"{\"success\":false,\"error\":\"serialize\"}".to_vec());
    if response.success {
        send_data_response(tls, &body);
    } else {
        send_error_response(tls, &String::from_utf8_lossy(&body));
    }
}

/// Forward a process request to the local agent via HTTP.
///
/// The agent runs inside the TEE and listens on localhost. This keeps
/// the attestation server (vsock-facing) separate from the agent (internal).
#[cfg(target_os = "linux")]
fn forward_to_agent(req: &ProcessRequest) -> std::result::Result<String, String> {
    use std::io::{Read, Write};
    use std::net::TcpStream;
    use std::time::Duration;

    let agent_addr = "127.0.0.1:8080";

    let mut stream = TcpStream::connect(agent_addr)
        .map_err(|e| format!("Cannot connect to agent at {}: {}", agent_addr, e))?;

    stream
        .set_read_timeout(Some(Duration::from_secs(30)))
        .map_err(|e| format!("Failed to set read timeout: {}", e))?;

    // Build JSON payload for the agent
    let payload = serde_json::json!({
        "session_id": req.session_id,
        "content": req.content,
        "request_type": req.request_type,
    });
    let payload_bytes = serde_json::to_vec(&payload)
        .map_err(|e| format!("Failed to serialize agent request: {}", e))?;

    // Send HTTP POST to agent
    let http_request = format!(
        "POST /process HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        payload_bytes.len()
    );
    stream
        .write_all(http_request.as_bytes())
        .map_err(|e| format!("Failed to write to agent: {}", e))?;
    stream
        .write_all(&payload_bytes)
        .map_err(|e| format!("Failed to write payload to agent: {}", e))?;

    // Read response
    let mut response = Vec::with_capacity(65536);
    stream
        .read_to_end(&mut response)
        .map_err(|e| format!("Failed to read agent response: {}", e))?;

    let response_str = String::from_utf8_lossy(&response);

    // Parse HTTP response body
    let body = response_str
        .find("\r\n\r\n")
        .map(|pos| &response_str[pos + 4..])
        .unwrap_or(&response_str);

    // Extract content from agent response JSON
    let agent_resp: serde_json::Value =
        serde_json::from_str(body).map_err(|e| format!("Invalid agent response JSON: {}", e))?;

    agent_resp
        .get("content")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .ok_or_else(|| "Agent response missing 'content' field".to_string())
}

// ============================================================================
// Sealed storage (guest-side)
// ============================================================================

/// Seal request from the host.
#[cfg(target_os = "linux")]
#[derive(serde::Deserialize)]
struct SealRequest {
    /// Data to seal (base64-encoded).
    data: String,
    /// Application-specific context for key derivation.
    context: String,
    /// Sealing policy: "MeasurementAndChip", "MeasurementOnly", or "ChipOnly".
    #[serde(default = "default_policy")]
    policy: String,
}

#[cfg(target_os = "linux")]
fn default_policy() -> String {
    "MeasurementAndChip".to_string()
}

/// Seal response returned to the host.
#[cfg(target_os = "linux")]
#[derive(serde::Serialize)]
struct SealResponse {
    /// Sealed blob (base64-encoded): nonce || ciphertext || tag.
    blob: String,
    /// Policy used for sealing.
    policy: String,
    /// Context used for key derivation.
    context: String,
}

/// Unseal request from the host.
#[cfg(target_os = "linux")]
#[derive(serde::Deserialize)]
struct UnsealRequest {
    /// Sealed blob (base64-encoded).
    blob: String,
    /// Context used during sealing.
    context: String,
    /// Sealing policy used during sealing.
    #[serde(default = "default_policy")]
    policy: String,
}

/// Unseal response returned to the host.
#[cfg(target_os = "linux")]
#[derive(serde::Serialize)]
struct UnsealResponse {
    /// Decrypted data (base64-encoded).
    data: String,
}

/// Handle a seal request: encrypt data bound to TEE identity.
#[cfg(target_os = "linux")]
fn handle_seal_request(payload: &serde_json::Value, snp_report: &[u8], tls: &mut impl Write) {
    use base64::Engine;
    use ring::aead::{self, Aad, BoundKey, Nonce, NonceSequence, NONCE_LEN};

    let req: SealRequest = match serde_json::from_value(payload.clone()) {
        Ok(r) => r,
        Err(e) => {
            send_error_response(tls, &format!("Invalid seal payload: {}", e));
            return;
        }
    };

    // Decode plaintext from base64
    let plaintext = match base64::engine::general_purpose::STANDARD.decode(&req.data) {
        Ok(d) => d,
        Err(e) => {
            send_error_response(tls, &format!("Invalid base64 data: {}", e));
            return;
        }
    };

    // Derive sealing key
    let key = match derive_guest_sealing_key(snp_report, &req.context, &req.policy) {
        Ok(k) => k,
        Err(e) => {
            send_error_response(tls, &e);
            return;
        }
    };

    // Generate random nonce
    let rng = ring::rand::SystemRandom::new();
    let mut nonce_bytes = [0u8; NONCE_LEN];
    if ring::rand::SecureRandom::fill(&rng, &mut nonce_bytes).is_err() {
        send_error_response(tls, "Failed to generate nonce");
        return;
    }

    // Encrypt with AES-256-GCM
    let mut in_out = plaintext;
    let unbound_key = match aead::UnboundKey::new(&aead::AES_256_GCM, &key) {
        Ok(k) => k,
        Err(_) => {
            send_error_response(tls, "Failed to create encryption key");
            return;
        }
    };

    struct SingleNonce(Option<[u8; 12]>);
    impl NonceSequence for SingleNonce {
        fn advance(&mut self) -> std::result::Result<Nonce, ring::error::Unspecified> {
            self.0
                .take()
                .map(Nonce::assume_unique_for_key)
                .ok_or(ring::error::Unspecified)
        }
    }

    let mut sealing_key = aead::SealingKey::new(unbound_key, SingleNonce(Some(nonce_bytes)));
    if sealing_key
        .seal_in_place_append_tag(Aad::from(req.context.as_bytes()), &mut in_out)
        .is_err()
    {
        send_error_response(tls, "Encryption failed");
        return;
    }

    // Build blob: nonce || ciphertext || tag
    let mut blob = Vec::with_capacity(NONCE_LEN + in_out.len());
    blob.extend_from_slice(&nonce_bytes);
    blob.extend_from_slice(&in_out);

    let response = SealResponse {
        blob: base64::engine::general_purpose::STANDARD.encode(&blob),
        policy: req.policy,
        context: req.context,
    };

    let body =
        serde_json::to_vec(&response).unwrap_or_else(|_| b"{\"error\":\"serialize\"}".to_vec());
    send_data_response(tls, &body);
    info!("Sealed {} bytes of data", blob.len());
}

/// Handle an unseal request: decrypt data using TEE identity.
#[cfg(target_os = "linux")]
fn handle_unseal_request(payload: &serde_json::Value, snp_report: &[u8], tls: &mut impl Write) {
    use base64::Engine;
    use ring::aead::{self, Aad, BoundKey, Nonce, NonceSequence, NONCE_LEN};

    let req: UnsealRequest = match serde_json::from_value(payload.clone()) {
        Ok(r) => r,
        Err(e) => {
            send_error_response(tls, &format!("Invalid unseal payload: {}", e));
            return;
        }
    };

    // Decode sealed blob from base64
    let blob = match base64::engine::general_purpose::STANDARD.decode(&req.blob) {
        Ok(d) => d,
        Err(e) => {
            send_error_response(tls, &format!("Invalid base64 blob: {}", e));
            return;
        }
    };

    if blob.len() < NONCE_LEN + aead::AES_256_GCM.tag_len() {
        send_error_response(tls, "Sealed blob too short");
        return;
    }

    // Derive sealing key
    let key = match derive_guest_sealing_key(snp_report, &req.context, &req.policy) {
        Ok(k) => k,
        Err(e) => {
            send_error_response(tls, &e);
            return;
        }
    };

    // Split nonce and ciphertext
    let nonce_bytes: [u8; NONCE_LEN] = match blob[..NONCE_LEN].try_into() {
        Ok(n) => n,
        Err(_) => {
            send_error_response(tls, "Invalid nonce");
            return;
        }
    };
    let mut in_out = blob[NONCE_LEN..].to_vec();

    // Decrypt with AES-256-GCM
    let unbound_key = match aead::UnboundKey::new(&aead::AES_256_GCM, &key) {
        Ok(k) => k,
        Err(_) => {
            send_error_response(tls, "Failed to create decryption key");
            return;
        }
    };

    struct SingleNonce(Option<[u8; 12]>);
    impl NonceSequence for SingleNonce {
        fn advance(&mut self) -> std::result::Result<Nonce, ring::error::Unspecified> {
            self.0
                .take()
                .map(Nonce::assume_unique_for_key)
                .ok_or(ring::error::Unspecified)
        }
    }

    let mut opening_key = aead::OpeningKey::new(unbound_key, SingleNonce(Some(nonce_bytes)));
    let plaintext = match opening_key.open_in_place(Aad::from(req.context.as_bytes()), &mut in_out)
    {
        Ok(pt) => pt,
        Err(_) => {
            send_error_response(
                tls,
                "Unseal failed: TEE identity mismatch or data corrupted",
            );
            return;
        }
    };

    let response = UnsealResponse {
        data: base64::engine::general_purpose::STANDARD.encode(plaintext),
    };

    let body =
        serde_json::to_vec(&response).unwrap_or_else(|_| b"{\"error\":\"serialize\"}".to_vec());
    send_data_response(tls, &body);
    info!("Unsealed data successfully");
}

/// Derive a 256-bit sealing key from the SNP report using HKDF-SHA256.
///
/// Algorithm matches `runtime/src/tee/sealed.rs::derive_sealing_key`.
pub(super) fn derive_guest_sealing_key(
    report: &[u8],
    context: &str,
    policy: &str,
) -> std::result::Result<[u8; 32], String> {
    use ring::hkdf;

    if report.len() < 0x1E0 {
        return Err("Report too short to extract sealing identity".to_string());
    }

    let measurement = &report[0x90..0xC0]; // 48 bytes
    let chip_id = &report[0x1A0..0x1E0]; // 64 bytes

    let ikm = match policy {
        "MeasurementAndChip" => {
            let mut v = Vec::with_capacity(112);
            v.extend_from_slice(measurement);
            v.extend_from_slice(chip_id);
            v
        }
        "MeasurementOnly" => measurement.to_vec(),
        "ChipOnly" => chip_id.to_vec(),
        _ => {
            let mut v = Vec::with_capacity(112);
            v.extend_from_slice(measurement);
            v.extend_from_slice(chip_id);
            v
        }
    };

    struct HkdfLen(usize);
    impl hkdf::KeyType for HkdfLen {
        fn len(&self) -> usize {
            self.0
        }
    }

    let salt = hkdf::Salt::new(hkdf::HKDF_SHA256, HKDF_SALT);
    let prk = salt.extract(&ikm);
    let info = [context.as_bytes()];
    let okm = prk
        .expand(&info, HkdfLen(32))
        .map_err(|_| "HKDF expand failed".to_string())?;

    let mut key = [0u8; 32];
    okm.fill(&mut key)
        .map_err(|_| "HKDF fill failed".to_string())?;

    Ok(key)
}

// ============================================================================
// Simulation mode
// ============================================================================

/// Check if TEE simulation mode is enabled via environment variable.
pub(super) fn is_simulate_mode() -> bool {
    std::env::var("A3S_TEE_SIMULATE")
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false)
}

/// Generate a simulated 1184-byte SNP report.
pub(super) fn build_simulated_report(report_data: &[u8; super::SNP_USER_DATA_SIZE]) -> Vec<u8> {
    let mut report = vec![0u8; 1184];

    // version at 0x00 — simulated marker
    report[0x00..0x04].copy_from_slice(&SIMULATED_REPORT_VERSION.to_le_bytes());
    // guest_svn at 0x04
    report[0x04..0x08].copy_from_slice(&1u32.to_le_bytes());
    // policy at 0x08
    report[0x08..0x10].copy_from_slice(&0u64.to_le_bytes());
    // current_tcb at 0x38
    report[0x38] = 3; // boot_loader
    report[0x39] = 0; // tee
    report[0x3E] = 8; // snp
    report[0x3F] = 115; // microcode
                        // report_data at 0x50
    report[0x50..0x90].copy_from_slice(report_data);
    // measurement at 0x90 (deterministic fake)
    for i in 0..48 {
        report[0x90 + i] = (i as u8).wrapping_mul(0xA3);
    }
    // chip_id at 0x1A0 (all 0xA3)
    for b in &mut report[0x1A0..0x1E0] {
        *b = 0xA3;
    }
    // signature at 0x2A0 — left as zeros (simulation marker)

    report
}
