//! High-level OCI image pull orchestrator.
//!
//! Combines the registry puller and image store to provide a cache-first
//! pull workflow. Images are checked in the local store first; if not found,
//! they are pulled from the registry and stored locally.

use std::sync::Arc;

use a3s_box_core::error::{BoxError, Result};

use super::image::OciImage;
use super::reference::ImageReference;
use super::registry::{RegistryAuth, RegistryPuller};
use super::store::ImageStore;

/// High-level image puller with caching.
pub struct ImagePuller {
    store: Arc<ImageStore>,
    puller: RegistryPuller,
    metrics: Option<crate::prom::RuntimeMetrics>,
}

impl ImagePuller {
    /// Create a new image puller.
    pub fn new(store: Arc<ImageStore>, auth: RegistryAuth) -> Self {
        Self {
            store,
            puller: RegistryPuller::with_auth(auth),
            metrics: None,
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

    /// Pull an image, using the local cache if available.
    ///
    /// Returns the loaded OCI image from the store.
    pub async fn pull(&self, reference: &str) -> Result<OciImage> {
        let parsed = ImageReference::parse(reference)?;
        let full_ref = parsed.full_reference();

        // Check cache first
        if let Some(stored) = self.store.get(&full_ref).await {
            tracing::info!(
                reference = %full_ref,
                digest = %stored.digest,
                "Using cached image"
            );
            return OciImage::from_path(&stored.path);
        }

        self.pull_and_store(&parsed).await
    }

    /// Pull an image, bypassing the local cache.
    pub async fn force_pull(&self, reference: &str) -> Result<OciImage> {
        let parsed = ImageReference::parse(reference)?;

        // Remove from cache if present
        let full_ref = parsed.full_reference();
        if self.store.get(&full_ref).await.is_some() {
            let _ = self.store.remove(&full_ref).await;
        }

        self.pull_and_store(&parsed).await
    }

    /// Check if an image is already cached.
    pub async fn is_cached(&self, reference: &str) -> bool {
        let parsed = match ImageReference::parse(reference) {
            Ok(p) => p,
            Err(_) => return false,
        };
        self.store.get(&parsed.full_reference()).await.is_some()
    }

    /// Remove a cached image by reference.
    pub async fn remove_cached(&self, reference: &str) -> Result<bool> {
        let parsed = ImageReference::parse(reference)?;
        let full_ref = parsed.full_reference();
        if self.store.get(&full_ref).await.is_some() {
            self.store.remove(&full_ref).await?;
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

        // Get the manifest digest for storage key
        let digest = self.puller.pull_manifest_digest(reference).await?;

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

        // Pull to a temporary directory first
        let tmp_dir = self.store.store_dir().join("tmp").join(&digest);
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
        self.puller.pull(reference, &tmp_dir).await?;
        if let Some(ref m) = self.metrics {
            m.image_pull_total.inc();
            m.image_pull_duration
                .observe(pull_start.elapsed().as_secs_f64());
        }

        // Store in the image store
        let stored = self.store.put(&full_ref, &digest, &tmp_dir).await?;

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
}

#[async_trait::async_trait]
impl a3s_box_core::traits::ImageRegistry for ImagePuller {
    async fn pull(&self, reference: &str) -> Result<a3s_box_core::traits::PulledImage> {
        let image = self.pull(reference).await?;
        let parsed = ImageReference::parse(reference)?;
        Ok(a3s_box_core::traits::PulledImage {
            path: image.root_dir().to_path_buf(),
            digest: String::new(), // digest not exposed by OciImage
            reference: parsed.full_reference(),
        })
    }

    async fn force_pull(&self, reference: &str) -> Result<a3s_box_core::traits::PulledImage> {
        let image = self.force_pull(reference).await?;
        let parsed = ImageReference::parse(reference)?;
        Ok(a3s_box_core::traits::PulledImage {
            path: image.root_dir().to_path_buf(),
            digest: String::new(),
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

    #[test]
    fn test_set_metrics_attaches_to_puller() {
        let tmp = TempDir::new().unwrap();
        let store = Arc::new(ImageStore::new(tmp.path(), 10 * 1024 * 1024).unwrap());
        let metrics = crate::prom::RuntimeMetrics::new();
        // Verify set_metrics() returns Self (builder pattern) and metrics start at zero
        let puller = ImagePuller::new(store, RegistryAuth::anonymous()).set_metrics(metrics.clone());
        assert!(puller.metrics.is_some());
        assert_eq!(metrics.image_pull_total.get(), 0);
        assert_eq!(metrics.image_pull_duration.get_sample_count(), 0);
    }
}
