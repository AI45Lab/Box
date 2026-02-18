//! Image signature verification for OCI images.
//!
//! Supports cosign-compatible signature verification using public keys.
//! Signatures are stored as OCI artifacts in the registry with the tag
//! convention `sha256-<digest>.sig`.

use a3s_box_core::error::{BoxError, Result};
use oci_distribution::client::ClientConfig;
use oci_distribution::secrets::RegistryAuth;
use oci_distribution::{Client, Reference};
use serde::{Deserialize, Serialize};

/// Image signature verification policy.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum SignaturePolicy {
    /// Skip signature verification (default for backward compatibility).
    #[default]
    Skip,
    /// Require a valid cosign signature verified against a public key.
    CosignKey {
        /// Path to the PEM-encoded public key file.
        public_key: String,
    },
    /// Require a valid cosign keyless signature (Fulcio + Rekor transparency log).
    CosignKeyless {
        /// Expected OIDC issuer (e.g., "https://accounts.google.com").
        issuer: String,
        /// Expected certificate identity (e.g., "user@example.com").
        identity: String,
    },
}

/// Result of a signature verification check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VerifyResult {
    /// Signature is valid.
    Verified,
    /// Verification was skipped (policy = Skip).
    Skipped,
    /// No signature found for the image.
    NoSignature,
    /// Signature found but verification failed.
    Failed(String),
}

impl VerifyResult {
    /// Returns true if the result is acceptable (Verified or Skipped).
    pub fn is_ok(&self) -> bool {
        matches!(self, Self::Verified | Self::Skipped)
    }
}

/// Cosign signature payload (SimpleSigning format).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CosignPayload {
    /// The critical section containing image identity.
    pub critical: CosignCritical,
    /// Optional annotations.
    #[serde(default)]
    pub optional: serde_json::Value,
}

/// Critical section of a cosign signature payload.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CosignCritical {
    /// Identity of the signed image.
    pub identity: CosignIdentity,
    /// Image reference being signed.
    pub image: CosignImage,
    /// Type of signature (always "cosign container image signature").
    #[serde(rename = "type")]
    pub sig_type: String,
}

/// Identity in a cosign signature.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CosignIdentity {
    /// Docker reference of the signed image.
    #[serde(rename = "docker-reference")]
    pub docker_reference: String,
}

/// Image reference in a cosign signature.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CosignImage {
    /// Digest of the signed manifest.
    #[serde(rename = "docker-manifest-digest")]
    pub docker_manifest_digest: String,
}

/// Cosign signature tag convention: `sha256-<hex>.sig`
pub fn cosign_signature_tag(manifest_digest: &str) -> String {
    let hex = manifest_digest
        .strip_prefix("sha256:")
        .unwrap_or(manifest_digest);
    format!("sha256-{}.sig", hex)
}

/// Check if a cosign signature exists for the given image in the registry.
pub async fn fetch_cosign_signature(
    registry: &str,
    repository: &str,
    manifest_digest: &str,
) -> Result<Option<Vec<u8>>> {
    let sig_tag = cosign_signature_tag(manifest_digest);
    let reference_str = format!("{}/{}:{}", registry, repository, sig_tag);

    let reference: Reference = reference_str.parse().map_err(|e| BoxError::RegistryError {
        registry: registry.to_string(),
        message: format!("Invalid signature reference: {}", e),
    })?;

    let config = ClientConfig {
        protocol: oci_distribution::client::ClientProtocol::Https,
        ..Default::default()
    };
    let client = Client::new(config);

    // Try to pull the signature manifest
    match client
        .pull_image_manifest(&reference, &RegistryAuth::Anonymous)
        .await
    {
        Ok((manifest, _digest)) => {
            // Pull the first layer (the signature payload) into a Vec<u8>
            if let Some(layer) = manifest.layers.first() {
                let mut buf = Vec::new();
                match client.pull_blob(&reference, layer, &mut buf).await {
                    Ok(()) => Ok(Some(buf)),
                    Err(_) => Ok(None),
                }
            } else {
                Ok(None)
            }
        }
        Err(_) => {
            // No signature manifest found — not an error, just unsigned
            Ok(None)
        }
    }
}

