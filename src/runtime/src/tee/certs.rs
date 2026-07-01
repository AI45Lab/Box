//! AMD certificate chain fetching and caching.
//!
//! Fetches VCEK, ASK, and ARK certificates from the AMD Key Distribution
//! Service (KDS) at `kds.amd.com`. Certificates are cached locally to
//! avoid repeated network requests.

use a3s_box_core::error::{BoxError, Result};
use std::path::PathBuf;

use super::attestation::{CertificateChain, TcbVersion};

/// AMD KDS base URL for SEV-SNP certificates.
const AMD_KDS_BASE_URL: &str = "https://kds.amd.com";

/// AMD KDS VCEK endpoint path.
const AMD_KDS_VCEK_PATH: &str = "vcek/v1";

/// AMD product name for Milan (3rd gen EPYC).
const PRODUCT_MILAN: &str = "Milan";

/// AMD product name for Genoa (4th gen EPYC).
const PRODUCT_GENOA: &str = "Genoa";

/// Client for fetching certificates from AMD KDS.
pub struct AmdKdsClient {
    /// HTTP client for KDS requests.
    http: reqwest::Client,
    /// Local cache directory for certificates.
    cache_dir: Option<PathBuf>,
}

impl AmdKdsClient {
    /// Create a new AMD KDS client.
    ///
    /// # Arguments
    /// * `cache_dir` - Optional directory for caching certificates locally.
    ///   If `None`, certificates are fetched on every request.
    pub fn new(cache_dir: Option<PathBuf>) -> Self {
        Self {
            http: reqwest::Client::builder()
                .no_proxy()
                .build()
                .expect("failed to build AMD KDS HTTP client"),
            cache_dir,
        }
    }

    /// Fetch the complete certificate chain for verifying an SNP report.
    ///
    /// Tries the local cache first, then falls back to AMD KDS.
    ///
    /// # Arguments
    /// * `chip_id` - Hex-encoded chip ID from the SNP report (128 hex chars)
    /// * `tcb` - TCB version from the SNP report
    /// * `product` - CPU product name ("Milan" or "Genoa")
    pub async fn fetch_cert_chain(
        &self,
        chip_id: &str,
        tcb: &TcbVersion,
        product: &str,
    ) -> Result<CertificateChain> {
        // Try cache first
        if let Some(cached) = self.load_from_cache(chip_id, tcb).await {
            tracing::debug!(chip_id = &chip_id[..16], "Using cached certificate chain");
            return Ok(cached);
        }

        // Fetch VCEK certificate
        let vcek = self.fetch_vcek(chip_id, tcb, product).await?;

        // Fetch ASK + ARK certificate chain
        let (ask, ark) = self.fetch_ask_ark(product).await?;

        let chain = CertificateChain { vcek, ask, ark };

        // Cache for future use
        self.save_to_cache(chip_id, tcb, &chain).await;

        Ok(chain)
    }

    /// Fetch the VCEK certificate from AMD KDS.
    ///
    /// URL format: `https://kds.amd.com/vcek/v1/{product}/{chip_id}?blSPL={bl}&teeSPL={tee}&snpSPL={snp}&ucodeSPL={ucode}`
    async fn fetch_vcek(&self, chip_id: &str, tcb: &TcbVersion, product: &str) -> Result<Vec<u8>> {
        let url = format!(
            "{}/{}/{}/{}?blSPL={}&teeSPL={}&snpSPL={}&ucodeSPL={}",
            AMD_KDS_BASE_URL,
            AMD_KDS_VCEK_PATH,
            product,
            chip_id,
            tcb.boot_loader,
            tcb.tee,
            tcb.snp,
            tcb.microcode,
        );

        tracing::debug!(url = %url, "Fetching VCEK from AMD KDS");

        let response = self
            .http
            .get(&url)
            .header("Accept", "application/x-pem-file")
            .send()
            .await
            .map_err(|e| {
                BoxError::AttestationError(format!("Failed to fetch VCEK from AMD KDS: {}", e))
            })?;

        if !response.status().is_success() {
            return Err(BoxError::AttestationError(format!(
                "AMD KDS returned {} for VCEK request",
                response.status()
            )));
        }

        response
            .bytes()
            .await
            .map(|b| b.to_vec())
            .map_err(|e| BoxError::AttestationError(format!("Failed to read VCEK response: {}", e)))
    }

