//! Cache for fully-built rootfs directories.
//!
//! Avoids rebuilding the rootfs from OCI layers when the same image
//! configuration has been seen before. The cache key is a SHA256 hash
//! of the image reference, layer digests, entrypoint, and environment.

use std::path::{Path, PathBuf};

use a3s_box_core::error::{BoxError, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Metadata for a cached rootfs entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RootfsMeta {
    /// Cache key (SHA256 hex string)
    pub key: String,
    /// Human-readable description of what produced this rootfs
    pub description: String,
    /// Size of the rootfs directory in bytes
    pub size_bytes: u64,
    /// When this rootfs was cached (Unix timestamp)
    pub cached_at: i64,
    /// Last time this rootfs was accessed (Unix timestamp)
    pub last_accessed: i64,
}

/// Cache for fully-built rootfs directories.
///
/// Rootfs entries are stored under `cache_dir/rootfs/<key>/`.
/// Metadata is stored alongside as `<key>.meta.json`.
pub struct RootfsCache {
    /// Root directory for rootfs cache (e.g., ~/.a3s/cache/rootfs)
    cache_dir: PathBuf,
}

impl RootfsCache {
    /// Create a new rootfs cache at the given directory.
    pub fn new(cache_dir: &Path) -> Result<Self> {
        std::fs::create_dir_all(cache_dir).map_err(|e| {
            BoxError::CacheError(format!(
                "Failed to create rootfs cache directory {}: {}",
                cache_dir.display(),
                e
            ))
        })?;

        Ok(Self {
            cache_dir: cache_dir.to_path_buf(),
        })
    }

    /// Compute a cache key from image components.
    ///
    /// The key is a SHA256 hash of the concatenation of:
    /// - image reference (e.g., "nginx:latest")
    /// - sorted layer digests
    /// - entrypoint
    /// - sorted environment variables
    pub fn compute_key(
        image_ref: &str,
        layer_digests: &[String],
        entrypoint: &[String],
        env: &[(String, String)],
    ) -> String {
        let mut hasher = Sha256::new();
        hasher.update(b"rootfs-cache-v1\n");
        hasher.update(image_ref.as_bytes());
        hasher.update(b"\n");

        for digest in layer_digests {
            hasher.update(digest.as_bytes());
            hasher.update(b"\n");
        }

        for part in entrypoint {
            hasher.update(part.as_bytes());
            hasher.update(b"\n");
        }

        let mut sorted_env: Vec<_> = env.to_vec();
        sorted_env.sort();
        for (k, v) in &sorted_env {
            hasher.update(k.as_bytes());
            hasher.update(b"=");
            hasher.update(v.as_bytes());
            hasher.update(b"\n");
        }

        hex::encode(hasher.finalize())
    }

    /// Get the path to a cached rootfs by key.
    ///
    /// Returns `None` if the rootfs is not cached or the cache entry is invalid.
    pub fn get(&self, key: &str) -> Result<Option<PathBuf>> {
        let rootfs_dir = self.cache_dir.join(key);
        let meta_path = self.cache_dir.join(format!("{}.meta.json", key));

        if !rootfs_dir.is_dir() || !meta_path.is_file() {
            return Ok(None);
        }

        // Update last_accessed timestamp
        if let Ok(content) = std::fs::read_to_string(&meta_path) {
            if let Ok(mut meta) = serde_json::from_str::<RootfsMeta>(&content) {
                meta.last_accessed = chrono::Utc::now().timestamp();
                let _ = std::fs::write(&meta_path, serde_json::to_string_pretty(&meta)?);
            }
        }

        Ok(Some(rootfs_dir))
    }

    /// Store a built rootfs directory in the cache.
    ///
    /// Copies the contents of `source_rootfs` into the cache keyed by `key`.
    /// Returns the path to the cached rootfs directory.
    pub fn put(&self, key: &str, source_rootfs: &Path, description: &str) -> Result<PathBuf> {
        let rootfs_dir = self.cache_dir.join(key);
        let meta_path = self.cache_dir.join(format!("{}.meta.json", key));

        // Remove existing entry if present
        if rootfs_dir.exists() {
            std::fs::remove_dir_all(&rootfs_dir).map_err(|e| {
                BoxError::CacheError(format!(
                    "Failed to remove existing rootfs cache entry {}: {}",
                    rootfs_dir.display(),
                    e
                ))
            })?;
        }

        // Copy source rootfs to cache
        super::layer_cache::copy_dir_recursive(source_rootfs, &rootfs_dir)?;

        // Calculate size
        let size_bytes = super::layer_cache::dir_size(&rootfs_dir).unwrap_or(0);

        // Write metadata
        let now = chrono::Utc::now().timestamp();
        let meta = RootfsMeta {
            key: key.to_string(),
            description: description.to_string(),
            size_bytes,
            cached_at: now,
            last_accessed: now,
        };
        std::fs::write(&meta_path, serde_json::to_string_pretty(&meta)?).map_err(|e| {
            BoxError::CacheError(format!(
                "Failed to write rootfs metadata {}: {}",
                meta_path.display(),
                e
            ))
        })?;

        tracing::debug!(
            key = %key,
            description = %description,
            size_bytes,
            path = %rootfs_dir.display(),
            "Cached rootfs"
        );

        Ok(rootfs_dir)
    }

    /// Remove a cached rootfs by key.
    pub fn invalidate(&self, key: &str) -> Result<()> {
        let rootfs_dir = self.cache_dir.join(key);
        let meta_path = self.cache_dir.join(format!("{}.meta.json", key));

        if rootfs_dir.exists() {
            std::fs::remove_dir_all(&rootfs_dir).map_err(|e| {
                BoxError::CacheError(format!(
                    "Failed to remove cached rootfs {}: {}",
                    rootfs_dir.display(),
                    e
                ))
            })?;
        }
        if meta_path.exists() {
            std::fs::remove_file(&meta_path).map_err(|e| {
                BoxError::CacheError(format!(
                    "Failed to remove rootfs metadata {}: {}",
                    meta_path.display(),
                    e
                ))
            })?;
        }

        Ok(())
    }

    /// Prune the cache to stay within the given entry count limit.
    ///
    /// Evicts least-recently-accessed entries first.
    /// Returns the number of entries evicted.
    pub fn prune(&self, max_entries: usize, max_bytes: u64) -> Result<usize> {
        let mut entries = self.list_entries()?;

        if entries.len() <= max_entries {
            let total_size: u64 = entries.iter().map(|e| e.size_bytes).sum();
            if total_size <= max_bytes {
                return Ok(0);
            }
        }

        // Sort by last_accessed ascending (oldest first)
        entries.sort_by_key(|e| e.last_accessed);

        let mut current_count = entries.len();
        let mut current_size: u64 = entries.iter().map(|e| e.size_bytes).sum();
        let mut evicted = 0;

        for entry in &entries {
            if current_count <= max_entries && current_size <= max_bytes {
                break;
            }
            self.invalidate(&entry.key)?;
            current_count -= 1;
            current_size = current_size.saturating_sub(entry.size_bytes);
            evicted += 1;

            tracing::debug!(
                key = %entry.key,
                description = %entry.description,
                size_bytes = entry.size_bytes,
                "Evicted cached rootfs"
            );
        }

        Ok(evicted)
    }

    /// List all cached rootfs entries with their metadata.
    pub fn list_entries(&self) -> Result<Vec<RootfsMeta>> {
        let mut entries = Vec::new();

        let read_dir = std::fs::read_dir(&self.cache_dir).map_err(|e| {
            BoxError::CacheError(format!(
                "Failed to read rootfs cache directory {}: {}",
                self.cache_dir.display(),
                e
            ))
        })?;

        for entry in read_dir {
            let entry = entry.map_err(|e| {
                BoxError::CacheError(format!("Failed to read directory entry: {}", e))
            })?;
            let path = entry.path();

            if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                if name.ends_with(".meta.json") {
                    if let Ok(content) = std::fs::read_to_string(&path) {
                        if let Ok(meta) = serde_json::from_str::<RootfsMeta>(&content) {
                            entries.push(meta);
                        }
                    }
                }
            }
        }

        Ok(entries)
    }

    /// Get the total size of all cached rootfs entries in bytes.
    pub fn total_size(&self) -> Result<u64> {
        Ok(self.list_entries()?.iter().map(|e| e.size_bytes).sum())
    }

    /// Get the number of cached rootfs entries.
    pub fn entry_count(&self) -> Result<usize> {
        Ok(self.list_entries()?.len())
    }
}

