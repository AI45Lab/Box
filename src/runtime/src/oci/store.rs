//! Disk-based OCI image store with LRU eviction.
//!
//! Stores pulled OCI images on disk with an in-memory index backed by
//! a persistent `index.json` file. Supports LRU eviction when the store
//! exceeds a configured maximum size.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use a3s_box_core::error::{BoxError, Result};
use a3s_box_core::{ImageStoreBackend, StoredImage};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;

/// Persistent index stored as JSON on disk.
#[derive(Debug, Default, Serialize, Deserialize)]
struct StoreIndex {
    images: Vec<StoredImage>,
}

/// Disk-based image store with in-memory index and LRU eviction.
pub struct ImageStore {
    /// Root directory for image storage
    store_dir: PathBuf,
    /// In-memory index: reference → StoredImage
    index: Arc<RwLock<HashMap<String, StoredImage>>>,
    /// Maximum total size in bytes
    max_size_bytes: u64,
}

impl ImageStore {
    /// Create a new image store.
    ///
    /// Creates the store directory if it doesn't exist and loads
    /// any existing index from disk.
    pub fn new(store_dir: &Path, max_size_bytes: u64) -> Result<Self> {
        std::fs::create_dir_all(store_dir).map_err(|e| {
            BoxError::OciImageError(format!(
                "Failed to create image store directory {}: {}",
                store_dir.display(),
                e
            ))
        })?;

        let mut store = Self {
            store_dir: store_dir.to_path_buf(),
            index: Arc::new(RwLock::new(HashMap::new())),
            max_size_bytes,
        };

        store.load_index()?;
        Ok(store)
    }

    /// Get a stored image by reference.
    pub async fn get(&self, reference: &str) -> Option<StoredImage> {
        let mut index = self.index.write().await;
        if let Some(image) = index.get_mut(reference) {
            image.last_used = Utc::now();
            let updated = image.clone();
            drop(index);
            // Best-effort save of updated last_used
            let _ = self.save_index_inner().await;
            Some(updated)
        } else {
            None
        }
    }

    /// Get a stored image by digest.
    pub async fn get_by_digest(&self, digest: &str) -> Option<StoredImage> {
        let mut index = self.index.write().await;
        let found = index.values_mut().find(|img| img.digest == digest);
        if let Some(image) = found {
            image.last_used = Utc::now();
            let updated = image.clone();
            drop(index);
            let _ = self.save_index_inner().await;
            Some(updated)
        } else {
            None
        }
    }

    /// Store an image from a source directory.
    ///
    /// Copies the OCI image layout from `source_dir` into the store
    /// under `sha256/<digest>/`.
    pub async fn put(
        &self,
        reference: &str,
        digest: &str,
        source_dir: &Path,
    ) -> Result<StoredImage> {
        // Compute target path from digest
        let digest_hex = digest.strip_prefix("sha256:").unwrap_or(digest);
        let target_dir = self.store_dir.join("sha256").join(digest_hex);

        // Copy source to target if not already present
        if !target_dir.exists() {
            copy_dir_recursive(source_dir, &target_dir).map_err(|e| {
                BoxError::OciImageError(format!("Failed to copy image to store: {}", e))
            })?;
        }

        let size_bytes = dir_size(&target_dir);
        let now = Utc::now();

        let stored = StoredImage {
            reference: reference.to_string(),
            digest: digest.to_string(),
            size_bytes,
            pulled_at: now,
            last_used: now,
            path: target_dir,
        };

        let mut index = self.index.write().await;
        index.insert(reference.to_string(), stored.clone());
        drop(index);

        self.save_index_inner().await?;

        Ok(stored)
    }

    /// Remove an image by reference.
    pub async fn remove(&self, reference: &str) -> Result<()> {
        let mut index = self.index.write().await;
        if let Some(image) = index.remove(reference) {
            // Check if any other reference points to the same digest
            let digest_still_used = index.values().any(|img| img.digest == image.digest);
            drop(index);

            if !digest_still_used && image.path.exists() {
                std::fs::remove_dir_all(&image.path).map_err(|e| {
                    BoxError::OciImageError(format!(
                        "Failed to remove image directory {}: {}",
                        image.path.display(),
                        e
                    ))
                })?;
            }

            self.save_index_inner().await?;
            Ok(())
        } else {
            drop(index);
            Err(BoxError::OciImageError(format!(
                "Image not found: {}",
                reference
            )))
        }
    }

    /// List all stored images.
    pub async fn list(&self) -> Vec<StoredImage> {
        let index = self.index.read().await;
        index.values().cloned().collect()
    }

