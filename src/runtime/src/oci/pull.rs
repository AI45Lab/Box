//! High-level OCI image pull orchestrator.
//!
//! Combines the registry puller and image store to provide a cache-first
//! pull workflow. Images are checked in the local store first; if not found,
//! they are pulled from the registry and stored locally.

use std::sync::Arc;

use a3s_box_core::error::{BoxError, Result};
use a3s_box_core::StoredImage;

use super::image::OciImage;
use super::reference::ImageReference;
use super::registry::{RegistryAuth, RegistryPuller};
use super::store::ImageStore;

/// Callback type for layer pull progress: `(current, total, digest, size_bytes)`.
type PullProgressFn = Arc<dyn Fn(usize, usize, &str, i64) + Send + Sync>;

/// High-level image puller with caching.
pub struct ImagePuller {
    store: Arc<ImageStore>,
    puller: RegistryPuller,
    metrics: Option<crate::prom::RuntimeMetrics>,
    /// Registry mirror map (original host → mirror host). Pulls fetch layers and
    /// manifests from the mirror while the image keeps its canonical reference
    /// for storage and identity. Parsed from `A3S_REGISTRY_MIRRORS`
    /// (`host=mirror,host=mirror`) — lets a3s-box pull in registry-restricted
    /// environments, like containerd's registry mirrors.
    mirrors: std::collections::HashMap<String, String>,
}

/// Parse `A3S_REGISTRY_MIRRORS=host=mirror,host=mirror` into a map.
fn parse_registry_mirrors() -> std::collections::HashMap<String, String> {
    std::env::var("A3S_REGISTRY_MIRRORS")
        .ok()
        .map(|raw| {
            raw.split(',')
                .filter_map(|pair| pair.split_once('='))
                .map(|(host, mirror)| (host.trim().to_string(), mirror.trim().to_string()))
                .filter(|(host, mirror)| !host.is_empty() && !mirror.is_empty())
                .collect()
        })
        .unwrap_or_default()
}

impl ImagePuller {
    /// Create a new image puller.
    pub fn new(store: Arc<ImageStore>, auth: RegistryAuth) -> Self {
        Self::with_platform(store, auth, None)
    }

    /// Create an image puller that resolves multi-arch images to an explicit
    /// platform (e.g. "linux/arm64") instead of the host architecture. `None`
    /// keeps the host-architecture default.
    pub fn with_platform(
        store: Arc<ImageStore>,
        auth: RegistryAuth,
        platform: Option<String>,
    ) -> Self {
        Self {
            store,
            puller: RegistryPuller::with_auth_and_platform(auth, platform),
            metrics: None,
            mirrors: parse_registry_mirrors(),
        }
    }

    /// Resolve a reference to its fetch target, applying a configured registry
    /// mirror. The returned reference is used only for the registry HTTP
    /// fetch — storage and identity always use the original canonical reference.
    fn mirror_reference(&self, reference: &ImageReference) -> ImageReference {
        match self.mirrors.get(&reference.registry) {
            Some(mirror) if !mirror.is_empty() => {
                tracing::info!(
                    registry = %reference.registry,
                    mirror = %mirror,
                    "Pulling via configured registry mirror"
                );
                let mut mirrored = reference.clone();
                mirror.clone_into(&mut mirrored.registry);
                mirrored
            }
            _ => reference.clone(),
        }
    }

    /// Attach Prometheus metrics to this puller.
    pub fn set_metrics(mut self, metrics: crate::prom::RuntimeMetrics) -> Self {
        self.metrics = Some(metrics);
        self
    }

    /// Set the signature verification policy for image pulls.
    pub fn with_signature_policy(mut self, policy: super::signing::SignaturePolicy) -> Self {
        self.puller = self.puller.with_signature_policy(policy);
        self
    }

    /// Set a layer progress callback: `(current, total, digest, size_bytes)`.
    pub fn with_progress_fn(mut self, f: PullProgressFn) -> Self {
        self.puller = self.puller.with_progress_fn(f);
        self
    }

