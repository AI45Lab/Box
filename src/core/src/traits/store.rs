//! Persistent store abstractions for networks, volumes, snapshots, and images.
//!
//! Decouples the runtime from the JSON-file-based storage in
//! `a3s-box-runtime`. Implementations can use any backend:
//! JSON files, etcd, consul, SQLite, etc.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

use crate::error::Result;
use crate::network::NetworkConfig;
use crate::snapshot::SnapshotMetadata;
use crate::volume::VolumeConfig;

/// Metadata for a stored OCI image.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredImage {
    /// OCI image reference (e.g., `docker.io/library/ubuntu:22.04`)
    pub reference: String,
    /// Content-addressable digest
    pub digest: String,
    /// Size on disk in bytes
    pub size_bytes: u64,
    /// When the image was first pulled
    pub pulled_at: DateTime<Utc>,
    /// When the image was last used (for LRU eviction)
    pub last_used: DateTime<Utc>,
    /// Path to the unpacked OCI image layout on disk
    pub path: PathBuf,
}

/// Abstraction over persistent network configuration storage.
///
/// The runtime and CLI use this to create, query, and manage
/// virtual networks that connect Box instances.
pub trait NetworkStoreBackend: Send + Sync {
    /// Get a network by name.
    fn get(&self, name: &str) -> Result<Option<NetworkConfig>>;

    /// Create a new network. Returns error if name already exists.
    fn create(&self, config: NetworkConfig) -> Result<()>;

    /// Remove a network by name. Returns the removed config.
    fn remove(&self, name: &str) -> Result<NetworkConfig>;

    /// List all networks.
    fn list(&self) -> Result<Vec<NetworkConfig>>;

    /// Update a network in-place (e.g., after connect/disconnect).
    fn update(&self, config: &NetworkConfig) -> Result<()>;
}

/// Abstraction over persistent volume configuration storage.
///
/// The runtime and CLI use this to create, query, and manage
/// named volumes that persist data across Box instances.
pub trait VolumeStoreBackend: Send + Sync {
    /// Get a volume by name.
    fn get(&self, name: &str) -> Result<Option<VolumeConfig>>;

    /// Create a new named volume. Returns the created config with mount point set.
    fn create(&self, config: VolumeConfig) -> Result<VolumeConfig>;

    /// Remove a volume by name. If `force` is false, returns error if in use.
    fn remove(&self, name: &str, force: bool) -> Result<VolumeConfig>;

    /// List all volumes.
    fn list(&self) -> Result<Vec<VolumeConfig>>;

    /// Update a volume in-place (e.g., after attach/detach).
    fn update(&self, config: &VolumeConfig) -> Result<()>;

    /// Remove all unused volumes. Returns names of removed volumes.
    fn prune(&self) -> Result<Vec<String>>;
}

/// Abstraction over VM snapshot storage.
///
/// Snapshots capture full VM configuration so a Box can be reconstructed.
/// Implementations can store snapshots locally, on NFS, S3, etc.
pub trait SnapshotStoreBackend: Send + Sync {
    /// Save a snapshot with its rootfs source directory.
    ///
    /// `metadata` carries the snapshot config; `rootfs_source` is the
    /// directory to copy into the snapshot bundle. Returns updated metadata
    /// with `size_bytes` populated.
    fn save(&self, metadata: SnapshotMetadata, rootfs_source: &Path) -> Result<SnapshotMetadata>;

    /// Load snapshot metadata by ID. Returns `None` if not found.
    fn get(&self, id: &str) -> Result<Option<SnapshotMetadata>>;

    /// List all snapshots, sorted by creation time (newest first).
    fn list(&self) -> Result<Vec<SnapshotMetadata>>;

    /// Delete a snapshot by ID. Returns `true` if it existed.
    fn delete(&self, id: &str) -> Result<bool>;

    /// Total number of stored snapshots.
    fn count(&self) -> Result<usize>;

    /// Total disk usage in bytes across all snapshots.
    fn total_size(&self) -> Result<u64>;

    /// Evict old snapshots until under `max_count` and `max_bytes`.
    ///
    /// Returns the IDs of deleted snapshots.
    fn prune(&self, max_count: usize, max_bytes: u64) -> Result<Vec<String>>;
}

/// Abstraction over OCI image storage.
///
/// Manages the local cache of pulled OCI images with LRU eviction.
/// Implementations can use local disk, remote object storage, etc.
#[async_trait::async_trait]
pub trait ImageStoreBackend: Send + Sync {
    /// Look up a stored image by reference. Returns `None` if not cached.
    async fn get(&self, reference: &str) -> Option<StoredImage>;

    /// Look up a stored image by content digest.
    async fn get_by_digest(&self, digest: &str) -> Option<StoredImage>;

    /// Store an image layout directory under the given reference and digest.
    async fn put(&self, reference: &str, digest: &str, source_dir: &Path) -> Result<StoredImage>;

    /// Remove an image by reference.
    async fn remove(&self, reference: &str) -> Result<()>;

    /// List all stored images.
    async fn list(&self) -> Vec<StoredImage>;

    /// Evict least-recently-used images to stay within the size cap.
    ///
    /// Returns the references of evicted images.
    async fn evict(&self) -> Result<Vec<String>>;

    /// Total disk usage across all stored images.
    async fn total_size(&self) -> u64;
}
