//! KBS (Key Broker Service) client for TEE secret provisioning.
//!
//! Implements the IETF RATS (Remote ATtestation procedureS) challenge-response
//! protocol for fetching secrets from a Key Broker Service:
//!
//! 1. Client sends attestation evidence (SNP report) to KBS
//! 2. KBS verifies the report against its policy
//! 3. If verified, KBS returns the requested secret(s)
//!
//! Supports both single-key fetch and batch resource retrieval.

use std::collections::HashMap;

use a3s_box_core::error::{BoxError, Result};
use serde::{Deserialize, Serialize};

/// KBS endpoint configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KbsConfig {
    /// KBS server URL (e.g., "https://kbs.example.com")
    pub url: String,
    /// Optional API key for authentication
    #[serde(default)]
    pub api_key: Option<String>,
    /// Request timeout in seconds (default: 30)
    #[serde(default = "default_timeout")]
    pub timeout_secs: u64,
    /// Whether to accept self-signed TLS certificates (for testing)
    #[serde(default)]
    pub insecure_tls: bool,
}

fn default_timeout() -> u64 {
    30
}

impl Default for KbsConfig {
    fn default() -> Self {
        Self {
            url: String::new(),
            api_key: None,
            timeout_secs: 30,
            insecure_tls: false,
        }
    }
}

/// A request to the KBS for secret provisioning.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KbsRequest {
    /// Resource path (e.g., "default/keys/my-secret")
    pub resource_path: String,
    /// TEE evidence: base64-encoded SNP attestation report
    pub evidence: String,
    /// TEE type identifier
    #[serde(default = "default_tee_type")]
    pub tee_type: String,
    /// Additional claims to include in the request
    #[serde(default)]
    pub extra_claims: HashMap<String, String>,
}

fn default_tee_type() -> String {
    "snp".to_string()
}

/// Response from the KBS.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KbsResponse {
    /// Whether the attestation was verified successfully
    pub verified: bool,
    /// The secret payload (base64-encoded), if verification passed
    pub payload: Option<String>,
    /// Error message if verification failed
    pub error: Option<String>,
    /// Token for subsequent requests (session-based KBS)
    #[serde(default)]
    pub token: Option<String>,
}

/// Result of a KBS secret fetch operation.
#[derive(Debug, Clone)]
pub struct KbsSecret {
    /// Resource path that was fetched
    pub resource_path: String,
    /// Decoded secret bytes
    pub secret: Vec<u8>,
    /// Optional token for session continuity
    pub token: Option<String>,
}

/// KBS client for fetching secrets from a Key Broker Service.
pub struct KbsClient {
    config: KbsConfig,
}

impl KbsClient {
    /// Create a new KBS client with the given configuration.
    pub fn new(config: KbsConfig) -> Self {
        Self { config }
    }

    /// Build the attestation request payload.
    pub fn build_request(&self, resource_path: &str, evidence: &[u8]) -> KbsRequest {
        use base64::Engine;
        KbsRequest {
            resource_path: resource_path.to_string(),
            evidence: base64::engine::general_purpose::STANDARD.encode(evidence),
            tee_type: "snp".to_string(),
            extra_claims: HashMap::new(),
        }
    }

    /// Parse a KBS response and extract the secret.
    pub fn parse_response(&self, resource_path: &str, response: &KbsResponse) -> Result<KbsSecret> {
        if !response.verified {
            return Err(BoxError::AttestationError(format!(
                "KBS attestation verification failed: {}",
                response.error.as_deref().unwrap_or("unknown error")
            )));
        }

        let payload = response.payload.as_ref().ok_or_else(|| {
            BoxError::AttestationError("KBS response verified but no payload returned".to_string())
        })?;

        use base64::Engine;
        let secret = base64::engine::general_purpose::STANDARD
            .decode(payload)
            .map_err(|e| {
                BoxError::AttestationError(format!("Failed to decode KBS payload: {}", e))
            })?;

        Ok(KbsSecret {
            resource_path: resource_path.to_string(),
            secret,
            token: response.token.clone(),
        })
    }