    /// Pull an image, using the local cache if available.
    ///
    /// Returns the loaded OCI image from the store.
    pub async fn pull(&self, reference: &str) -> Result<OciImage> {
        self.pull_resolved(reference).await.map(|(image, _)| image)
    }

    async fn pull_resolved(&self, reference: &str) -> Result<(OciImage, String)> {
        let reference = reference.trim();
        if is_digest_reference(reference) {
            let Some((matched_reference, stored)) = self.cached_digest_image(reference).await?
            else {
                return Err(BoxError::OciImageError(format!(
                    "Image digest not found in local cache: {reference}"
                )));
            };
            tracing::info!(
                requested_reference = %reference,
                matched_reference = %matched_reference,
                digest = %stored.digest,
                "Using cached image by digest"
            );
            return Ok((OciImage::from_path(&stored.path)?, matched_reference));
        }

        let parsed = ImageReference::parse(reference)?;

        if let Some((matched_reference, stored)) = self.cached_image(reference, &parsed).await? {
            tracing::info!(
                requested_reference = %reference,
                matched_reference = %matched_reference,
                digest = %stored.digest,
                "Using cached image"
            );
            return Ok((OciImage::from_path(&stored.path)?, matched_reference));
        }

        Ok((self.pull_and_store(&parsed).await?, parsed.full_reference()))
    }

    /// Pull an image, bypassing the local cache.
    pub async fn force_pull(&self, reference: &str) -> Result<OciImage> {
        let reference = reference.trim();
        if is_digest_reference(reference) {
            return Err(BoxError::OciImageError(format!(
                "Cannot force-pull digest-only reference {reference}; use a tagged registry reference"
            )));
        }

        let parsed = ImageReference::parse(reference)?;

        for candidate in cache_reference_candidates(reference, &parsed) {
            if self.store.get(&candidate).await.is_some() {
                let _ = self.store.remove(&candidate).await;
            }
        }

        self.pull_and_store(&parsed).await
    }

    /// Check if an image is already cached.
    pub async fn is_cached(&self, reference: &str) -> bool {
        let reference = reference.trim();
        if is_digest_reference(reference) {
            return matches!(self.cached_digest_image(reference).await, Ok(Some(_)));
        }

        let parsed = match ImageReference::parse(reference) {
            Ok(p) => p,
            Err(_) => return false,
        };
        matches!(self.cached_image(reference, &parsed).await, Ok(Some(_)))
    }

    /// Remove a cached image by reference.
    pub async fn remove_cached(&self, reference: &str) -> Result<bool> {
        let reference = reference.trim();
        if is_digest_reference(reference) {
            if let Some((matched_reference, _)) = self.cached_digest_image(reference).await? {
                self.store.remove(&matched_reference).await?;
                return Ok(true);
            }
            return Ok(false);
        }

        let parsed = ImageReference::parse(reference)?;
        if let Some((matched_reference, _)) = self.cached_image(reference, &parsed).await? {
            self.store.remove(&matched_reference).await?;
            Ok(true)
        } else {
            Ok(false)
        }
    }

    /// List all cached image references.
    pub async fn list_cached(&self) -> Result<Vec<String>> {
        Ok(self
            .store
            .list()
            .await
            .into_iter()
            .map(|img| img.reference)
            .collect())
    }