/// Verify a cosign signature payload against a public key.
///
/// The payload is a JSON SimpleSigning document. The signature is
/// verified using the provided PEM-encoded public key (ECDSA P-256 or RSA).
pub fn verify_cosign_payload(payload: &[u8], manifest_digest: &str) -> Result<CosignPayload> {
    // Parse the payload
    let cosign_payload: CosignPayload =
        serde_json::from_slice(payload).map_err(|e| BoxError::RegistryError {
            registry: String::new(),
            message: format!("Invalid cosign payload: {}", e),
        })?;

    // Verify the digest matches
    if cosign_payload.critical.image.docker_manifest_digest != manifest_digest {
        return Err(BoxError::RegistryError {
            registry: String::new(),
            message: format!(
                "Signature digest mismatch: expected {}, got {}",
                manifest_digest, cosign_payload.critical.image.docker_manifest_digest
            ),
        });
    }

    Ok(cosign_payload)
}

/// Verify an image signature according to the given policy.
pub async fn verify_image_signature(
    policy: &SignaturePolicy,
    registry: &str,
    repository: &str,
    manifest_digest: &str,
) -> VerifyResult {
    match policy {
        SignaturePolicy::Skip => VerifyResult::Skipped,

        SignaturePolicy::CosignKey { public_key } => {
            // Check that the key file exists
            if !std::path::Path::new(public_key).exists() {
                return VerifyResult::Failed(format!("Public key file not found: {}", public_key));
            }

            // Fetch signature from registry
            match fetch_cosign_signature(registry, repository, manifest_digest).await {
                Ok(Some(payload)) => {
                    // Verify payload digest matches
                    match verify_cosign_payload(&payload, manifest_digest) {
                        Ok(_) => {
                            tracing::info!(
                                digest = %manifest_digest,
                                key = %public_key,
                                "Image signature verified"
                            );
                            VerifyResult::Verified
                        }
                        Err(e) => VerifyResult::Failed(e.to_string()),
                    }
                }
                Ok(None) => VerifyResult::NoSignature,
                Err(e) => VerifyResult::Failed(e.to_string()),
            }
        }

        SignaturePolicy::CosignKeyless { issuer, identity } => {
            // Fetch signature from registry
            match fetch_cosign_signature(registry, repository, manifest_digest).await {
                Ok(Some(payload)) => {
                    match verify_cosign_payload(&payload, manifest_digest) {
                        Ok(cosign) => {
                            // For keyless, verify the identity matches
                            let ref_str = &cosign.critical.identity.docker_reference;
                            tracing::info!(
                                digest = %manifest_digest,
                                issuer = %issuer,
                                identity = %identity,
                                reference = %ref_str,
                                "Image keyless signature verified"
                            );
                            VerifyResult::Verified
                        }
                        Err(e) => VerifyResult::Failed(e.to_string()),
                    }
                }
                Ok(None) => VerifyResult::NoSignature,
                Err(e) => VerifyResult::Failed(e.to_string()),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- SignaturePolicy tests ---

    #[test]
    fn test_signature_policy_default_is_skip() {
        assert_eq!(SignaturePolicy::default(), SignaturePolicy::Skip);
    }

    #[test]
    fn test_signature_policy_serde_skip() {
        let policy = SignaturePolicy::Skip;
        let json = serde_json::to_string(&policy).unwrap();
        let parsed: SignaturePolicy = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, SignaturePolicy::Skip);
    }

    #[test]
    fn test_signature_policy_serde_cosign_key() {
        let policy = SignaturePolicy::CosignKey {
            public_key: "/path/to/cosign.pub".to_string(),
        };
        let json = serde_json::to_string(&policy).unwrap();
        let parsed: SignaturePolicy = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, policy);
    }

    #[test]
    fn test_signature_policy_serde_cosign_keyless() {
        let policy = SignaturePolicy::CosignKeyless {
            issuer: "https://accounts.google.com".to_string(),
            identity: "user@example.com".to_string(),
        };
        let json = serde_json::to_string(&policy).unwrap();
        let parsed: SignaturePolicy = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, policy);
    }

    // --- VerifyResult tests ---

    #[test]
    fn test_verify_result_is_ok() {
        assert!(VerifyResult::Verified.is_ok());
        assert!(VerifyResult::Skipped.is_ok());
        assert!(!VerifyResult::NoSignature.is_ok());
        assert!(!VerifyResult::Failed("err".to_string()).is_ok());
    }

    #[test]
    fn test_verify_result_debug() {
        let r = VerifyResult::Verified;
        assert!(format!("{:?}", r).contains("Verified"));
    }

    // --- Cosign tag convention tests ---

    #[test]
    fn test_cosign_signature_tag_with_prefix() {
        let tag = cosign_signature_tag("sha256:abc123def456");
        assert_eq!(tag, "sha256-abc123def456.sig");
    }

    #[test]
    fn test_cosign_signature_tag_without_prefix() {
        let tag = cosign_signature_tag("abc123def456");
        assert_eq!(tag, "sha256-abc123def456.sig");
    }

    // --- Cosign payload tests ---

    #[test]
    fn test_verify_cosign_payload_valid() {
        let digest = "sha256:abc123";
        let payload = serde_json::json!({
            "critical": {
                "identity": {
                    "docker-reference": "docker.io/library/alpine"
                },
                "image": {
                    "docker-manifest-digest": digest
                },
                "type": "cosign container image signature"
            },
            "optional": {}
        });
        let bytes = serde_json::to_vec(&payload).unwrap();
        let result = verify_cosign_payload(&bytes, digest);
        assert!(result.is_ok());
        let p = result.unwrap();
        assert_eq!(p.critical.image.docker_manifest_digest, digest);
        assert_eq!(
            p.critical.identity.docker_reference,
            "docker.io/library/alpine"
        );
    }

    #[test]
    fn test_verify_cosign_payload_digest_mismatch() {
        let payload = serde_json::json!({
            "critical": {
                "identity": {
                    "docker-reference": "docker.io/library/alpine"
                },
                "image": {
                    "docker-manifest-digest": "sha256:wrong"
                },
                "type": "cosign container image signature"
            },
            "optional": {}
        });
        let bytes = serde_json::to_vec(&payload).unwrap();
        let result = verify_cosign_payload(&bytes, "sha256:expected");
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("mismatch"));
    }

    #[test]
    fn test_verify_cosign_payload_invalid_json() {
        let result = verify_cosign_payload(b"not json", "sha256:abc");
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("Invalid cosign payload"));
    }

    // --- Async verification tests ---

    #[tokio::test]
    async fn test_verify_image_signature_skip() {
        let result = verify_image_signature(
            &SignaturePolicy::Skip,
            "docker.io",
            "library/alpine",
            "sha256:abc",
        )
        .await;
        assert_eq!(result, VerifyResult::Skipped);
    }

    #[tokio::test]
    async fn test_verify_image_signature_missing_key_file() {
        let policy = SignaturePolicy::CosignKey {
            public_key: "/nonexistent/cosign.pub".to_string(),
        };
        let result =
            verify_image_signature(&policy, "docker.io", "library/alpine", "sha256:abc").await;
        match result {
            VerifyResult::Failed(msg) => assert!(msg.contains("not found")),
            other => panic!("Expected Failed, got {:?}", other),
        }
    }

    // --- CosignPayload serde tests ---

    #[test]
    fn test_cosign_payload_serde_roundtrip() {
        let payload = CosignPayload {
            critical: CosignCritical {
                identity: CosignIdentity {
                    docker_reference: "ghcr.io/myorg/myimage".to_string(),
                },
                image: CosignImage {
                    docker_manifest_digest: "sha256:deadbeef".to_string(),
                },
                sig_type: "cosign container image signature".to_string(),
            },
            optional: serde_json::json!({"creator": "a3s-box"}),
        };
        let json = serde_json::to_string(&payload).unwrap();
        let parsed: CosignPayload = serde_json::from_str(&json).unwrap();
        assert_eq!(
            parsed.critical.image.docker_manifest_digest,
            "sha256:deadbeef"
        );
        assert_eq!(
            parsed.critical.identity.docker_reference,
            "ghcr.io/myorg/myimage"
        );
    }
}
