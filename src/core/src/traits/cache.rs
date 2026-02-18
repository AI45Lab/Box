//! Cache backend abstraction.
//!
//! Decouples rootfs and layer caching from the filesystem-based LRU
//! implementation in `a3s-box-runtime`. Implementations can use any
//! storage backend: local filesystem, shared NFS, Redis, content-addressable
//! store, etc.
//!
//! The cache operates on opaque string keys and directory-shaped values
//! (a `PathBuf` pointing to a directory tree). The backend is responsible
//! for storage, retrieval, and eviction.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::Result;

/// Metadata about a single cache entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CacheEntry {
    /// Cache key.
    pub key: String,

    /// Human-readable description of the cached content.
    pub description: String,

    /// Size of the cached content in bytes.
    pub size_bytes: u64,

    /// When this entry was first cached (Unix timestamp).
    pub cached_at: i64,

    /// When this entry was last accessed (Unix timestamp).
    pub last_accessed: i64,
}

/// Statistics about the cache as a whole.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CacheStats {
    /// Number of entries in the cache.
    pub entry_count: usize,

    /// Total size of all cached content in bytes.
    pub total_bytes: u64,
}

/// Abstraction over directory-based caching.
///
/// Used for both OCI layer caching and fully-built rootfs caching.
/// The key is an opaque string (typically a content hash), and the
/// value is a directory tree on disk.
///
/// # Lifecycle
///
/// 1. `get(key)` — check if a cached directory exists for this key
/// 2. On miss: build the content, then `put(key, source_dir, desc)`
/// 3. `prune(max_entries, max_bytes)` — evict old entries to stay within limits
///
/// # Thread Safety
///
/// Implementations must be `Send + Sync`. Concurrent `get`/`put` calls
/// with different keys must be safe. Concurrent calls with the same key
/// have implementation-defined behavior (last writer wins is acceptable).
pub trait CacheBackend: Send + Sync {
    /// Retrieve a cached directory by key.
    ///
    /// Returns `Some(path)` if the key exists and the cached content is valid.
    /// The returned path points to a directory that the caller can read from.
    /// Returns `None` on cache miss.
    ///
    /// Implementations should update the last-accessed timestamp on hit.
    fn get(&self, key: &str) -> Result<Option<PathBuf>>;

    /// Store a directory tree in the cache under the given key.
    ///
    /// Copies (or moves) the contents of `source_dir` into the cache.
    /// If an entry with this key already exists, it is replaced.
    ///
    /// Returns the path to the cached directory (which may differ from `source_dir`).
    fn put(&self, key: &str, source_dir: &Path, description: &str) -> Result<PathBuf>;

    /// Remove a cached entry by key.
    ///
    /// No-op if the key does not exist.
    fn invalidate(&self, key: &str) -> Result<()>;

    /// Evict entries to satisfy the given constraints.
    ///
    /// Implementations should evict least-recently-accessed entries first.
    /// Returns the number of entries evicted.
    fn prune(&self, max_entries: usize, max_bytes: u64) -> Result<usize>;

    /// List all cache entries with their metadata.
    fn list(&self) -> Result<Vec<CacheEntry>>;

    /// Get aggregate cache statistics.
    fn stats(&self) -> Result<CacheStats> {
        let entries = self.list()?;
        Ok(CacheStats {
            entry_count: entries.len(),
            total_bytes: entries.iter().map(|e| e.size_bytes).sum(),
        })
    }
}
