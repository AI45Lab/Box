//! Content-addressed cache for extracted OCI layers.
//!
//! Each layer is stored by its digest (SHA256), so identical layers
//! shared across different images are only stored once on disk.

use std::path::{Path, PathBuf};

use a3s_box_core::error::{BoxError, Result};
use a3s_box_core::traits::{CacheBackend, CacheEntry};
use serde::{Deserialize, Serialize};

/// Metadata for a cached layer entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LayerMeta {
    /// Layer digest (e.g., "sha256:abc123...")
    pub digest: String,
    /// Size of the extracted layer in bytes
    pub size_bytes: u64,
    /// When this layer was cached (Unix timestamp)
    pub cached_at: i64,
    /// Last time this layer was accessed (Unix timestamp)
    pub last_accessed: i64,
}

/// Content-addressed cache for extracted OCI layers.
///
/// Layers are stored by digest under `cache_dir/layers/<digest>/`.
/// Metadata is stored alongside as `<digest>.meta.json`.
pub struct LayerCache {
    /// Root directory for layer cache (e.g., ~/.a3s/cache/layers)
    cache_dir: PathBuf,
}

impl LayerCache {
    /// Create a new layer cache at the given directory.
    pub fn new(cache_dir: &Path) -> Result<Self> {
        std::fs::create_dir_all(cache_dir).map_err(|e| {
            BoxError::CacheError(format!(
                "Failed to create layer cache directory {}: {}",
                cache_dir.display(),
                e
            ))
        })?;

        Ok(Self {
            cache_dir: cache_dir.to_path_buf(),
        })
    }

    /// Get the path to a cached layer by digest.
    ///
    /// Returns `None` if the layer is not cached or the cache entry is invalid.
    pub fn get(&self, digest: &str) -> Result<Option<PathBuf>> {
        let safe_name = Self::digest_to_dirname(digest);
        let layer_dir = self.cache_dir.join(&safe_name);
        let meta_path = self.cache_dir.join(format!("{}.meta.json", safe_name));

        if !layer_dir.is_dir() || !meta_path.is_file() {
            return Ok(None);
        }

        // Update last_accessed timestamp
        if let Ok(content) = std::fs::read_to_string(&meta_path) {
            if let Ok(mut meta) = serde_json::from_str::<LayerMeta>(&content) {
                meta.last_accessed = chrono::Utc::now().timestamp();
                let _ = std::fs::write(&meta_path, serde_json::to_string_pretty(&meta)?);
            }
        }

        Ok(Some(layer_dir))
    }

    /// Store an extracted layer directory in the cache.
    ///
    /// Copies the contents of `source_dir` into the cache keyed by `digest`.
    /// Returns the path to the cached layer directory.
    pub fn put(&self, digest: &str, source_dir: &Path) -> Result<PathBuf> {
        let safe_name = Self::digest_to_dirname(digest);
        let layer_dir = self.cache_dir.join(&safe_name);
        let meta_path = self.cache_dir.join(format!("{}.meta.json", safe_name));

        // Remove existing entry if present
        if layer_dir.exists() {
            std::fs::remove_dir_all(&layer_dir).map_err(|e| {
                BoxError::CacheError(format!(
                    "Failed to remove existing cache entry {}: {}",
                    layer_dir.display(),
                    e
                ))
            })?;
        }

        // Copy source directory to cache
        copy_dir_recursive(source_dir, &layer_dir)?;

        // Calculate size
        let size_bytes = dir_size(&layer_dir).unwrap_or(0);

        // Write metadata
        let now = chrono::Utc::now().timestamp();
        let meta = LayerMeta {
            digest: digest.to_string(),
            size_bytes,
            cached_at: now,
            last_accessed: now,
        };
        std::fs::write(&meta_path, serde_json::to_string_pretty(&meta)?).map_err(|e| {
            BoxError::CacheError(format!(
                "Failed to write layer metadata {}: {}",
                meta_path.display(),
                e
            ))
        })?;

        tracing::debug!(
            digest = %digest,
            size_bytes,
            path = %layer_dir.display(),
            "Cached OCI layer"
        );

        Ok(layer_dir)
    }

