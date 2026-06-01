//! OCI registry client for pulling and pushing images.
//!
//! Uses the `oci-distribution` crate to interact with container registries
//! (Docker Hub, GHCR, etc.).

use std::path::{Path, PathBuf};
use std::sync::Arc;

use a3s_box_core::error::{BoxError, Result};
use oci_distribution::client::{ClientConfig, ClientProtocol, Config, ImageLayer, PushResponse};
use oci_distribution::manifest::{ImageIndexEntry, OciImageManifest};
use oci_distribution::secrets::RegistryAuth as OciRegistryAuth;
use oci_distribution::{Client, Reference};

use super::credentials::CredentialStore;
use super::reference::ImageReference;
use super::signing::{verify_image_signature, SignaturePolicy, VerifyResult};

const REGISTRY_PROTOCOL_ENV: &str = "A3S_REGISTRY_PROTOCOL";

fn registry_protocol_from_env() -> ClientProtocol {
    match std::env::var(REGISTRY_PROTOCOL_ENV) {
        Ok(value) if value.eq_ignore_ascii_case("http") => ClientProtocol::Http,
        _ => ClientProtocol::Https,
    }
}

/// An `AsyncWrite` that streams bytes straight to a file while computing their
/// SHA-256, so a pulled blob is hashed and written in bounded chunks instead of
/// being fully buffered in memory.
struct HashingFileWriter {
    file: tokio::fs::File,
    hasher: sha2::Sha256,
}

impl HashingFileWriter {
    fn new(file: tokio::fs::File) -> Self {
        use sha2::Digest;
        Self {
            file,
            hasher: sha2::Sha256::new(),
        }
    }

    fn finalize_hex(self) -> String {
        use sha2::Digest;
        format!("{:x}", self.hasher.finalize())
    }
}

impl tokio::io::AsyncWrite for HashingFileWriter {
    fn poll_write(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        use sha2::Digest;
        let this = self.get_mut();
        match std::pin::Pin::new(&mut this.file).poll_write(cx, buf) {
            std::task::Poll::Ready(Ok(n)) => {
                this.hasher.update(&buf[..n]);
                std::task::Poll::Ready(Ok(n))
            }
            other => other,
        }
    }

    fn poll_flush(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.get_mut().file).poll_flush(cx)
    }

    fn poll_shutdown(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.get_mut().file).poll_shutdown(cx)
    }
}

/// Stream a blob to `dest`, verifying its SHA-256 against `descriptor.digest`
/// as it downloads. The blob is written to a `.partial` temp file and only
/// renamed into place once the digest checks out, so a failed/corrupted pull
/// never leaves a bad blob under its content-addressed name. Unknown digest
/// algorithms are stored with a warning rather than silently trusted.
async fn stream_and_verify_blob(
    client: &Client,
    oci_ref: &Reference,
    descriptor: &oci_distribution::manifest::OciDescriptor,
    dest: &Path,
    what: &str,
    registry: &str,
) -> Result<()> {
    use tokio::io::AsyncWriteExt;

    let tmp = dest.with_extension("partial");
    let file = tokio::fs::File::create(&tmp)
        .await
        .map_err(|e| BoxError::RegistryError {
            registry: registry.to_string(),
            message: format!("Failed to create {what} file: {e}"),
        })?;
    let mut writer = HashingFileWriter::new(file);

    if let Err(e) = client.pull_blob(oci_ref, descriptor, &mut writer).await {
        let _ = tokio::fs::remove_file(&tmp).await;
        return Err(BoxError::RegistryError {
            registry: registry.to_string(),
            message: format!("Failed to pull {what}: {e}"),
        });
    }
    let _ = writer.flush().await;
    let _ = writer.shutdown().await;
    let actual_hex = writer.finalize_hex();

    match descriptor.digest.strip_prefix("sha256:") {
        Some(expected_hex) if actual_hex.eq_ignore_ascii_case(expected_hex) => {}
        Some(expected_hex) => {
            let _ = tokio::fs::remove_file(&tmp).await;
            return Err(BoxError::RegistryError {
                registry: registry.to_string(),
                message: format!(
                    "{what} digest mismatch: expected sha256:{expected_hex}, computed sha256:{actual_hex}"
                ),
            });
        }
        None => {
            tracing::warn!(
                digest = %descriptor.digest,
                "Unrecognized digest algorithm; skipping {what} content verification"
            );
        }
    }

    tokio::fs::rename(&tmp, dest)
        .await
        .map_err(|e| BoxError::RegistryError {
            registry: registry.to_string(),
            message: format!("Failed to store {what} blob: {e}"),
        })
}