    /// Pull from registry and store locally.
    async fn pull_and_store(&self, reference: &ImageReference) -> Result<OciImage> {
        let full_ref = reference.full_reference();

        // Fetch from a configured registry mirror when one applies; the image
        // keeps its canonical reference (full_ref) for storage and identity.
        let fetch = self.mirror_reference(reference);

        // Get the manifest digest for storage key
        let digest = self.puller.pull_manifest_digest(&fetch).await?;

        // Check if we already have this digest (different tag, same content)
        if let Some(stored) = self.store.get_by_digest(&digest).await {
            tracing::info!(
                reference = %full_ref,
                digest = %digest,
                "Image content already cached under different reference"
            );
            // Store under the new reference too
            self.store.put(&full_ref, &digest, &stored.path).await?;
            return OciImage::from_path(&stored.path);
        }

        // Pull to a temporary directory first.
        // Strip the "sha256:" prefix so the directory name is pure hex,
        // which is valid on all platforms (Windows forbids ':' in filenames).
        let digest_hex = digest.strip_prefix("sha256:").unwrap_or(&digest);
        let tmp_dir = self.store.store_dir().join("tmp").join(digest_hex);
        if tmp_dir.exists() {
            std::fs::remove_dir_all(&tmp_dir).map_err(|e| {
                BoxError::OciImageError(format!(
                    "Failed to clean temp directory {}: {}",
                    tmp_dir.display(),
                    e
                ))
            })?;
        }

        let pull_start = std::time::Instant::now();
        // Remove the partially-written temp directory on ANY failure so aborted
        // pulls (network error, signature failure, disk error) don't accumulate
        // under store_dir/tmp — each can be hundreds of MB and is never counted
        // toward the cache size or evicted by the LRU.
        if let Err(e) = self.puller.pull(&fetch, &tmp_dir).await {
            let _ = std::fs::remove_dir_all(&tmp_dir);
            return Err(e);
        }
        if let Some(ref m) = self.metrics {
            m.image_pull_total.inc();
            m.image_pull_duration
                .observe(pull_start.elapsed().as_secs_f64());
        }

        // Store in the image store
        let stored = match self.store.put(&full_ref, &digest, &tmp_dir).await {
            Ok(stored) => stored,
            Err(e) => {
                let _ = std::fs::remove_dir_all(&tmp_dir);
                return Err(e);
            }
        };

        // Clean up temp directory
        if let Err(e) = std::fs::remove_dir_all(&tmp_dir) {
            tracing::warn!(path = %tmp_dir.display(), error = %e, "Failed to remove temp dir after pull");
        }

        // Evict old images if over capacity
        let evicted = self.store.evict().await?;
        if !evicted.is_empty() {
            tracing::info!(
                count = evicted.len(),
                references = ?evicted,
                "Evicted images from cache"
            );
        }

        OciImage::from_path(&stored.path)
    }

    async fn cached_image(
        &self,
        reference: &str,
        parsed: &ImageReference,
    ) -> Result<Option<(String, StoredImage)>> {
        for candidate in cache_reference_candidates(reference, parsed) {
            if let Some(stored) = self.store.get(&candidate).await {
                return Ok(Some((candidate, stored)));
            }
        }
        if let Some(digest) = parsed.digest.as_deref() {
            return self.cached_digest_image(digest).await;
        }
        Ok(None)
    }

    async fn cached_digest_image(&self, digest: &str) -> Result<Option<(String, StoredImage)>> {
        let images = self.store.list().await;
        let mut matches = Vec::new();

        for image in images {
            let already_matched = matches
                .iter()
                .any(|matched: &StoredImage| matched.reference == image.reference);
            if (digest_matches(&image.digest, digest) || image.reference == digest)
                && !already_matched
            {
                matches.push(image);
            }
        }

        match matches.len() {
            0 => Ok(None),
            1 => {
                let stored = matches.pop().expect("checked one match");
                let stored = self.store.get(&stored.reference).await.unwrap_or(stored);
                Ok(Some((stored.reference.clone(), stored)))
            }
            _ => Err(BoxError::OciImageError(ambiguous_digest_error(
                digest, &matches,
            ))),
        }
    }
}

fn ambiguous_digest_error(query: &str, matches: &[StoredImage]) -> String {
    let mut references: Vec<_> = matches
        .iter()
        .map(|image| image.reference.as_str())
        .collect();
    references.sort_unstable();
    format!(
        "Image digest '{query}' is ambiguous; it matches: {}",
        references.join(", ")
    )
}

fn is_digest_reference(reference: &str) -> bool {
    reference.starts_with("sha256:")
}

fn digest_matches(stored_digest: &str, query: &str) -> bool {
    if stored_digest == query {
        return true;
    }
    let Some(query_hex) = query.strip_prefix("sha256:") else {
        return false;
    };
    !query_hex.is_empty() && stored_digest.starts_with(query)
}