    /// Fetch the ASK and ARK certificates from AMD KDS.
    ///
    /// URL: `https://kds.amd.com/vcek/v1/{product}/cert_chain`
    /// Returns a PEM bundle containing both ASK and ARK.
    async fn fetch_ask_ark(&self, product: &str) -> Result<(Vec<u8>, Vec<u8>)> {
        let url = format!(
            "{}/{}/{}/{}",
            AMD_KDS_BASE_URL, AMD_KDS_VCEK_PATH, product, "cert_chain",
        );

        tracing::debug!(url = %url, "Fetching ASK+ARK from AMD KDS");

        let response = self
            .http
            .get(&url)
            .header("Accept", "application/x-pem-file")
            .send()
            .await
            .map_err(|e| {
                BoxError::AttestationError(format!(
                    "Failed to fetch cert chain from AMD KDS: {}",
                    e
                ))
            })?;

        if !response.status().is_success() {
            return Err(BoxError::AttestationError(format!(
                "AMD KDS returned {} for cert chain request",
                response.status()
            )));
        }

        let pem_bundle = response.bytes().await.map_err(|e| {
            BoxError::AttestationError(format!("Failed to read cert chain response: {}", e))
        })?;

        // The PEM bundle contains two certificates: ASK first, then ARK.
        // Split them by finding the PEM boundaries.
        Self::split_pem_bundle(&pem_bundle)
    }

    /// Split a PEM bundle containing ASK and ARK into separate DER blobs.
    fn split_pem_bundle(bundle: &[u8]) -> Result<(Vec<u8>, Vec<u8>)> {
        let pem_str = String::from_utf8_lossy(bundle);
        let certs: Vec<&str> = pem_str
            .split("-----END CERTIFICATE-----")
            .filter(|s| s.contains("-----BEGIN CERTIFICATE-----"))
            .collect();

        if certs.len() < 2 {
            return Err(BoxError::AttestationError(format!(
                "Expected 2 certificates in PEM bundle, found {}",
                certs.len()
            )));
        }

        let ask = Self::pem_to_der(certs[0])?;
        let ark = Self::pem_to_der(certs[1])?;

        Ok((ask, ark))
    }

    /// Convert a PEM certificate to DER bytes.
    fn pem_to_der(pem: &str) -> Result<Vec<u8>> {
        let b64: String = pem
            .lines()
            .filter(|line| !line.starts_with("-----") && !line.is_empty())
            .collect();

        base64_decode(&b64).map_err(|e| {
            BoxError::AttestationError(format!("Failed to decode PEM certificate: {}", e))
        })
    }

    /// Try to load a cached certificate chain.
    async fn load_from_cache(&self, chip_id: &str, tcb: &TcbVersion) -> Option<CertificateChain> {
        let cache_dir = self.cache_dir.as_ref()?;
        let cache_key = Self::cache_key(chip_id, tcb);
        let cache_path = cache_dir.join(&cache_key);

        let data = tokio::fs::read(&cache_path).await.ok()?;
        serde_json::from_slice(&data).ok()
    }

    /// Save a certificate chain to the local cache.
    async fn save_to_cache(&self, chip_id: &str, tcb: &TcbVersion, chain: &CertificateChain) {
        let Some(cache_dir) = &self.cache_dir else {
            return;
        };

        if let Err(e) = tokio::fs::create_dir_all(cache_dir).await {
            tracing::warn!("Failed to create cert cache dir: {}", e);
            return;
        }

        let cache_key = Self::cache_key(chip_id, tcb);
        let cache_path = cache_dir.join(&cache_key);

        match serde_json::to_vec(chain) {
            Ok(data) => {
                if let Err(e) = tokio::fs::write(&cache_path, &data).await {
                    tracing::warn!("Failed to cache certificate chain: {}", e);
                }
            }
            Err(e) => {
                tracing::warn!("Failed to serialize certificate chain for cache: {}", e);
            }
        }
    }