/// Callback type for layer pull progress: `(current, total, digest, size_bytes)`.
type PullProgressFn = Arc<dyn Fn(usize, usize, &str, i64) + Send + Sync>;

/// Authentication credentials for a container registry.
#[derive(Debug, Clone)]
pub struct RegistryAuth {
    username: Option<String>,
    password: Option<String>,
}

impl RegistryAuth {
    /// Create anonymous authentication (no credentials).
    pub fn anonymous() -> Self {
        Self {
            username: None,
            password: None,
        }
    }

    /// Create basic authentication with username and password.
    pub fn basic(username: impl Into<String>, password: impl Into<String>) -> Self {
        Self {
            username: Some(username.into()),
            password: Some(password.into()),
        }
    }

    /// Create authentication from environment variables.
    ///
    /// Reads `REGISTRY_USERNAME` and `REGISTRY_PASSWORD`.
    /// Falls back to anonymous if not set.
    pub fn from_env() -> Self {
        let username = std::env::var("REGISTRY_USERNAME").ok();
        let password = std::env::var("REGISTRY_PASSWORD").ok();

        if username.is_some() && password.is_some() {
            Self { username, password }
        } else {
            Self::anonymous()
        }
    }

    /// Create authentication from the credential store, falling back to env vars,
    /// then anonymous.
    pub fn from_credential_store(registry: &str) -> Self {
        // Try credential store first
        if let Ok(store) = CredentialStore::default_path() {
            if let Ok(Some((username, password))) = store.get(registry) {
                return Self::basic(username, password);
            }
        }
        // Fall back to env vars, then anonymous
        Self::from_env()
    }

    /// Convert to oci-distribution auth type.
    fn to_oci_auth(&self) -> OciRegistryAuth {
        match (&self.username, &self.password) {
            (Some(u), Some(p)) => OciRegistryAuth::Basic(u.clone(), p.clone()),
            _ => OciRegistryAuth::Anonymous,
        }
    }
}

/// Pulls OCI images from container registries.
pub(crate) struct RegistryPuller {
    client: Client,
    auth: RegistryAuth,
    /// Signature verification policy (default: Skip).
    signature_policy: SignaturePolicy,
    /// Optional layer progress callback: (current, total, digest, size_bytes).
    progress_fn: Option<PullProgressFn>,
}

impl Default for RegistryPuller {
    fn default() -> Self {
        Self::new()
    }
}

impl RegistryPuller {
    /// Create a new registry puller with anonymous authentication.
    pub fn new() -> Self {
        Self::with_auth(RegistryAuth::anonymous())
    }

    /// Create a new registry puller with the given authentication.
    pub fn with_auth(auth: RegistryAuth) -> Self {
        let config = ClientConfig {
            protocol: registry_protocol_from_env(),
            platform_resolver: Some(Box::new(linux_platform_resolver)),
            ..Default::default()
        };
        let client = Client::new(config);

        Self {
            client,
            auth,
            signature_policy: SignaturePolicy::default(),
            progress_fn: None,
        }
    }

    /// Set the signature verification policy.
    pub fn with_signature_policy(mut self, policy: SignaturePolicy) -> Self {
        self.signature_policy = policy;
        self
    }

    /// Set a progress callback invoked for each layer: `(current, total, digest, size_bytes)`.
    pub fn with_progress_fn(mut self, f: PullProgressFn) -> Self {
        self.progress_fn = Some(f);
        self
    }