fn cache_reference_candidates(reference: &str, parsed: &ImageReference) -> Vec<String> {
    let mut candidates = Vec::new();
    push_unique(&mut candidates, reference.trim().to_string());
    push_unique(&mut candidates, parsed.full_reference());

    if parsed.registry == "docker.io" {
        let repository = parsed
            .repository
            .strip_prefix("library/")
            .unwrap_or(&parsed.repository);
        push_unique(
            &mut candidates,
            reference_from_repository(repository, parsed.tag.as_deref(), parsed.digest.as_deref()),
        );

        if parsed.digest.is_none() && parsed.tag.as_deref() == Some("latest") {
            push_unique(&mut candidates, repository.to_string());
        }
    }

    if let Some(digest) = &parsed.digest {
        push_unique(&mut candidates, digest.clone());
    }

    candidates
}

fn reference_from_repository(repository: &str, tag: Option<&str>, digest: Option<&str>) -> String {
    let mut reference = repository.to_string();
    if let Some(tag) = tag {
        reference.push(':');
        reference.push_str(tag);
    }
    if let Some(digest) = digest {
        reference.push('@');
        reference.push_str(digest);
    }
    reference
}

fn push_unique(values: &mut Vec<String>, value: String) {
    if !value.is_empty() && !values.iter().any(|existing| existing == &value) {
        values.push(value);
    }
}

#[async_trait::async_trait]
impl a3s_box_core::traits::ImageRegistry for ImagePuller {
    async fn pull(&self, reference: &str) -> Result<a3s_box_core::traits::PulledImage> {
        let (image, resolved_reference) = self.pull_resolved(reference).await?;
        Ok(a3s_box_core::traits::PulledImage {
            path: image.root_dir().to_path_buf(),
            digest: image.manifest_digest().to_string(),
            reference: resolved_reference,
        })
    }

    async fn force_pull(&self, reference: &str) -> Result<a3s_box_core::traits::PulledImage> {
        let image = self.force_pull(reference).await?;
        let parsed = ImageReference::parse(reference)?;
        Ok(a3s_box_core::traits::PulledImage {
            path: image.root_dir().to_path_buf(),
            digest: image.manifest_digest().to_string(),
            reference: parsed.full_reference(),
        })
    }

    async fn is_cached(&self, reference: &str) -> bool {
        self.is_cached(reference).await
    }

    async fn remove(&self, reference: &str) -> Result<bool> {
        self.remove_cached(reference).await
    }