    /// Generate a cache key from chip ID and TCB version.
    fn cache_key(chip_id: &str, tcb: &TcbVersion) -> String {
        // Use first 16 chars of chip_id + TCB components for uniqueness
        let short_id = &chip_id[..chip_id.len().min(16)];
        format!(
            "snp_certs_{}_bl{}_tee{}_snp{}_uc{}.json",
            short_id, tcb.boot_loader, tcb.tee, tcb.snp, tcb.microcode,
        )
    }

    /// Get the product name string for AMD KDS.
    pub fn product_name(generation: &str) -> &'static str {
        match generation.to_lowercase().as_str() {
            "milan" => PRODUCT_MILAN,
            "genoa" => PRODUCT_GENOA,
            _ => PRODUCT_MILAN, // Default to Milan
        }
    }
}

/// Decode a base64 string (standard alphabet, tolerates whitespace and missing padding).
fn base64_decode(input: &str) -> std::result::Result<Vec<u8>, String> {
    use base64::{engine::general_purpose, Engine};
    // Strip whitespace before decoding (PEM base64 contains newlines)
    let cleaned: String = input.chars().filter(|c| !c.is_whitespace()).collect();
    // Use STANDARD_NO_PAD to tolerate both padded and unpadded input
    general_purpose::STANDARD_NO_PAD
        .decode(&cleaned)
        .or_else(|_| general_purpose::STANDARD.decode(&cleaned))
        .map_err(|e| format!("base64 decode error: {}", e))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_product_name() {
        assert_eq!(AmdKdsClient::product_name("milan"), "Milan");
        assert_eq!(AmdKdsClient::product_name("Milan"), "Milan");
        assert_eq!(AmdKdsClient::product_name("genoa"), "Genoa");
        assert_eq!(AmdKdsClient::product_name("Genoa"), "Genoa");
        assert_eq!(AmdKdsClient::product_name("unknown"), "Milan");
    }

    #[test]
    fn test_cache_key() {
        let tcb = TcbVersion {
            boot_loader: 3,
            tee: 0,
            snp: 8,
            microcode: 115,
        };
        let key = AmdKdsClient::cache_key("abcdef1234567890aabbccdd", &tcb);
        assert_eq!(key, "snp_certs_abcdef1234567890_bl3_tee0_snp8_uc115.json");
    }

    #[test]
    fn test_base64_decode() {
        let encoded = "SGVsbG8gV29ybGQ=";
        let decoded = base64_decode(encoded).unwrap();
        assert_eq!(decoded, b"Hello World");
    }

    #[test]
    fn test_base64_decode_no_padding() {
        let encoded = "SGVsbG8";
        let decoded = base64_decode(encoded).unwrap();
        assert_eq!(decoded, b"Hello");
    }

    #[test]
    fn test_base64_decode_with_newlines() {
        let encoded = "SGVs\nbG8g\nV29ybGQ=";
        let decoded = base64_decode(encoded).unwrap();
        assert_eq!(decoded, b"Hello World");
    }

    #[test]
    fn test_split_pem_bundle() {
        let bundle = b"-----BEGIN CERTIFICATE-----\nAQID\n-----END CERTIFICATE-----\n-----BEGIN CERTIFICATE-----\nBAUG\n-----END CERTIFICATE-----\n";
        let (ask, ark) = AmdKdsClient::split_pem_bundle(bundle).unwrap();
        assert_eq!(ask, vec![1, 2, 3]);
        assert_eq!(ark, vec![4, 5, 6]);
    }

    #[test]
    fn test_split_pem_bundle_too_few() {
        let bundle = b"-----BEGIN CERTIFICATE-----\nAQID\n-----END CERTIFICATE-----\n";
        let result = AmdKdsClient::split_pem_bundle(bundle);
        assert!(result.is_err());
    }

    #[test]
    fn test_pem_to_der() {
        let pem = "-----BEGIN CERTIFICATE-----\nAQID\n-----END CERTIFICATE-----";
        let der = AmdKdsClient::pem_to_der(pem).unwrap();
        assert_eq!(der, vec![1, 2, 3]);
    }

    #[test]
    fn test_kds_client_creation() {
        let client = AmdKdsClient::new(None);
        assert!(client.cache_dir.is_none());

        let client = AmdKdsClient::new(Some(PathBuf::from("/tmp/test-certs")));
        assert_eq!(client.cache_dir, Some(PathBuf::from("/tmp/test-certs")));
    }
}