    /// Evict least-recently-used images until total size is under the limit.
    ///
    /// Returns the references of evicted images.
    pub async fn evict(&self) -> Result<Vec<String>> {
        let mut evicted = Vec::new();
        let mut total = self.total_size().await;

        while total > self.max_size_bytes {
            // Find the least recently used image
            let lru_ref = {
                let index = self.index.read().await;
                index
                    .values()
                    .min_by_key(|img| img.last_used)
                    .map(|img| img.reference.clone())
            };

            match lru_ref {
                Some(reference) => {
                    self.remove(&reference).await?;
                    evicted.push(reference);
                    total = self.total_size().await;
                }
                None => break,
            }
        }

        Ok(evicted)
    }

    /// Get total size of all stored images in bytes.
    pub async fn total_size(&self) -> u64 {
        let index = self.index.read().await;
        index.values().map(|img| img.size_bytes).sum()
    }

    /// Load index from disk.
    fn load_index(&mut self) -> Result<()> {
        let index_path = self.store_dir.join("index.json");
        if !index_path.exists() {
            return Ok(());
        }

        let data = std::fs::read_to_string(&index_path).map_err(|e| {
            BoxError::OciImageError(format!(
                "Failed to read image store index {}: {}",
                index_path.display(),
                e
            ))
        })?;

        let store_index: StoreIndex = serde_json::from_str(&data).map_err(|e| {
            BoxError::OciImageError(format!("Failed to parse image store index: {}", e))
        })?;

        let mut index = HashMap::new();
        for image in store_index.images {
            // Only include images whose directories still exist
            if image.path.exists() {
                index.insert(image.reference.clone(), image);
            }
        }

        // We need to set the inner value directly since we're in a sync context during construction
        self.index = Arc::new(RwLock::new(index));
        Ok(())
    }

    /// Save index to disk (async inner helper).
    async fn save_index_inner(&self) -> Result<()> {
        let index = self.index.read().await;
        let store_index = StoreIndex {
            images: index.values().cloned().collect(),
        };
        drop(index);

        let data = serde_json::to_string_pretty(&store_index)?;
        let index_path = self.store_dir.join("index.json");

        tokio::fs::write(&index_path, data).await.map_err(|e| {
            BoxError::OciImageError(format!(
                "Failed to write image store index {}: {}",
                index_path.display(),
                e
            ))
        })?;

        Ok(())
    }

    /// Get the store directory path.
    pub fn store_dir(&self) -> &Path {
        &self.store_dir
    }
}

#[async_trait::async_trait]
impl ImageStoreBackend for ImageStore {
    async fn get(&self, reference: &str) -> Option<StoredImage> {
        self.get(reference).await
    }

    async fn get_by_digest(&self, digest: &str) -> Option<StoredImage> {
        self.get_by_digest(digest).await
    }

    async fn put(&self, reference: &str, digest: &str, source_dir: &Path) -> Result<StoredImage> {
        self.put(reference, digest, source_dir).await
    }

    async fn remove(&self, reference: &str) -> Result<()> {
        self.remove(reference).await
    }

    async fn list(&self) -> Vec<StoredImage> {
        self.list().await
    }

    async fn evict(&self) -> Result<Vec<String>> {
        self.evict().await
    }

    async fn total_size(&self) -> u64 {
        self.total_size().await
    }
}

/// Recursively copy a directory.
fn copy_dir_recursive(src: &Path, dst: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let src_path = entry.path();
        let dst_path = dst.join(entry.file_name());
        if src_path.is_dir() {
            copy_dir_recursive(&src_path, &dst_path)?;
        } else {
            std::fs::copy(&src_path, &dst_path)?;
        }
    }
    Ok(())
}

