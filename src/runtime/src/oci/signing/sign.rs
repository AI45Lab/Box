//! Image signing (cosign-compatible): create and push a signature artifact.

use a3s_box_core::error::{BoxError, Result};
use oci_distribution::secrets::RegistryAuth;
use oci_distribution::{Client, Reference};

use super::crypto::{base64_decode, base64_encode};
use super::{
    cosign_signature_tag, CosignCritical, CosignIdentity, CosignImage, CosignPayload,
    CosignSignatureEnvelope,
};

/// Result of a successful image signing operation.
#[derive(Debug, Clone)]
pub struct SignResult {
    /// The signature tag pushed to the registry (e.g., "sha256-abc123.sig").
    pub signature_tag: String,
}

/// Sign an image after push using a PEM-encoded ECDSA P-256 private key.
///
/// Creates a cosign-compatible signature artifact and pushes it to the registry
/// as a separate image with the `.sig` tag convention.
///
/// # Arguments
/// * `private_key_path` - Path to PEM-encoded ECDSA P-256 private key
/// * `registry` - Registry hostname (e.g., "ghcr.io")
/// * `repository` - Repository path (e.g., "myorg/myimage")
/// * `manifest_digest` - Digest of the pushed manifest (e.g., "sha256:abc123...")
/// * `docker_reference` - Full image reference (e.g., "ghcr.io/myorg/myimage:latest")
pub async fn sign_image(
    private_key_path: &str,
    registry: &str,
    repository: &str,
    manifest_digest: &str,
    docker_reference: &str,
) -> Result<SignResult> {
    use p256::ecdsa::signature::Signer;

    // 1. Read and parse the private key
    let pem_bytes = std::fs::read(private_key_path).map_err(|e| {
        BoxError::OciImageError(format!(
            "Failed to read signing key '{}': {}",
            private_key_path, e
        ))
    })?;
    let signing_key = parse_pem_private_key(&pem_bytes)
        .map_err(|e| BoxError::OciImageError(format!("Failed to parse signing key: {}", e)))?;

    // 2. Build the SimpleSigning payload
    let payload = CosignPayload {
        critical: CosignCritical {
            identity: CosignIdentity {
                docker_reference: docker_reference.to_string(),
            },
            image: CosignImage {
                docker_manifest_digest: manifest_digest.to_string(),
            },
            sig_type: "cosign container image signature".to_string(),
        },
        optional: serde_json::json!({}),
    };
    let payload_bytes = serde_json::to_vec(&payload).map_err(|e| {
        BoxError::SerializationError(format!("Failed to serialize cosign payload: {}", e))
    })?;

    // 3. Sign the payload with ECDSA P-256
    let signature: p256::ecdsa::DerSignature = signing_key.sign(&payload_bytes);

    // 4. Build the cosign signature envelope
    let envelope = CosignSignatureEnvelope {
        payload: base64_encode(&payload_bytes),
        signature: base64_encode(signature.as_bytes()),
    };
    let envelope_bytes = serde_json::to_vec(&envelope).map_err(|e| {
        BoxError::SerializationError(format!("Failed to serialize signature envelope: {}", e))
    })?;

    // 5. Push the signature as an OCI image with the .sig tag
    let sig_tag = cosign_signature_tag(manifest_digest);
    let sig_reference_str = format!("{}/{}:{}", registry, repository, sig_tag);

    let sig_reference: Reference =
        sig_reference_str
            .parse()
            .map_err(|e| BoxError::RegistryError {
                registry: registry.to_string(),
                message: format!("Invalid signature reference: {}", e),
            })?;

    let config = oci_distribution::client::ClientConfig {
        protocol: oci_distribution::client::ClientProtocol::Https,
        ..Default::default()
    };
    let client = Client::new(config);

    // The signature layer uses the cosign media type
    let sig_layer = oci_distribution::client::ImageLayer::new(
        envelope_bytes,
        "application/vnd.dev.cosign.simplesigning.v1+json".to_string(),
        None,
    );

    // Empty config for the signature image
    let sig_config = oci_distribution::client::Config::new(
        b"{}".to_vec(),
        "application/vnd.oci.image.config.v1+json".to_string(),
        None,
    );

    client
        .push(
            &sig_reference,
            &[sig_layer],
            sig_config,
            &RegistryAuth::Anonymous,
            None,
        )
        .await
        .map_err(|e| BoxError::RegistryError {
            registry: registry.to_string(),
            message: format!("Failed to push signature artifact: {}", e),
        })?;

    tracing::info!(
        digest = %manifest_digest,
        signature_tag = %sig_tag,
        "Image signed and signature pushed"
    );

    Ok(SignResult {
        signature_tag: sig_tag,
    })
}

/// Parse a PEM-encoded ECDSA P-256 private key.
///
/// Supports "EC PRIVATE KEY" (SEC1) and "PRIVATE KEY" (PKCS#8) PEM formats.
pub(super) fn parse_pem_private_key(
    pem_bytes: &[u8],
) -> std::result::Result<p256::ecdsa::SigningKey, String> {
    let pem_str = std::str::from_utf8(pem_bytes)
        .map_err(|e| format!("PEM file is not valid UTF-8: {}", e))?;

    let der_bytes = if pem_str.contains("BEGIN EC PRIVATE KEY") {
        // SEC1 format
        extract_pem_content(
            pem_str,
            "-----BEGIN EC PRIVATE KEY-----",
            "-----END EC PRIVATE KEY-----",
        )?
    } else if pem_str.contains("BEGIN PRIVATE KEY") {
        // PKCS#8 format
        extract_pem_content(
            pem_str,
            "-----BEGIN PRIVATE KEY-----",
            "-----END PRIVATE KEY-----",
        )?
    } else {
        return Err("Unsupported PEM format: expected EC PRIVATE KEY or PRIVATE KEY".to_string());
    };

    // Try SEC1 first, then PKCS#8
    if let Ok(key) = p256::SecretKey::from_sec1_der(&der_bytes) {
        return Ok(p256::ecdsa::SigningKey::from(key));
    }

    // Try PKCS#8
    use p256::pkcs8::DecodePrivateKey;
    p256::SecretKey::from_pkcs8_der(&der_bytes)
        .map(p256::ecdsa::SigningKey::from)
        .map_err(|e| format!("Failed to parse P-256 private key: {}", e))
}

/// Extract base64 content between PEM markers.
pub(super) fn extract_pem_content(
    pem_str: &str,
    begin_marker: &str,
    end_marker: &str,
) -> std::result::Result<Vec<u8>, String> {
    let start = pem_str
        .find(begin_marker)
        .ok_or("Missing PEM begin marker")?
        + begin_marker.len();
    let end = pem_str.find(end_marker).ok_or("Missing PEM end marker")?;

    let b64: String = pem_str[start..end]
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect();

    base64_decode(&b64).map_err(|e| format!("Failed to decode PEM base64: {}", e))
}