    async fn list_cached(&self) -> Result<Vec<String>> {
        self.list_cached().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::oci::store::ImageStore;
    use std::path::Path;
    use tempfile::TempDir;

    #[test]
    fn test_image_puller_creation() {
        let tmp = TempDir::new().unwrap();
        let store = Arc::new(ImageStore::new(tmp.path(), 10 * 1024 * 1024).unwrap());
        let _puller = ImagePuller::new(store, RegistryAuth::anonymous());
    }

    #[tokio::test]
    async fn test_is_cached_empty_store() {
        let tmp = TempDir::new().unwrap();
        let store = Arc::new(ImageStore::new(tmp.path(), 10 * 1024 * 1024).unwrap());
        let puller = ImagePuller::new(store, RegistryAuth::anonymous());
        assert!(!puller.is_cached("nginx:latest").await);
    }

    #[tokio::test]
    async fn test_is_cached_invalid_reference() {
        let tmp = TempDir::new().unwrap();
        let store = Arc::new(ImageStore::new(tmp.path(), 10 * 1024 * 1024).unwrap());
        let puller = ImagePuller::new(store, RegistryAuth::anonymous());
        assert!(!puller.is_cached("").await);
    }

    #[tokio::test]
    async fn test_is_cached_matches_docker_hub_aliases() {
        let tmp = TempDir::new().unwrap();
        let source = TempDir::new().unwrap();
        create_complete_oci_image(source.path());
        let store = Arc::new(ImageStore::new(tmp.path(), 10 * 1024 * 1024).unwrap());
        store
            .put("alpine:latest", "sha256:manifestxyz789", source.path())
            .await
            .unwrap();

        let puller = ImagePuller::new(store, RegistryAuth::anonymous());

        assert!(puller.is_cached("alpine:latest").await);
        assert!(puller.is_cached("docker.io/library/alpine:latest").await);
        assert!(puller.is_cached("alpine").await);
    }

    #[tokio::test]
    async fn test_pull_uses_cached_short_alias_for_full_reference() {
        let tmp = TempDir::new().unwrap();
        let source = TempDir::new().unwrap();
        create_complete_oci_image(source.path());
        let store = Arc::new(ImageStore::new(tmp.path(), 10 * 1024 * 1024).unwrap());
        store
            .put("alpine:latest", "sha256:manifestxyz789", source.path())
            .await
            .unwrap();
        let puller = ImagePuller::new(store, RegistryAuth::anonymous());

        let image = puller
            .pull("docker.io/library/alpine:latest")
            .await
            .unwrap();

        assert_eq!(image.manifest_digest(), "sha256:manifestxyz789");
    }

    #[tokio::test]
    async fn test_pull_uses_cached_digest_reference_without_registry_parse() {
        let tmp = TempDir::new().unwrap();
        let source = TempDir::new().unwrap();
        create_complete_oci_image(source.path());
        let store = Arc::new(ImageStore::new(tmp.path(), 10 * 1024 * 1024).unwrap());
        store
            .put("alpine:latest", "sha256:manifestxyz789", source.path())
            .await
            .unwrap();
        let puller = ImagePuller::new(store, RegistryAuth::anonymous());

        let image = puller.pull("sha256:manifestxyz789").await.unwrap();

        assert_eq!(image.manifest_digest(), "sha256:manifestxyz789");
    }

    #[tokio::test]
    async fn test_is_cached_matches_digest_prefix() {
        let tmp = TempDir::new().unwrap();
        let source = TempDir::new().unwrap();
        create_complete_oci_image(source.path());
        let store = Arc::new(ImageStore::new(tmp.path(), 10 * 1024 * 1024).unwrap());
        store
            .put("alpine:latest", "sha256:manifestxyz789", source.path())
            .await
            .unwrap();
        let puller = ImagePuller::new(store, RegistryAuth::anonymous());

        assert!(puller.is_cached("sha256:manifest").await);
    }

    #[tokio::test]
    async fn test_pull_reports_missing_digest_reference_as_local_cache_miss() {
        let tmp = TempDir::new().unwrap();
        let store = Arc::new(ImageStore::new(tmp.path(), 10 * 1024 * 1024).unwrap());
        let puller = ImagePuller::new(store, RegistryAuth::anonymous());

        let error = puller.pull("sha256:notfound").await.unwrap_err();

        assert!(error.to_string().contains("not found in local cache"));
    }

    #[tokio::test]
    async fn test_pull_reports_ambiguous_digest_prefix() {
        let tmp = TempDir::new().unwrap();
        let source = TempDir::new().unwrap();
        create_complete_oci_image(source.path());
        let store = Arc::new(ImageStore::new(tmp.path(), 10 * 1024 * 1024).unwrap());
        store
            .put("alpine:latest", "sha256:manifestaaa", source.path())
            .await
            .unwrap();
        store
            .put("busybox:latest", "sha256:manifestbbb", source.path())
            .await
            .unwrap();
        let puller = ImagePuller::new(store, RegistryAuth::anonymous());

        let error = puller.pull("sha256:manifest").await.unwrap_err();

        assert!(error.to_string().contains("ambiguous"));
        assert!(error.to_string().contains("alpine:latest"));
        assert!(error.to_string().contains("busybox:latest"));
    }

    #[tokio::test]
    async fn test_remove_cached_matches_docker_hub_alias() {
        let tmp = TempDir::new().unwrap();
        let source = TempDir::new().unwrap();
        create_complete_oci_image(source.path());
        let store = Arc::new(ImageStore::new(tmp.path(), 10 * 1024 * 1024).unwrap());
        store
            .put("alpine:latest", "sha256:manifestxyz789", source.path())
            .await
            .unwrap();
        let puller = ImagePuller::new(store.clone(), RegistryAuth::anonymous());

        assert!(puller
            .remove_cached("docker.io/library/alpine:latest")
            .await
            .unwrap());
        assert!(store.get("alpine:latest").await.is_none());
    }

    #[test]
    fn test_set_metrics_attaches_to_puller() {
        let tmp = TempDir::new().unwrap();
        let store = Arc::new(ImageStore::new(tmp.path(), 10 * 1024 * 1024).unwrap());
        let metrics = crate::prom::RuntimeMetrics::new();
        // Verify set_metrics() returns Self (builder pattern) and metrics start at zero
        let puller =
            ImagePuller::new(store, RegistryAuth::anonymous()).set_metrics(metrics.clone());
        assert!(puller.metrics.is_some());
        assert_eq!(metrics.image_pull_total.get(), 0);
        assert_eq!(metrics.image_pull_duration.get_sample_count(), 0);
    }

    #[test]
    fn test_cache_reference_candidates_include_short_docker_hub_aliases() {
        let parsed = ImageReference::parse("docker.io/library/alpine:latest").unwrap();
        let candidates = cache_reference_candidates("docker.io/library/alpine:latest", &parsed);

        assert_eq!(
            candidates,
            vec![
                "docker.io/library/alpine:latest".to_string(),
                "alpine:latest".to_string(),
                "alpine".to_string(),
            ]
        );
    }

    #[test]
    fn test_digest_matches_exact_and_prefix_queries() {
        assert!(digest_matches("sha256:abcdef123456", "sha256:abcdef123456"));
        assert!(digest_matches("sha256:abcdef123456", "sha256:abcdef"));
        assert!(!digest_matches("sha256:abcdef123456", "sha256:"));
        assert!(!digest_matches("sha256:abcdef123456", "abcdef"));
    }

    fn create_complete_oci_image(path: &Path) {
        std::fs::create_dir_all(path.join("blobs/sha256")).unwrap();
        std::fs::write(path.join("oci-layout"), r#"{"imageLayoutVersion":"1.0.0"}"#).unwrap();

        let config_content = r#"{
            "architecture": "amd64",
            "os": "linux",
            "config": {
                "Entrypoint": ["/bin/sh"],
                "Cmd": ["-c", "true"],
                "Env": ["PATH=/usr/bin:/bin"],
                "WorkingDir": "/"
            },
            "rootfs": {
                "type": "layers",
                "diff_ids": ["sha256:layerdiff"]
            },
            "history": []
        }"#;
        let config_hash = "configabc123";
        std::fs::write(path.join("blobs/sha256").join(config_hash), config_content).unwrap();

        let layer_hash = "layerdef456";
        std::fs::write(path.join("blobs/sha256").join(layer_hash), b"layer").unwrap();

        let manifest_content = format!(
            r#"{{
            "schemaVersion": 2,
            "mediaType": "application/vnd.oci.image.manifest.v1+json",
            "config": {{
                "mediaType": "application/vnd.oci.image.config.v1+json",
                "digest": "sha256:{}",
                "size": {}
            }},
            "layers": [
                {{
                    "mediaType": "application/vnd.oci.image.layer.v1.tar+gzip",
                    "digest": "sha256:{}",
                    "size": 5
                }}
            ]
        }}"#,
            config_hash,
            config_content.len(),
            layer_hash
        );
        let manifest_hash = "manifestxyz789";
        std::fs::write(
            path.join("blobs/sha256").join(manifest_hash),
            &manifest_content,
        )
        .unwrap();

        let index_content = format!(
            r#"{{
            "schemaVersion": 2,
            "mediaType": "application/vnd.oci.image.index.v1+json",
            "manifests": [
                {{
                    "mediaType": "application/vnd.oci.image.manifest.v1+json",
                    "digest": "sha256:{}",
                    "size": {}
                }}
            ]
        }}"#,
            manifest_hash,
            manifest_content.len()
        );
        std::fs::write(path.join("index.json"), index_content).unwrap();
    }
}