    /// Pull an image and write it as an OCI image layout to `target_dir`.
    ///
    /// The resulting directory will contain:
    /// - `oci-layout`
    /// - `index.json`
    /// - `blobs/sha256/...`
    pub async fn pull(&self, reference: &ImageReference, target_dir: &Path) -> Result<PathBuf> {
        let oci_ref = self.to_oci_reference(reference)?;

        tracing::info!(
            reference = %reference,
            target = %target_dir.display(),
            "Pulling image from registry"
        );

        // Create target directory structure
        let blobs_dir = target_dir.join("blobs").join("sha256");
        std::fs::create_dir_all(&blobs_dir).map_err(|e| BoxError::RegistryError {
            registry: reference.registry.clone(),
            message: format!("Failed to create blobs directory: {}", e),
        })?;

        // Pull manifest (resolves multi-arch image indexes to current platform)
        let auth = self.auth.to_oci_auth();
        let (image_manifest, manifest_digest) = self
            .client
            .pull_image_manifest(&oci_ref, &auth)
            .await
            .map_err(|e| BoxError::RegistryError {
                registry: reference.registry.clone(),
                message: format!("Failed to pull manifest: {}", e),
            })?;

        // Verify image signature before downloading layers
        let verify_result = verify_image_signature(
            &self.signature_policy,
            &reference.registry,
            &reference.repository,
            &manifest_digest,
        )
        .await;

        if !verify_result.is_ok() {
            return Err(BoxError::RegistryError {
                registry: reference.registry.clone(),
                message: match verify_result {
                    VerifyResult::NoSignature => format!(
                        "Image {}:{} has no signature and policy requires verification",
                        reference.repository,
                        reference.tag.as_deref().unwrap_or("latest")
                    ),
                    VerifyResult::Failed(msg) => format!(
                        "Image signature verification failed for {}:{}: {}",
                        reference.repository,
                        reference.tag.as_deref().unwrap_or("latest"),
                        msg
                    ),
                    _ => "Signature verification failed".to_string(),
                },
            });
        }

        // Write manifest blob
        let manifest_json = serde_json::to_vec(&image_manifest)?;
        let manifest_digest_hex = manifest_digest
            .strip_prefix("sha256:")
            .unwrap_or(&manifest_digest);
        std::fs::write(blobs_dir.join(manifest_digest_hex), &manifest_json).map_err(|e| {
            BoxError::RegistryError {
                registry: reference.registry.clone(),
                message: format!("Failed to write manifest: {}", e),
            }
        })?;

        // Pull image config and layers
        self.pull_image_content(&oci_ref, &image_manifest, &blobs_dir, &reference.registry)
            .await?;

        // Write oci-layout file
        std::fs::write(
            target_dir.join("oci-layout"),
            r#"{"imageLayoutVersion":"1.0.0"}"#,
        )
        .map_err(|e| BoxError::RegistryError {
            registry: reference.registry.clone(),
            message: format!("Failed to write oci-layout: {}", e),
        })?;

        // Write index.json
        let index = serde_json::json!({
            "schemaVersion": 2,
            "manifests": [{
                "mediaType": "application/vnd.oci.image.manifest.v1+json",
                "digest": manifest_digest,
                "size": manifest_json.len()
            }]
        });
        std::fs::write(
            target_dir.join("index.json"),
            serde_json::to_string_pretty(&index)?,
        )
        .map_err(|e| BoxError::RegistryError {
            registry: reference.registry.clone(),
            message: format!("Failed to write index.json: {}", e),
        })?;

        tracing::info!(
            reference = %reference,
            digest = %manifest_digest,
            "Image pulled successfully"
        );

        Ok(target_dir.to_path_buf())
    }

    /// Pull the manifest digest string for an image reference.
    pub async fn pull_manifest_digest(&self, reference: &ImageReference) -> Result<String> {
        let oci_ref = self.to_oci_reference(reference)?;
        let auth = self.auth.to_oci_auth();

        let (_manifest, digest) =
            self.client
                .pull_manifest(&oci_ref, &auth)
                .await
                .map_err(|e| BoxError::RegistryError {
                    registry: reference.registry.clone(),
                    message: format!("Failed to pull manifest: {}", e),
                })?;

        Ok(digest)
    }