    /// Get the KBS endpoint URL for a resource path.
    pub fn resource_url(&self, resource_path: &str) -> String {
        let base = self.config.url.trim_end_matches('/');
        format!("{}/kbs/v0/resource/{}", base, resource_path)
    }

    /// Get the KBS attestation endpoint URL.
    pub fn attest_url(&self) -> String {
        let base = self.config.url.trim_end_matches('/');
        format!("{}/kbs/v0/attest", base)
    }

    /// Get the configuration.
    pub fn config(&self) -> &KbsConfig {
        &self.config
    }
}

/// Parse a KBS resource path into (repository, type, tag).
///
/// Format: `repository/type/tag` (e.g., `default/keys/my-secret`)
pub fn parse_resource_path(path: &str) -> Result<(&str, &str, &str)> {
    let parts: Vec<&str> = path.splitn(3, '/').collect();
    if parts.len() != 3 {
        return Err(BoxError::AttestationError(format!(
            "Invalid KBS resource path '{}': expected 'repository/type/tag'",
            path
        )));
    }
    Ok((parts[0], parts[1], parts[2]))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_kbs_config_default() {
        let config = KbsConfig::default();
        assert!(config.url.is_empty());
        assert!(config.api_key.is_none());
        assert_eq!(config.timeout_secs, 30);
        assert!(!config.insecure_tls);
    }

    #[test]
    fn test_kbs_config_serde_roundtrip() {
        let config = KbsConfig {
            url: "https://kbs.example.com".to_string(),
            api_key: Some("secret-key".to_string()),
            timeout_secs: 60,
            insecure_tls: true,
        };
        let json = serde_json::to_string(&config).unwrap();
        let parsed: KbsConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.url, "https://kbs.example.com");
        assert_eq!(parsed.api_key, Some("secret-key".to_string()));
        assert_eq!(parsed.timeout_secs, 60);
        assert!(parsed.insecure_tls);
    }

    #[test]
    fn test_kbs_client_build_request() {
        let config = KbsConfig {
            url: "https://kbs.example.com".to_string(),
            ..Default::default()
        };
        let client = KbsClient::new(config);
        let evidence = b"fake-snp-report";
        let request = client.build_request("default/keys/my-key", evidence);

        assert_eq!(request.resource_path, "default/keys/my-key");
        assert_eq!(request.tee_type, "snp");
        assert!(!request.evidence.is_empty());

        // Verify base64 roundtrip
        use base64::Engine;
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(&request.evidence)
            .unwrap();
        assert_eq!(decoded, evidence);
    }

    #[test]
    fn test_kbs_client_parse_response_success() {
        let config = KbsConfig::default();
        let client = KbsClient::new(config);

        use base64::Engine;
        let secret_data = b"my-secret-value";
        let payload = base64::engine::general_purpose::STANDARD.encode(secret_data);

        let response = KbsResponse {
            verified: true,
            payload: Some(payload),
            error: None,
            token: Some("session-token".to_string()),
        };

        let secret = client
            .parse_response("default/keys/test", &response)
            .unwrap();
        assert_eq!(secret.resource_path, "default/keys/test");
        assert_eq!(secret.secret, secret_data);
        assert_eq!(secret.token, Some("session-token".to_string()));
    }

    #[test]
    fn test_kbs_client_parse_response_verification_failed() {
        let config = KbsConfig::default();
        let client = KbsClient::new(config);

        let response = KbsResponse {
            verified: false,
            payload: None,
            error: Some("measurement mismatch".to_string()),
            token: None,
        };

        let result = client.parse_response("default/keys/test", &response);
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("measurement mismatch"));
    }

    #[test]
    fn test_kbs_client_parse_response_no_payload() {
        let config = KbsConfig::default();
        let client = KbsClient::new(config);

        let response = KbsResponse {
            verified: true,
            payload: None,
            error: None,
            token: None,
        };

        let result = client.parse_response("default/keys/test", &response);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("no payload"));
    }

    #[test]
    fn test_kbs_client_parse_response_invalid_base64() {
        let config = KbsConfig::default();
        let client = KbsClient::new(config);

        let response = KbsResponse {
            verified: true,
            payload: Some("not-valid-base64!!!".to_string()),
            error: None,
            token: None,
        };

        let result = client.parse_response("default/keys/test", &response);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("decode"));
    }

    #[test]
    fn test_kbs_client_resource_url() {
        let config = KbsConfig {
            url: "https://kbs.example.com".to_string(),
            ..Default::default()
        };
        let client = KbsClient::new(config);
        assert_eq!(
            client.resource_url("default/keys/my-key"),
            "https://kbs.example.com/kbs/v0/resource/default/keys/my-key"
        );
    }

    #[test]
    fn test_kbs_client_resource_url_trailing_slash() {
        let config = KbsConfig {
            url: "https://kbs.example.com/".to_string(),
            ..Default::default()
        };
        let client = KbsClient::new(config);
        assert_eq!(
            client.resource_url("default/keys/my-key"),
            "https://kbs.example.com/kbs/v0/resource/default/keys/my-key"
        );
    }

    #[test]
    fn test_kbs_client_attest_url() {
        let config = KbsConfig {
            url: "https://kbs.example.com".to_string(),
            ..Default::default()
        };
        let client = KbsClient::new(config);
        assert_eq!(client.attest_url(), "https://kbs.example.com/kbs/v0/attest");
    }

    #[test]
    fn test_parse_resource_path_valid() {
        let (repo, rtype, tag) = parse_resource_path("default/keys/my-secret").unwrap();
        assert_eq!(repo, "default");
        assert_eq!(rtype, "keys");
        assert_eq!(tag, "my-secret");
    }

    #[test]
    fn test_parse_resource_path_with_nested_tag() {
        let (repo, rtype, tag) = parse_resource_path("myrepo/certs/tls/server.pem").unwrap();
        assert_eq!(repo, "myrepo");
        assert_eq!(rtype, "certs");
        assert_eq!(tag, "tls/server.pem");
    }

    #[test]
    fn test_parse_resource_path_invalid_too_few() {
        assert!(parse_resource_path("default/keys").is_err());
        assert!(parse_resource_path("just-one").is_err());
        assert!(parse_resource_path("").is_err());
    }

    #[test]
    fn test_kbs_request_serde_roundtrip() {
        let request = KbsRequest {
            resource_path: "default/keys/test".to_string(),
            evidence: "base64evidence".to_string(),
            tee_type: "snp".to_string(),
            extra_claims: HashMap::from([("nonce".to_string(), "abc123".to_string())]),
        };
        let json = serde_json::to_string(&request).unwrap();
        let parsed: KbsRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.resource_path, "default/keys/test");
        assert_eq!(parsed.evidence, "base64evidence");
        assert_eq!(parsed.tee_type, "snp");
        assert_eq!(parsed.extra_claims.get("nonce").unwrap(), "abc123");
    }

    #[test]
    fn test_kbs_response_serde_roundtrip() {
        let response = KbsResponse {
            verified: true,
            payload: Some("c2VjcmV0".to_string()),
            error: None,
            token: Some("tok-123".to_string()),
        };
        let json = serde_json::to_string(&response).unwrap();
        let parsed: KbsResponse = serde_json::from_str(&json).unwrap();
        assert!(parsed.verified);
        assert_eq!(parsed.payload, Some("c2VjcmV0".to_string()));
        assert!(parsed.error.is_none());
        assert_eq!(parsed.token, Some("tok-123".to_string()));
    }
}
