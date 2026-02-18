//! Image registry abstraction.
//!
//! Decouples the VM boot path from any specific OCI registry client.
//! The default implementation in `a3s-box-runtime` uses `oci-distribution`,
//! but this trait allows swapping in a local-only store, a P2P distribution
//! layer, or a custom registry protocol.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::error::Result;

/// Metadata about a successfully pulled image.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PulledImage {
    /// Path to the OCI image layout directory on disk.
    ///
    /// The directory follows the OCI Image Layout Specification:
    /// `oci-layout`, `index.json`, `blobs/sha256/...`
    pub path: PathBuf,

    /// Manifest digest (e.g., `"sha256:abc123..."`).
    pub digest: String,

    /// The fully-qualified reference that was resolved
    /// (e.g., `"docker.io/library/nginx:latest"`).
    pub reference: String,
}

/// Abstraction over container image registries.
///
/// Implementations handle authentication, transport, and local caching
/// of OCI image layouts. The runtime calls `pull` during VM boot to
/// obtain the image layers needed to build a guest rootfs.
///
/// # Example (conceptual)
///
/// ```ignore
/// let registry: Box<dyn ImageRegistry> = make_registry();
/// let image = registry.pull("nginx:latest").await?;
/// // image.path now points to a valid OCI layout directory
/// ```
#[async_trait::async_trait]
pub trait ImageRegistry: Send + Sync {
    /// Pull an image by reference, using local cache when available.
    ///
    /// The `reference` follows Docker conventions:
    /// - `"nginx"` → `docker.io/library/nginx:latest`
    /// - `"ghcr.io/org/repo:v1"`
    /// - `"registry.example.com/image@sha256:..."`
    ///
    /// Returns the path to a directory containing a valid OCI image layout.
    async fn pull(&self, reference: &str) -> Result<PulledImage>;

    /// Pull an image, bypassing any local cache.
    async fn force_pull(&self, reference: &str) -> Result<PulledImage>;

    /// Check whether an image is already available locally.
    async fn is_cached(&self, reference: &str) -> bool;

    /// Remove a locally cached image by reference.
    ///
    /// Returns `true` if the image was found and removed.
    async fn remove(&self, reference: &str) -> Result<bool>;

    /// List all locally cached image references.
    async fn list_cached(&self) -> Result<Vec<String>>;
}