    /// Pull config and layers for an image manifest, writing blobs to disk.
    async fn pull_image_content(
        &self,
        oci_ref: &Reference,
        manifest: &OciImageManifest,
        blobs_dir: &Path,
        registry: &str,
    ) -> Result<()> {
        // Stream the config blob to disk, verifying its digest on the fly.
        // pull_blob delivers raw bytes without validation, so the streaming
        // hasher both bounds memory and guards against a buggy/malicious
        // registry or a corrupted transfer being stored content-addressed and
        // later extracted into the guest.
        let config_descriptor = &manifest.config;
        let config_digest_hex = config_descriptor
            .digest
            .strip_prefix("sha256:")
            .unwrap_or(&config_descriptor.digest);
        stream_and_verify_blob(
            &self.client,
            oci_ref,
            config_descriptor,
            &blobs_dir.join(config_digest_hex),
            "config blob",
            registry,
        )
        .await?;

        // Pull layer blobs
        let total = manifest.layers.len();
        for (idx, layer) in manifest.layers.iter().enumerate() {
            tracing::debug!(
                digest = %layer.digest,
                size = layer.size,
                "Pulling layer"
            );

            if let Some(ref f) = self.progress_fn {
                f(idx + 1, total, &layer.digest, layer.size);
            }

            let layer_digest_hex = layer
                .digest
                .strip_prefix("sha256:")
                .unwrap_or(&layer.digest);
            stream_and_verify_blob(
                &self.client,
                oci_ref,
                layer,
                &blobs_dir.join(layer_digest_hex),
                "layer",
                registry,
            )
            .await?;

            // Call progress callback again with negative size to signal completion
            if let Some(ref f) = self.progress_fn {
                f(idx + 1, total, &layer.digest, -(layer.size));
            }
        }

        Ok(())
    }

    /// Convert an ImageReference to an oci-distribution Reference.
    fn to_oci_reference(&self, reference: &ImageReference) -> Result<Reference> {
        let ref_str = if let Some(ref digest) = reference.digest {
            format!("{}/{}@{}", reference.registry, reference.repository, digest)
        } else if let Some(ref tag) = reference.tag {
            format!("{}/{}:{}", reference.registry, reference.repository, tag)
        } else {
            format!("{}/{}:latest", reference.registry, reference.repository)
        };

        ref_str.parse::<Reference>().map_err(|e| {
            BoxError::OciImageError(format!("Invalid OCI reference '{}': {}", ref_str, e))
        })
    }
}

/// Result of a successful image push.
#[derive(Debug, Clone)]
pub struct PushResult {
    /// URL of the pushed config blob.
    pub config_url: String,
    /// URL of the pushed manifest.
    pub manifest_url: String,
    /// Digest of the pushed manifest (e.g., "sha256:abc123...").
    pub manifest_digest: String,
}

/// Pushes OCI images to container registries.
pub struct RegistryPusher {
    client: Client,
    auth: RegistryAuth,
}

impl Default for RegistryPusher {
    fn default() -> Self {
        Self::new()
    }
}

impl RegistryPusher {
    /// Create a new registry pusher with anonymous authentication.
    pub fn new() -> Self {
        Self::with_auth(RegistryAuth::anonymous())
    }

    /// Create a new registry pusher with the given authentication.
    pub fn with_auth(auth: RegistryAuth) -> Self {
        let config = ClientConfig {
            protocol: registry_protocol_from_env(),
            ..Default::default()
        };
        let client = Client::new(config);
        Self { client, auth }
    }