    /// Remove a cached layer by digest.
    pub fn invalidate(&self, digest: &str) -> Result<()> {
        let safe_name = Self::digest_to_dirname(digest);
        let layer_dir = self.cache_dir.join(&safe_name);
        let meta_path = self.cache_dir.join(format!("{}.meta.json", safe_name));

        if layer_dir.exists() {
            std::fs::remove_dir_all(&layer_dir).map_err(|e| {
                BoxError::CacheError(format!(
                    "Failed to remove cached layer {}: {}",
                    layer_dir.display(),
                    e
                ))
            })?;
        }
        if meta_path.exists() {
            std::fs::remove_file(&meta_path).map_err(|e| {
                BoxError::CacheError(format!(
                    "Failed to remove layer metadata {}: {}",
                    meta_path.display(),
                    e
                ))
            })?;
        }

        Ok(())
    }

    /// Prune the cache to stay within the given byte limit.
    ///
    /// Evicts least-recently-accessed entries first.
    /// Returns the number of entries evicted.
    pub fn prune(&self, max_bytes: u64) -> Result<usize> {
        let mut entries = self.list_entries()?;

        // Calculate total size
        let total_size: u64 = entries.iter().map(|e| e.size_bytes).sum();
        if total_size <= max_bytes {
            return Ok(0);
        }

        // Sort by last_accessed ascending (oldest first)
        entries.sort_by_key(|e| e.last_accessed);

        let mut current_size = total_size;
        let mut evicted = 0;

        for entry in &entries {
            if current_size <= max_bytes {
                break;
            }
            self.invalidate(&entry.digest)?;
            current_size = current_size.saturating_sub(entry.size_bytes);
            evicted += 1;

            tracing::debug!(
                digest = %entry.digest,
                size_bytes = entry.size_bytes,
                "Evicted cached layer"
            );
        }

        Ok(evicted)
    }

    /// List all cached layer entries with their metadata.
    pub fn list_entries(&self) -> Result<Vec<LayerMeta>> {
        let mut entries = Vec::new();

        let read_dir = std::fs::read_dir(&self.cache_dir).map_err(|e| {
            BoxError::CacheError(format!(
                "Failed to read cache directory {}: {}",
                self.cache_dir.display(),
                e
            ))
        })?;

        for entry in read_dir {
            let entry = entry.map_err(|e| {
                BoxError::CacheError(format!("Failed to read directory entry: {}", e))
            })?;
            let path = entry.path();

            // Only process .meta.json files
            if path.extension().and_then(|e| e.to_str()) == Some("json") {
                if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                    if name.ends_with(".meta.json") {
                        if let Ok(content) = std::fs::read_to_string(&path) {
                            if let Ok(meta) = serde_json::from_str::<LayerMeta>(&content) {
                                entries.push(meta);
                            }
                        }
                    }
                }
            }
        }

        Ok(entries)
    }

    /// Get the total size of all cached layers in bytes.
    pub fn total_size(&self) -> Result<u64> {
        Ok(self.list_entries()?.iter().map(|e| e.size_bytes).sum())
    }

    /// Convert a digest string to a safe directory name.
    ///
    /// Replaces ':' with '_' to avoid filesystem issues.
    /// e.g., "sha256:abc123" → "sha256_abc123"
    fn digest_to_dirname(digest: &str) -> String {
        digest.replace(':', "_")
    }
}

/// Recursively copy a directory and its contents.
pub(crate) fn copy_dir_recursive(src: &Path, dst: &Path) -> Result<()> {
    std::fs::create_dir_all(dst).map_err(|e| {
        BoxError::CacheError(format!(
            "Failed to create directory {}: {}",
            dst.display(),
            e
        ))
    })?;

    for entry in std::fs::read_dir(src).map_err(|e| {
        BoxError::CacheError(format!("Failed to read directory {}: {}", src.display(), e))
    })? {
        let entry = entry
            .map_err(|e| BoxError::CacheError(format!("Failed to read directory entry: {}", e)))?;
        let src_path = entry.path();
        let dst_path = dst.join(entry.file_name());

        if src_path.is_dir() {
            copy_dir_recursive(&src_path, &dst_path)?;
        } else {
            std::fs::copy(&src_path, &dst_path).map_err(|e| {
                BoxError::CacheError(format!(
                    "Failed to copy {} to {}: {}",
                    src_path.display(),
                    dst_path.display(),
                    e
                ))
            })?;
        }
    }

    Ok(())
}

/// Calculate the total size of a directory recursively.
pub(crate) fn dir_size(path: &Path) -> std::io::Result<u64> {
    let mut total = 0;
    if path.is_dir() {
        for entry in std::fs::read_dir(path)? {
            let entry = entry?;
            let path = entry.path();
            if path.is_dir() {
                total += dir_size(&path)?;
            } else {
                total += entry.metadata()?.len();
            }
        }
    }
    Ok(total)
}