/// Calculate total size of a directory recursively.
fn dir_size(path: &Path) -> u64 {
    let mut total = 0;
    if let Ok(entries) = std::fs::read_dir(path) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                total += dir_size(&path);
            } else if let Ok(meta) = path.metadata() {
                total += meta.len();
            }
        }
    }
    total
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn create_test_oci_layout(dir: &Path) {
        std::fs::create_dir_all(dir.join("blobs/sha256")).unwrap();
        std::fs::write(dir.join("oci-layout"), r#"{"imageLayoutVersion":"1.0.0"}"#).unwrap();
        std::fs::write(dir.join("index.json"), r#"{"manifests":[]}"#).unwrap();
        // Write some blob data to have measurable size
        std::fs::write(dir.join("blobs/sha256/testblob"), "x".repeat(1024)).unwrap();
    }

    #[tokio::test]
    async fn test_new_creates_directory() {
        let tmp = TempDir::new().unwrap();
        let store_dir = tmp.path().join("images");
        let store = ImageStore::new(&store_dir, 1024 * 1024).unwrap();
        assert!(store_dir.exists());
        assert_eq!(store.total_size().await, 0);
    }

    #[tokio::test]
    async fn test_put_and_get() {
        let tmp = TempDir::new().unwrap();
        let store_dir = tmp.path().join("store");
        let source_dir = tmp.path().join("source");
        create_test_oci_layout(&source_dir);

        let store = ImageStore::new(&store_dir, 10 * 1024 * 1024).unwrap();

        let stored = store
            .put("nginx:latest", "sha256:abc123", &source_dir)
            .await
            .unwrap();

        assert_eq!(stored.reference, "nginx:latest");
        assert_eq!(stored.digest, "sha256:abc123");
        assert!(stored.size_bytes > 0);
        assert!(stored.path.exists());

        // Get by reference
        let fetched = store.get("nginx:latest").await.unwrap();
        assert_eq!(fetched.digest, "sha256:abc123");

        // Get by digest
        let fetched = store.get_by_digest("sha256:abc123").await.unwrap();
        assert_eq!(fetched.reference, "nginx:latest");
    }

    #[tokio::test]
    async fn test_get_nonexistent() {
        let tmp = TempDir::new().unwrap();
        let store = ImageStore::new(tmp.path(), 1024 * 1024).unwrap();
        assert!(store.get("nonexistent").await.is_none());
    }

    #[tokio::test]
    async fn test_remove() {
        let tmp = TempDir::new().unwrap();
        let store_dir = tmp.path().join("store");
        let source_dir = tmp.path().join("source");
        create_test_oci_layout(&source_dir);

        let store = ImageStore::new(&store_dir, 10 * 1024 * 1024).unwrap();
        store
            .put("nginx:latest", "sha256:abc123", &source_dir)
            .await
            .unwrap();

        store.remove("nginx:latest").await.unwrap();
        assert!(store.get("nginx:latest").await.is_none());
    }

    #[tokio::test]
    async fn test_remove_nonexistent() {
        let tmp = TempDir::new().unwrap();
        let store = ImageStore::new(tmp.path(), 1024 * 1024).unwrap();
        assert!(store.remove("nonexistent").await.is_err());
    }

    #[tokio::test]
    async fn test_list() {
        let tmp = TempDir::new().unwrap();
        let store_dir = tmp.path().join("store");
        let source_dir = tmp.path().join("source");
        create_test_oci_layout(&source_dir);

        let store = ImageStore::new(&store_dir, 10 * 1024 * 1024).unwrap();
        store
            .put("nginx:latest", "sha256:aaa", &source_dir)
            .await
            .unwrap();
        store
            .put("alpine:3.18", "sha256:bbb", &source_dir)
            .await
            .unwrap();

        let images = store.list().await;
        assert_eq!(images.len(), 2);
    }

    #[tokio::test]
    async fn test_total_size() {
        let tmp = TempDir::new().unwrap();
        let store_dir = tmp.path().join("store");
        let source_dir = tmp.path().join("source");
        create_test_oci_layout(&source_dir);

        let store = ImageStore::new(&store_dir, 10 * 1024 * 1024).unwrap();
        store
            .put("nginx:latest", "sha256:aaa", &source_dir)
            .await
            .unwrap();

        assert!(store.total_size().await > 0);
    }

    #[tokio::test]
    async fn test_lru_eviction() {
        let tmp = TempDir::new().unwrap();
        let store_dir = tmp.path().join("store");
        let source_dir = tmp.path().join("source");
        create_test_oci_layout(&source_dir);

        // Set max size very small to trigger eviction
        let store = ImageStore::new(&store_dir, 100).unwrap();

        store
            .put("old:v1", "sha256:old1", &source_dir)
            .await
            .unwrap();

        // Sleep briefly so timestamps differ
        tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;

        store
            .put("new:v2", "sha256:new2", &source_dir)
            .await
            .unwrap();

        // Access the newer one to update its last_used
        store.get("new:v2").await;

        let evicted = store.evict().await.unwrap();
        // At least one image should be evicted (the older one first)
        assert!(!evicted.is_empty());
        assert!(evicted.contains(&"old:v1".to_string()));
    }

    #[tokio::test]
    async fn test_index_persistence() {
        let tmp = TempDir::new().unwrap();
        let store_dir = tmp.path().join("store");
        let source_dir = tmp.path().join("source");
        create_test_oci_layout(&source_dir);

        // Create store and add image
        {
            let store = ImageStore::new(&store_dir, 10 * 1024 * 1024).unwrap();
            store
                .put("nginx:latest", "sha256:persist", &source_dir)
                .await
                .unwrap();
        }

        // Create new store from same directory — should load persisted index
        {
            let store = ImageStore::new(&store_dir, 10 * 1024 * 1024).unwrap();
            let image = store.get("nginx:latest").await;
            assert!(image.is_some());
            assert_eq!(image.unwrap().digest, "sha256:persist");
        }
    }
}