    /// Push a local OCI image layout to a registry.
    ///
    /// Reads the OCI layout from `image_dir` (index.json → manifest → config + layers),
    /// then pushes all blobs and the manifest to the target registry.
    pub async fn push(&self, reference: &ImageReference, image_dir: &Path) -> Result<PushResult> {
        let oci_ref = self.to_oci_reference(reference)?;

        tracing::info!(
            reference = %reference,
            source = %image_dir.display(),
            "Pushing image to registry"
        );

        // Read index.json to find the manifest digest
        let index_path = image_dir.join("index.json");
        let index_data = std::fs::read_to_string(&index_path)
            .map_err(|e| BoxError::OciImageError(format!("Failed to read index.json: {}", e)))?;
        let index: serde_json::Value = serde_json::from_str(&index_data)?;

        let manifest_digest = index["manifests"][0]["digest"].as_str().ok_or_else(|| {
            BoxError::OciImageError("No manifest digest in index.json".to_string())
        })?;

        // Read manifest blob
        let manifest_digest_hex = manifest_digest
            .strip_prefix("sha256:")
            .unwrap_or(manifest_digest);
        let blobs_dir = image_dir.join("blobs").join("sha256");
        let manifest_data = std::fs::read(blobs_dir.join(manifest_digest_hex))
            .map_err(|e| BoxError::OciImageError(format!("Failed to read manifest blob: {}", e)))?;
        let manifest: OciImageManifest = serde_json::from_slice(&manifest_data)?;

        // Read config blob
        let config_digest_hex = manifest
            .config
            .digest
            .strip_prefix("sha256:")
            .unwrap_or(&manifest.config.digest);
        let config_data = std::fs::read(blobs_dir.join(config_digest_hex))
            .map_err(|e| BoxError::OciImageError(format!("Failed to read config blob: {}", e)))?;
        let config = Config::new(config_data, manifest.config.media_type.clone(), None);

        // Read layer blobs
        let mut layers = Vec::new();
        for layer_desc in &manifest.layers {
            let layer_digest_hex = layer_desc
                .digest
                .strip_prefix("sha256:")
                .unwrap_or(&layer_desc.digest);
            let layer_data = std::fs::read(blobs_dir.join(layer_digest_hex)).map_err(|e| {
                BoxError::OciImageError(format!(
                    "Failed to read layer blob {}: {}",
                    layer_desc.digest, e
                ))
            })?;

            tracing::debug!(
                digest = %layer_desc.digest,
                size = layer_data.len(),
                "Read layer for push"
            );

            layers.push(ImageLayer::new(
                layer_data,
                layer_desc.media_type.clone(),
                None,
            ));
        }

        // Push to registry
        let auth = self.auth.to_oci_auth();
        let response: PushResponse = self
            .client
            .push(&oci_ref, &layers, config, &auth, Some(manifest))
            .await
            .map_err(|e| BoxError::RegistryError {
                registry: reference.registry.clone(),
                message: format!("Failed to push image: {}", e),
            })?;

        tracing::info!(
            reference = %reference,
            manifest_url = %response.manifest_url,
            "Image pushed successfully"
        );

        Ok(PushResult {
            config_url: response.config_url,
            manifest_url: response.manifest_url,
            manifest_digest: manifest_digest.to_string(),
        })
    }

    /// Convert an ImageReference to an oci-distribution Reference.
    fn to_oci_reference(&self, reference: &ImageReference) -> Result<Reference> {
        let ref_str = if let Some(ref tag) = reference.tag {
            format!("{}/{}:{}", reference.registry, reference.repository, tag)
        } else {
            format!("{}/{}:latest", reference.registry, reference.repository)
        };

        ref_str.parse::<Reference>().map_err(|e| {
            BoxError::OciImageError(format!("Invalid OCI reference '{}': {}", ref_str, e))
        })
    }
}