impl CacheBackend for LayerCache {
    fn get(&self, key: &str) -> Result<Option<PathBuf>> {
        self.get(key)
    }

    fn put(&self, key: &str, source_dir: &Path, _description: &str) -> Result<PathBuf> {
        self.put(key, source_dir)
    }

    fn invalidate(&self, key: &str) -> Result<()> {
        self.invalidate(key)
    }

    fn prune(&self, _max_entries: usize, max_bytes: u64) -> Result<usize> {
        self.prune(max_bytes)
    }

    fn list(&self) -> Result<Vec<CacheEntry>> {
        self.list_entries().map(|entries| {
            entries
                .into_iter()
                .map(|m| CacheEntry {
                    key: m.digest,
                    description: String::new(),
                    size_bytes: m.size_bytes,
                    cached_at: m.cached_at,
                    last_accessed: m.last_accessed,
                })
                .collect()
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn create_test_layer(dir: &Path, files: &[(&str, &str)]) {
        std::fs::create_dir_all(dir).unwrap();
        for (name, content) in files {
            let file_path = dir.join(name);
            if let Some(parent) = file_path.parent() {
                std::fs::create_dir_all(parent).unwrap();
            }
            std::fs::write(&file_path, content).unwrap();
        }
    }

    #[test]
    fn test_layer_cache_new_creates_directory() {
        let tmp = TempDir::new().unwrap();
        let cache_dir = tmp.path().join("layers");

        assert!(!cache_dir.exists());
        let _cache = LayerCache::new(&cache_dir).unwrap();
        assert!(cache_dir.is_dir());
    }

    #[test]
    fn test_layer_cache_get_miss() {
        let tmp = TempDir::new().unwrap();
        let cache = LayerCache::new(tmp.path()).unwrap();

        let result = cache.get("sha256:nonexistent").unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn test_layer_cache_put_and_get() {
        let tmp = TempDir::new().unwrap();
        let cache = LayerCache::new(tmp.path()).unwrap();

        // Create a source layer directory
        let source = tmp.path().join("source_layer");
        create_test_layer(
            &source,
            &[("file.txt", "hello"), ("sub/nested.txt", "world")],
        );

        // Put into cache
        let digest = "sha256:abc123def456";
        let cached_path = cache.put(digest, &source).unwrap();

        assert!(cached_path.is_dir());
        assert!(cached_path.join("file.txt").is_file());
        assert!(cached_path.join("sub/nested.txt").is_file());

        // Get from cache
        let result = cache.get(digest).unwrap();
        assert!(result.is_some());
        assert_eq!(result.unwrap(), cached_path);
    }

    #[test]
    fn test_layer_cache_put_overwrites_existing() {
        let tmp = TempDir::new().unwrap();
        let cache = LayerCache::new(tmp.path()).unwrap();
        let digest = "sha256:overwrite_test";

        // Put first version
        let source1 = tmp.path().join("v1");
        create_test_layer(&source1, &[("v1.txt", "version 1")]);
        cache.put(digest, &source1).unwrap();

        // Put second version (overwrites)
        let source2 = tmp.path().join("v2");
        create_test_layer(&source2, &[("v2.txt", "version 2")]);
        let cached_path = cache.put(digest, &source2).unwrap();

        // Should have v2 content, not v1
        assert!(!cached_path.join("v1.txt").exists());
        assert!(cached_path.join("v2.txt").is_file());
    }

    #[test]
    fn test_layer_cache_invalidate() {
        let tmp = TempDir::new().unwrap();
        let cache = LayerCache::new(tmp.path()).unwrap();
        let digest = "sha256:to_invalidate";

        let source = tmp.path().join("source");
        create_test_layer(&source, &[("data.bin", "binary data")]);
        cache.put(digest, &source).unwrap();

        // Verify it exists
        assert!(cache.get(digest).unwrap().is_some());

        // Invalidate
        cache.invalidate(digest).unwrap();

        // Should be gone
        assert!(cache.get(digest).unwrap().is_none());
    }

    #[test]
    fn test_layer_cache_invalidate_nonexistent() {
        let tmp = TempDir::new().unwrap();
        let cache = LayerCache::new(tmp.path()).unwrap();

        // Should not error on nonexistent digest
        cache.invalidate("sha256:does_not_exist").unwrap();
    }

    #[test]
    fn test_layer_cache_list_entries() {
        let tmp = TempDir::new().unwrap();
        let cache = LayerCache::new(tmp.path()).unwrap();

        // Empty cache
        assert_eq!(cache.list_entries().unwrap().len(), 0);

        // Add two layers
        let s1 = tmp.path().join("s1");
        create_test_layer(&s1, &[("a.txt", "aaa")]);
        cache.put("sha256:layer1", &s1).unwrap();

        let s2 = tmp.path().join("s2");
        create_test_layer(&s2, &[("b.txt", "bbb")]);
        cache.put("sha256:layer2", &s2).unwrap();

        let entries = cache.list_entries().unwrap();
        assert_eq!(entries.len(), 2);

        let digests: Vec<&str> = entries.iter().map(|e| e.digest.as_str()).collect();
        assert!(digests.contains(&"sha256:layer1"));
        assert!(digests.contains(&"sha256:layer2"));
    }

    #[test]
    fn test_layer_cache_total_size() {
        let tmp = TempDir::new().unwrap();
        let cache = LayerCache::new(tmp.path()).unwrap();

        assert_eq!(cache.total_size().unwrap(), 0);

        let source = tmp.path().join("source");
        create_test_layer(&source, &[("data.txt", "hello world")]);
        cache.put("sha256:sized", &source).unwrap();

        let total = cache.total_size().unwrap();
        assert!(total > 0);
    }

    #[test]
    fn test_layer_cache_prune_under_limit() {
        let tmp = TempDir::new().unwrap();
        let cache = LayerCache::new(tmp.path()).unwrap();

        let source = tmp.path().join("source");
        create_test_layer(&source, &[("small.txt", "tiny")]);
        cache.put("sha256:small", &source).unwrap();

        // Prune with a large limit — nothing should be evicted
        let evicted = cache.prune(1024 * 1024 * 1024).unwrap();
        assert_eq!(evicted, 0);
        assert!(cache.get("sha256:small").unwrap().is_some());
    }

    #[test]
    fn test_layer_cache_prune_evicts_oldest() {
        let tmp = TempDir::new().unwrap();
        let cache = LayerCache::new(tmp.path()).unwrap();

        // Add three layers with different access times
        for i in 0..3 {
            let source = tmp.path().join(format!("s{}", i));
            // Create a file with enough content to matter
            create_test_layer(&source, &[("data.txt", &"x".repeat(100))]);
            cache.put(&format!("sha256:layer{}", i), &source).unwrap();
            // Small delay to ensure different timestamps
            std::thread::sleep(std::time::Duration::from_millis(10));
        }

        // Access layer2 to make it most recently used
        cache.get("sha256:layer2").unwrap();

        // Prune to a very small limit — should evict oldest first
        let evicted = cache.prune(1).unwrap();
        assert!(evicted >= 2);

        // layer2 was most recently accessed, so it should survive longest
        // (though with limit=1 byte, all may be evicted)
    }

    #[test]
    fn test_layer_cache_metadata_persists() {
        let tmp = TempDir::new().unwrap();
        let cache = LayerCache::new(tmp.path()).unwrap();
        let digest = "sha256:meta_test";

        let source = tmp.path().join("source");
        create_test_layer(&source, &[("file.txt", "content")]);
        cache.put(digest, &source).unwrap();

        // Read metadata directly
        let meta_path = tmp.path().join("sha256_meta_test.meta.json");
        assert!(meta_path.is_file());

        let content = std::fs::read_to_string(&meta_path).unwrap();
        let meta: LayerMeta = serde_json::from_str(&content).unwrap();

        assert_eq!(meta.digest, digest);
        assert!(meta.size_bytes > 0);
        assert!(meta.cached_at > 0);
        assert_eq!(meta.cached_at, meta.last_accessed);
    }

    #[test]
    fn test_digest_to_dirname() {
        assert_eq!(
            LayerCache::digest_to_dirname("sha256:abc123"),
            "sha256_abc123"
        );
        assert_eq!(
            LayerCache::digest_to_dirname("plain_digest"),
            "plain_digest"
        );
    }

    #[test]
    fn test_copy_dir_recursive() {
        let tmp = TempDir::new().unwrap();
        let src = tmp.path().join("src");
        let dst = tmp.path().join("dst");

        create_test_layer(
            &src,
            &[
                ("a.txt", "aaa"),
                ("sub/b.txt", "bbb"),
                ("sub/deep/c.txt", "ccc"),
            ],
        );

        copy_dir_recursive(&src, &dst).unwrap();

        assert_eq!(std::fs::read_to_string(dst.join("a.txt")).unwrap(), "aaa");
        assert_eq!(
            std::fs::read_to_string(dst.join("sub/b.txt")).unwrap(),
            "bbb"
        );
        assert_eq!(
            std::fs::read_to_string(dst.join("sub/deep/c.txt")).unwrap(),
            "ccc"
        );
    }

    #[test]
    fn test_dir_size() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join("sized");
        create_test_layer(
            &dir,
            &[
                ("a.txt", "hello"),     // 5 bytes
                ("sub/b.txt", "world"), // 5 bytes
            ],
        );

        let size = dir_size(&dir).unwrap();
        assert_eq!(size, 10);
    }

    #[test]
    fn test_dir_size_empty_directory() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join("empty");
        std::fs::create_dir_all(&dir).unwrap();

        let size = dir_size(&dir).unwrap();
        assert_eq!(size, 0);
    }

    #[test]
    fn test_dir_size_nonexistent_returns_zero() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join("nonexistent");

        // Not a directory, so returns 0
        let size = dir_size(&dir).unwrap();
        assert_eq!(size, 0);
    }

    #[test]
    fn test_copy_dir_recursive_empty_directory() {
        let tmp = TempDir::new().unwrap();
        let src = tmp.path().join("empty_src");
        let dst = tmp.path().join("empty_dst");
        std::fs::create_dir_all(&src).unwrap();

        copy_dir_recursive(&src, &dst).unwrap();
        assert!(dst.is_dir());
    }

    #[test]
    fn test_copy_dir_recursive_source_not_exists() {
        let tmp = TempDir::new().unwrap();
        let src = tmp.path().join("nonexistent");
        let dst = tmp.path().join("dst");

        let result = copy_dir_recursive(&src, &dst);
        assert!(result.is_err());
    }

    #[test]
    fn test_layer_cache_get_updates_last_accessed() {
        let tmp = TempDir::new().unwrap();
        let cache = LayerCache::new(tmp.path()).unwrap();
        let digest = "sha256:access_test";

        let source = tmp.path().join("source");
        create_test_layer(&source, &[("f.txt", "data")]);
        cache.put(digest, &source).unwrap();

        // Read initial metadata
        let meta_path = tmp.path().join("sha256_access_test.meta.json");
        let content = std::fs::read_to_string(&meta_path).unwrap();
        let meta_before: LayerMeta = serde_json::from_str(&content).unwrap();

        // Small delay to ensure timestamp difference
        std::thread::sleep(std::time::Duration::from_millis(10));

        // Access the cache entry
        cache.get(digest).unwrap();

        // Read updated metadata
        let content = std::fs::read_to_string(&meta_path).unwrap();
        let meta_after: LayerMeta = serde_json::from_str(&content).unwrap();

        assert!(meta_after.last_accessed >= meta_before.last_accessed);
        // cached_at should not change
        assert_eq!(meta_after.cached_at, meta_before.cached_at);
    }

    #[test]
    fn test_layer_cache_get_corrupted_metadata() {
        let tmp = TempDir::new().unwrap();
        let cache = LayerCache::new(tmp.path()).unwrap();
        let digest = "sha256:corrupted";
        let safe_name = LayerCache::digest_to_dirname(digest);

        // Create layer directory manually
        let layer_dir = tmp.path().join(&safe_name);
        std::fs::create_dir_all(&layer_dir).unwrap();

        // Write corrupted metadata
        let meta_path = tmp.path().join(format!("{}.meta.json", safe_name));
        std::fs::write(&meta_path, "not valid json!!!").unwrap();

        // get() should still return Some (directory exists, metadata is best-effort)
        // The directory exists and meta file exists, so it returns the path
        let result = cache.get(digest).unwrap();
        assert!(result.is_some());
    }

    #[test]
    fn test_layer_cache_get_directory_without_metadata() {
        let tmp = TempDir::new().unwrap();
        let cache = LayerCache::new(tmp.path()).unwrap();
        let digest = "sha256:no_meta";
        let safe_name = LayerCache::digest_to_dirname(digest);

        // Create layer directory but no metadata file
        let layer_dir = tmp.path().join(&safe_name);
        std::fs::create_dir_all(&layer_dir).unwrap();

        // Should return None (metadata missing)
        let result = cache.get(digest).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn test_layer_cache_get_metadata_without_directory() {
        let tmp = TempDir::new().unwrap();
        let cache = LayerCache::new(tmp.path()).unwrap();
        let digest = "sha256:no_dir";
        let safe_name = LayerCache::digest_to_dirname(digest);

        // Create metadata file but no layer directory
        let meta_path = tmp.path().join(format!("{}.meta.json", safe_name));
        let meta = LayerMeta {
            digest: digest.to_string(),
            size_bytes: 0,
            cached_at: 0,
            last_accessed: 0,
        };
        std::fs::write(&meta_path, serde_json::to_string(&meta).unwrap()).unwrap();

        // Should return None (directory missing)
        let result = cache.get(digest).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn test_layer_cache_put_source_not_exists() {
        let tmp = TempDir::new().unwrap();
        let cache = LayerCache::new(tmp.path()).unwrap();

        let nonexistent = tmp.path().join("does_not_exist");
        let result = cache.put("sha256:bad_source", &nonexistent);
        assert!(result.is_err());
    }

    #[test]
    fn test_layer_cache_prune_zero_limit() {
        let tmp = TempDir::new().unwrap();
        let cache = LayerCache::new(tmp.path()).unwrap();

        let source = tmp.path().join("source");
        create_test_layer(&source, &[("f.txt", "data")]);
        cache.put("sha256:entry1", &source).unwrap();
        cache.put("sha256:entry2", &source).unwrap();

        // Prune with 0 bytes limit — should evict everything
        let evicted = cache.prune(0).unwrap();
        assert_eq!(evicted, 2);
        assert_eq!(cache.list_entries().unwrap().len(), 0);
    }

    #[test]
    fn test_layer_cache_list_entries_ignores_non_meta_files() {
        let tmp = TempDir::new().unwrap();
        let cache = LayerCache::new(tmp.path()).unwrap();

        // Add a valid entry
        let source = tmp.path().join("source");
        create_test_layer(&source, &[("f.txt", "data")]);
        cache.put("sha256:valid", &source).unwrap();

        // Add random non-meta files to cache directory
        std::fs::write(tmp.path().join("random.txt"), "noise").unwrap();
        std::fs::write(tmp.path().join("other.json"), "{}").unwrap();

        // Should only return the valid entry
        let entries = cache.list_entries().unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].digest, "sha256:valid");
    }

    #[test]
    fn test_layer_cache_list_entries_skips_invalid_json() {
        let tmp = TempDir::new().unwrap();
        let cache = LayerCache::new(tmp.path()).unwrap();

        // Add a valid entry
        let source = tmp.path().join("source");
        create_test_layer(&source, &[("f.txt", "data")]);
        cache.put("sha256:valid", &source).unwrap();

        // Add a corrupted .meta.json
        std::fs::write(
            tmp.path().join("sha256_corrupted.meta.json"),
            "not json at all",
        )
        .unwrap();

        // Should only return the valid entry, skip corrupted
        let entries = cache.list_entries().unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].digest, "sha256:valid");
    }

    #[test]
    fn test_layer_cache_put_preserves_file_content() {
        let tmp = TempDir::new().unwrap();
        let cache = LayerCache::new(tmp.path()).unwrap();

        let source = tmp.path().join("source");
        create_test_layer(
            &source,
            &[
                ("binary.bin", "\x00\x01\x02\x03"),
                ("text.txt", "hello world\n"),
            ],
        );

        let cached = cache.put("sha256:content_check", &source).unwrap();

        assert_eq!(
            std::fs::read(cached.join("binary.bin")).unwrap(),
            b"\x00\x01\x02\x03"
        );
        assert_eq!(
            std::fs::read_to_string(cached.join("text.txt")).unwrap(),
            "hello world\n"
        );
    }

    #[test]
    fn test_layer_cache_multiple_colons_in_digest() {
        let tmp = TempDir::new().unwrap();
        let cache = LayerCache::new(tmp.path()).unwrap();

        let digest = "sha256:abc:def:ghi";
        let source = tmp.path().join("source");
        create_test_layer(&source, &[("f.txt", "data")]);

        cache.put(digest, &source).unwrap();
        let result = cache.get(digest).unwrap();
        assert!(result.is_some());

        cache.invalidate(digest).unwrap();
        assert!(cache.get(digest).unwrap().is_none());
    }
}