impl a3s_box_core::traits::CacheBackend for RootfsCache {
    fn get(&self, key: &str) -> Result<Option<PathBuf>> {
        self.get(key)
    }

    fn put(&self, key: &str, source_dir: &Path, description: &str) -> Result<PathBuf> {
        self.put(key, source_dir, description)
    }

    fn invalidate(&self, key: &str) -> Result<()> {
        self.invalidate(key)
    }

    fn prune(&self, max_entries: usize, max_bytes: u64) -> Result<usize> {
        self.prune(max_entries, max_bytes)
    }

    fn list(&self) -> Result<Vec<a3s_box_core::traits::CacheEntry>> {
        self.list_entries().map(|entries| {
            entries
                .into_iter()
                .map(|m| a3s_box_core::traits::CacheEntry {
                    key: m.key,
                    description: m.description,
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

    fn create_test_rootfs(dir: &Path, files: &[(&str, &str)]) {
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
    fn test_rootfs_cache_new_creates_directory() {
        let tmp = TempDir::new().unwrap();
        let cache_dir = tmp.path().join("rootfs");

        assert!(!cache_dir.exists());
        let _cache = RootfsCache::new(&cache_dir).unwrap();
        assert!(cache_dir.is_dir());
    }

    #[test]
    fn test_rootfs_cache_get_miss() {
        let tmp = TempDir::new().unwrap();
        let cache = RootfsCache::new(tmp.path()).unwrap();

        let result = cache.get("nonexistent_key").unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn test_rootfs_cache_put_and_get() {
        let tmp = TempDir::new().unwrap();
        let cache = RootfsCache::new(tmp.path()).unwrap();

        let source = tmp.path().join("source_rootfs");
        create_test_rootfs(
            &source,
            &[("bin/agent", "binary"), ("etc/config.json", "{}")],
        );

        let key = "abc123def456";
        let cached_path = cache.put(key, &source, "test rootfs").unwrap();

        assert!(cached_path.is_dir());
        assert!(cached_path.join("bin/agent").is_file());
        assert!(cached_path.join("etc/config.json").is_file());

        let result = cache.get(key).unwrap();
        assert!(result.is_some());
        assert_eq!(result.unwrap(), cached_path);
    }

    #[test]
    fn test_rootfs_cache_invalidate() {
        let tmp = TempDir::new().unwrap();
        let cache = RootfsCache::new(tmp.path()).unwrap();
        let key = "to_invalidate";

        let source = tmp.path().join("source");
        create_test_rootfs(&source, &[("data.bin", "data")]);
        cache.put(key, &source, "temp").unwrap();

        assert!(cache.get(key).unwrap().is_some());
        cache.invalidate(key).unwrap();
        assert!(cache.get(key).unwrap().is_none());
    }

    #[test]
    fn test_rootfs_cache_invalidate_nonexistent() {
        let tmp = TempDir::new().unwrap();
        let cache = RootfsCache::new(tmp.path()).unwrap();
        cache.invalidate("does_not_exist").unwrap();
    }

    #[test]
    fn test_rootfs_cache_list_entries() {
        let tmp = TempDir::new().unwrap();
        let cache = RootfsCache::new(tmp.path()).unwrap();

        assert_eq!(cache.list_entries().unwrap().len(), 0);

        let s1 = tmp.path().join("s1");
        create_test_rootfs(&s1, &[("a.txt", "aaa")]);
        cache.put("key1", &s1, "first").unwrap();

        let s2 = tmp.path().join("s2");
        create_test_rootfs(&s2, &[("b.txt", "bbb")]);
        cache.put("key2", &s2, "second").unwrap();

        let entries = cache.list_entries().unwrap();
        assert_eq!(entries.len(), 2);

        let keys: Vec<&str> = entries.iter().map(|e| e.key.as_str()).collect();
        assert!(keys.contains(&"key1"));
        assert!(keys.contains(&"key2"));
    }

    #[test]
    fn test_rootfs_cache_entry_count() {
        let tmp = TempDir::new().unwrap();
        let cache = RootfsCache::new(tmp.path()).unwrap();

        assert_eq!(cache.entry_count().unwrap(), 0);

        let source = tmp.path().join("source");
        create_test_rootfs(&source, &[("f.txt", "data")]);
        cache.put("k1", &source, "one").unwrap();
        cache.put("k2", &source, "two").unwrap();

        assert_eq!(cache.entry_count().unwrap(), 2);
    }

    #[test]
    fn test_rootfs_cache_total_size() {
        let tmp = TempDir::new().unwrap();
        let cache = RootfsCache::new(tmp.path()).unwrap();

        assert_eq!(cache.total_size().unwrap(), 0);

        let source = tmp.path().join("source");
        create_test_rootfs(&source, &[("data.txt", "hello world")]);
        cache.put("sized", &source, "sized entry").unwrap();

        assert!(cache.total_size().unwrap() > 0);
    }

    #[test]
    fn test_rootfs_cache_prune_by_count() {
        let tmp = TempDir::new().unwrap();
        let cache = RootfsCache::new(tmp.path()).unwrap();

        // Add 5 entries
        for i in 0..5 {
            let source = tmp.path().join(format!("s{}", i));
            create_test_rootfs(&source, &[("f.txt", "data")]);
            cache
                .put(&format!("key{}", i), &source, &format!("entry {}", i))
                .unwrap();
            std::thread::sleep(std::time::Duration::from_millis(10));
        }

        assert_eq!(cache.entry_count().unwrap(), 5);

        // Prune to max 2 entries
        let evicted = cache.prune(2, u64::MAX).unwrap();
        assert_eq!(evicted, 3);
        assert_eq!(cache.entry_count().unwrap(), 2);
    }

    #[test]
    fn test_rootfs_cache_prune_by_size() {
        let tmp = TempDir::new().unwrap();
        let cache = RootfsCache::new(tmp.path()).unwrap();

        for i in 0..3 {
            let source = tmp.path().join(format!("s{}", i));
            create_test_rootfs(&source, &[("f.txt", &"x".repeat(100))]);
            cache
                .put(&format!("key{}", i), &source, &format!("entry {}", i))
                .unwrap();
            std::thread::sleep(std::time::Duration::from_millis(10));
        }

        // Prune to 1 byte — should evict all but possibly one
        let evicted = cache.prune(usize::MAX, 1).unwrap();
        assert!(evicted >= 2);
    }

    #[test]
    fn test_rootfs_cache_prune_no_eviction_needed() {
        let tmp = TempDir::new().unwrap();
        let cache = RootfsCache::new(tmp.path()).unwrap();

        let source = tmp.path().join("source");
        create_test_rootfs(&source, &[("f.txt", "data")]);
        cache.put("key1", &source, "entry").unwrap();

        let evicted = cache.prune(10, u64::MAX).unwrap();
        assert_eq!(evicted, 0);
        assert_eq!(cache.entry_count().unwrap(), 1);
    }

    #[test]
    fn test_rootfs_cache_metadata_persists() {
        let tmp = TempDir::new().unwrap();
        let cache = RootfsCache::new(tmp.path()).unwrap();
        let key = "meta_test";

        let source = tmp.path().join("source");
        create_test_rootfs(&source, &[("file.txt", "content")]);
        cache.put(key, &source, "test description").unwrap();

        let meta_path = tmp.path().join(format!("{}.meta.json", key));
        assert!(meta_path.is_file());

        let content = std::fs::read_to_string(&meta_path).unwrap();
        let meta: RootfsMeta = serde_json::from_str(&content).unwrap();

        assert_eq!(meta.key, key);
        assert_eq!(meta.description, "test description");
        assert!(meta.size_bytes > 0);
        assert!(meta.cached_at > 0);
        assert_eq!(meta.cached_at, meta.last_accessed);
    }

    #[test]
    fn test_compute_key_deterministic() {
        let key1 = RootfsCache::compute_key(
            "nginx:latest",
            &["sha256:aaa".to_string(), "sha256:bbb".to_string()],
            &["/bin/nginx".to_string()],
            &[("PATH".to_string(), "/usr/bin".to_string())],
        );
        let key2 = RootfsCache::compute_key(
            "nginx:latest",
            &["sha256:aaa".to_string(), "sha256:bbb".to_string()],
            &["/bin/nginx".to_string()],
            &[("PATH".to_string(), "/usr/bin".to_string())],
        );
        assert_eq!(key1, key2);
    }

    #[test]
    fn test_compute_key_different_inputs() {
        let key1 = RootfsCache::compute_key("nginx:latest", &[], &[], &[]);
        let key2 = RootfsCache::compute_key("nginx:1.25", &[], &[], &[]);
        assert_ne!(key1, key2);
    }

    #[test]
    fn test_compute_key_env_order_independent() {
        let key1 = RootfsCache::compute_key(
            "img",
            &[],
            &[],
            &[
                ("A".to_string(), "1".to_string()),
                ("B".to_string(), "2".to_string()),
            ],
        );
        let key2 = RootfsCache::compute_key(
            "img",
            &[],
            &[],
            &[
                ("B".to_string(), "2".to_string()),
                ("A".to_string(), "1".to_string()),
            ],
        );
        assert_eq!(key1, key2);
    }

    #[test]
    fn test_compute_key_is_hex_sha256() {
        let key = RootfsCache::compute_key("test", &[], &[], &[]);
        // SHA256 hex is 64 characters
        assert_eq!(key.len(), 64);
        assert!(key.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn test_compute_key_layer_order_matters() {
        let key1 = RootfsCache::compute_key(
            "img",
            &["sha256:aaa".to_string(), "sha256:bbb".to_string()],
            &[],
            &[],
        );
        let key2 = RootfsCache::compute_key(
            "img",
            &["sha256:bbb".to_string(), "sha256:aaa".to_string()],
            &[],
            &[],
        );
        // Layer order matters (different filesystem result)
        assert_ne!(key1, key2);
    }

    #[test]
    fn test_compute_key_entrypoint_order_matters() {
        let key1 =
            RootfsCache::compute_key("img", &[], &["/bin/sh".to_string(), "-c".to_string()], &[]);
        let key2 =
            RootfsCache::compute_key("img", &[], &["-c".to_string(), "/bin/sh".to_string()], &[]);
        assert_ne!(key1, key2);
    }

    #[test]
    fn test_compute_key_with_special_characters() {
        let key = RootfsCache::compute_key(
            "registry.example.com/org/image:v1.0-beta+build.123",
            &["sha256:abc/def".to_string()],
            &[
                "/bin/sh".to_string(),
                "-c".to_string(),
                "echo 'hello world'".to_string(),
            ],
            &[("PATH".to_string(), "/usr/bin:/usr/local/bin".to_string())],
        );
        assert_eq!(key.len(), 64);
        assert!(key.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn test_compute_key_empty_all_params() {
        let key = RootfsCache::compute_key("", &[], &[], &[]);
        assert_eq!(key.len(), 64);
        assert!(key.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn test_rootfs_cache_get_updates_last_accessed() {
        let tmp = TempDir::new().unwrap();
        let cache = RootfsCache::new(tmp.path()).unwrap();
        let key = "access_test";

        let source = tmp.path().join("source");
        create_test_rootfs(&source, &[("f.txt", "data")]);
        cache.put(key, &source, "test").unwrap();

        // Read initial metadata
        let meta_path = tmp.path().join(format!("{}.meta.json", key));
        let content = std::fs::read_to_string(&meta_path).unwrap();
        let meta_before: RootfsMeta = serde_json::from_str(&content).unwrap();

        std::thread::sleep(std::time::Duration::from_millis(10));

        // Access the cache entry
        cache.get(key).unwrap();

        // Read updated metadata
        let content = std::fs::read_to_string(&meta_path).unwrap();
        let meta_after: RootfsMeta = serde_json::from_str(&content).unwrap();

        assert!(meta_after.last_accessed >= meta_before.last_accessed);
        assert_eq!(meta_after.cached_at, meta_before.cached_at);
    }

    #[test]
    fn test_rootfs_cache_get_directory_without_metadata() {
        let tmp = TempDir::new().unwrap();
        let cache = RootfsCache::new(tmp.path()).unwrap();
        let key = "no_meta";

        // Create rootfs directory but no metadata file
        std::fs::create_dir_all(tmp.path().join(key)).unwrap();

        let result = cache.get(key).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn test_rootfs_cache_get_metadata_without_directory() {
        let tmp = TempDir::new().unwrap();
        let cache = RootfsCache::new(tmp.path()).unwrap();
        let key = "no_dir";

        // Create metadata file but no rootfs directory
        let meta = RootfsMeta {
            key: key.to_string(),
            description: "orphan".to_string(),
            size_bytes: 0,
            cached_at: 0,
            last_accessed: 0,
        };
        std::fs::write(
            tmp.path().join(format!("{}.meta.json", key)),
            serde_json::to_string(&meta).unwrap(),
        )
        .unwrap();

        let result = cache.get(key).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn test_rootfs_cache_get_corrupted_metadata() {
        let tmp = TempDir::new().unwrap();
        let cache = RootfsCache::new(tmp.path()).unwrap();
        let key = "corrupted";

        // Create rootfs directory and corrupted metadata
        std::fs::create_dir_all(tmp.path().join(key)).unwrap();
        std::fs::write(
            tmp.path().join(format!("{}.meta.json", key)),
            "not valid json!!!",
        )
        .unwrap();

        // Should still return Some (directory + meta file both exist)
        let result = cache.get(key).unwrap();
        assert!(result.is_some());
    }

    #[test]
    fn test_rootfs_cache_put_source_not_exists() {
        let tmp = TempDir::new().unwrap();
        let cache = RootfsCache::new(tmp.path()).unwrap();

        let nonexistent = tmp.path().join("does_not_exist");
        let result = cache.put("bad_key", &nonexistent, "bad source");
        assert!(result.is_err());
    }

    #[test]
    fn test_rootfs_cache_put_overwrites_existing() {
        let tmp = TempDir::new().unwrap();
        let cache = RootfsCache::new(tmp.path()).unwrap();
        let key = "overwrite";

        // Put first version
        let s1 = tmp.path().join("v1");
        create_test_rootfs(&s1, &[("v1.txt", "version 1")]);
        cache.put(key, &s1, "first").unwrap();

        // Put second version
        let s2 = tmp.path().join("v2");
        create_test_rootfs(&s2, &[("v2.txt", "version 2")]);
        let cached = cache.put(key, &s2, "second").unwrap();

        assert!(!cached.join("v1.txt").exists());
        assert!(cached.join("v2.txt").is_file());

        // Metadata should reflect second put
        let meta_path = tmp.path().join(format!("{}.meta.json", key));
        let content = std::fs::read_to_string(&meta_path).unwrap();
        let meta: RootfsMeta = serde_json::from_str(&content).unwrap();
        assert_eq!(meta.description, "second");
    }

    #[test]
    fn test_rootfs_cache_prune_both_constraints() {
        let tmp = TempDir::new().unwrap();
        let cache = RootfsCache::new(tmp.path()).unwrap();

        // Add 5 entries with 100 bytes each
        for i in 0..5 {
            let source = tmp.path().join(format!("s{}", i));
            create_test_rootfs(&source, &[("f.txt", &"x".repeat(100))]);
            cache
                .put(&format!("key{}", i), &source, &format!("entry {}", i))
                .unwrap();
            std::thread::sleep(std::time::Duration::from_millis(10));
        }

        // Prune with both constraints: max 3 entries AND max 200 bytes
        // Both constraints should be satisfied
        let evicted = cache.prune(3, 200).unwrap();
        assert!(evicted >= 2);
        let remaining = cache.entry_count().unwrap();
        assert!(remaining <= 3);
    }

    #[test]
    fn test_rootfs_cache_prune_zero_limits() {
        let tmp = TempDir::new().unwrap();
        let cache = RootfsCache::new(tmp.path()).unwrap();

        let source = tmp.path().join("source");
        create_test_rootfs(&source, &[("f.txt", "data")]);
        cache.put("k1", &source, "one").unwrap();
        cache.put("k2", &source, "two").unwrap();

        // Prune with 0 entries limit — should evict all
        let evicted = cache.prune(0, u64::MAX).unwrap();
        assert_eq!(evicted, 2);
        assert_eq!(cache.entry_count().unwrap(), 0);
    }

    #[test]
    fn test_rootfs_cache_list_entries_ignores_non_meta_files() {
        let tmp = TempDir::new().unwrap();
        let cache = RootfsCache::new(tmp.path()).unwrap();

        // Add a valid entry
        let source = tmp.path().join("source");
        create_test_rootfs(&source, &[("f.txt", "data")]);
        cache.put("valid_key", &source, "valid").unwrap();

        // Add noise files
        std::fs::write(tmp.path().join("random.txt"), "noise").unwrap();
        std::fs::write(tmp.path().join("other.json"), "{}").unwrap();
        std::fs::create_dir_all(tmp.path().join("random_dir")).unwrap();

        let entries = cache.list_entries().unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].key, "valid_key");
    }

    #[test]
    fn test_rootfs_cache_list_entries_skips_invalid_json() {
        let tmp = TempDir::new().unwrap();
        let cache = RootfsCache::new(tmp.path()).unwrap();

        // Add a valid entry
        let source = tmp.path().join("source");
        create_test_rootfs(&source, &[("f.txt", "data")]);
        cache.put("valid_key", &source, "valid").unwrap();

        // Add corrupted .meta.json
        std::fs::write(tmp.path().join("corrupted.meta.json"), "not json").unwrap();

        let entries = cache.list_entries().unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].key, "valid_key");
    }

    #[test]
    fn test_rootfs_cache_put_preserves_content() {
        let tmp = TempDir::new().unwrap();
        let cache = RootfsCache::new(tmp.path()).unwrap();

        let source = tmp.path().join("source");
        create_test_rootfs(
            &source,
            &[
                ("bin/agent", "binary_content"),
                ("etc/config.json", r#"{"key":"value"}"#),
                ("lib/deep/nested.so", "shared_object"),
            ],
        );

        let cached = cache.put("content_key", &source, "content test").unwrap();

        assert_eq!(
            std::fs::read_to_string(cached.join("bin/agent")).unwrap(),
            "binary_content"
        );
        assert_eq!(
            std::fs::read_to_string(cached.join("etc/config.json")).unwrap(),
            r#"{"key":"value"}"#
        );
        assert_eq!(
            std::fs::read_to_string(cached.join("lib/deep/nested.so")).unwrap(),
            "shared_object"
        );
    }

    #[test]
    fn test_rootfs_cache_invalidate_then_put_same_key() {
        let tmp = TempDir::new().unwrap();
        let cache = RootfsCache::new(tmp.path()).unwrap();
        let key = "reuse_key";

        let s1 = tmp.path().join("s1");
        create_test_rootfs(&s1, &[("v1.txt", "first")]);
        cache.put(key, &s1, "first").unwrap();

        cache.invalidate(key).unwrap();
        assert!(cache.get(key).unwrap().is_none());

        let s2 = tmp.path().join("s2");
        create_test_rootfs(&s2, &[("v2.txt", "second")]);
        let cached = cache.put(key, &s2, "second").unwrap();

        assert!(cache.get(key).unwrap().is_some());
        assert!(cached.join("v2.txt").is_file());
        assert!(!cached.join("v1.txt").exists());
    }
}