/// Platform resolver that always selects linux images matching the host architecture.
///
/// Container images run inside a Linux microVM regardless of the host OS,
/// so we always look for `os: "linux"` with the host's CPU architecture.
fn linux_platform_resolver(manifests: &[ImageIndexEntry]) -> Option<String> {
    let arch = match std::env::consts::ARCH {
        "x86_64" => "amd64",
        "aarch64" => "arm64",
        other => other,
    };

    manifests
        .iter()
        .find(|entry| {
            entry
                .platform
                .as_ref()
                .is_some_and(|p| p.os == "linux" && p.architecture == arch)
        })
        .map(|entry| entry.digest.clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Mutex, OnceLock};

    fn env_lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(())).lock().unwrap()
    }

    #[test]
    fn test_registry_auth_anonymous() {
        let auth = RegistryAuth::anonymous();
        assert!(auth.username.is_none());
        assert!(auth.password.is_none());
    }

    #[tokio::test]
    async fn test_hashing_file_writer_matches_sha256() {
        use sha2::{Digest, Sha256};
        use tokio::io::AsyncWriteExt;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("blob");
        let payload = b"a3s-box streaming blob hash test payload";

        let mut writer = HashingFileWriter::new(tokio::fs::File::create(&path).await.unwrap());
        // Write in two chunks to exercise incremental hashing.
        writer.write_all(&payload[..10]).await.unwrap();
        writer.write_all(&payload[10..]).await.unwrap();
        writer.flush().await.unwrap();
        writer.shutdown().await.unwrap();
        let streamed = writer.finalize_hex();

        let expected = format!("{:x}", Sha256::digest(payload));
        assert_eq!(
            streamed, expected,
            "streamed hash must equal sha256(payload)"
        );
        // The file on disk must contain exactly the written bytes.
        assert_eq!(std::fs::read(&path).unwrap(), payload);
    }

    #[test]
    fn test_registry_auth_basic() {
        let auth = RegistryAuth::basic("user", "pass");
        assert_eq!(auth.username, Some("user".to_string()));
        assert_eq!(auth.password, Some("pass".to_string()));
    }

    #[test]
    fn test_registry_auth_to_oci_anonymous() {
        let auth = RegistryAuth::anonymous();
        let oci_auth = auth.to_oci_auth();
        assert!(matches!(oci_auth, OciRegistryAuth::Anonymous));
    }

    #[test]
    fn test_registry_auth_to_oci_basic() {
        let auth = RegistryAuth::basic("user", "pass");
        let oci_auth = auth.to_oci_auth();
        assert!(matches!(oci_auth, OciRegistryAuth::Basic(_, _)));
    }

    #[test]
    fn test_registry_protocol_defaults_to_https() {
        let _guard = env_lock();
        std::env::remove_var(REGISTRY_PROTOCOL_ENV);
        assert!(matches!(
            registry_protocol_from_env(),
            ClientProtocol::Https
        ));
    }

    #[test]
    fn test_registry_protocol_can_use_http_for_local_testing() {
        let _guard = env_lock();
        std::env::set_var(REGISTRY_PROTOCOL_ENV, "http");
        assert!(matches!(registry_protocol_from_env(), ClientProtocol::Http));
        std::env::remove_var(REGISTRY_PROTOCOL_ENV);
    }

    #[test]
    fn test_registry_protocol_rejects_unknown_values_to_https() {
        let _guard = env_lock();
        std::env::set_var(REGISTRY_PROTOCOL_ENV, "ftp");
        assert!(matches!(
            registry_protocol_from_env(),
            ClientProtocol::Https
        ));
        std::env::remove_var(REGISTRY_PROTOCOL_ENV);
    }

    #[test]
    fn test_to_oci_reference_with_tag() {
        let puller = RegistryPuller::new();
        let img_ref = ImageReference {
            registry: "ghcr.io".to_string(),
            repository: "a3s-box/code".to_string(),
            tag: Some("v0.1.0".to_string()),
            digest: None,
        };
        let oci_ref = puller.to_oci_reference(&img_ref).unwrap();
        assert_eq!(oci_ref.to_string(), "ghcr.io/a3s-box/code:v0.1.0");
    }

    #[test]
    fn test_to_oci_reference_with_digest() {
        let puller = RegistryPuller::new();
        let img_ref = ImageReference {
            registry: "ghcr.io".to_string(),
            repository: "a3s-box/code".to_string(),
            tag: None,
            digest: Some(
                "sha256:abcdef1234567890abcdef1234567890abcdef1234567890abcdef1234567890"
                    .to_string(),
            ),
        };
        let oci_ref = puller.to_oci_reference(&img_ref).unwrap();
        let ref_str = oci_ref.to_string();
        assert!(ref_str.contains("sha256:"));
    }

    #[test]
    fn test_to_oci_reference_default_tag() {
        let puller = RegistryPuller::new();
        let img_ref = ImageReference {
            registry: "docker.io".to_string(),
            repository: "library/nginx".to_string(),
            tag: None,
            digest: None,
        };
        let oci_ref = puller.to_oci_reference(&img_ref).unwrap();
        assert!(oci_ref.to_string().contains("latest"));
    }
}
