//! Layer-level build cache for the image build engine.
//!
//! Implements a Docker/BuildKit-style cache keyed on a running "chain key":
//! a rolling hash of every instruction (and, for COPY/ADD, the content of the
//! source files) executed so far. When a layer-producing instruction
//! (RUN/COPY/ADD) is reached with a chain key that has been seen before, the
//! previously produced layer is reused instead of re-executing the instruction.
//!
//! The cache lives under `~/.a3s/buildcache`:
//! - `blobs/<digest>`  — the cached layer tar.gz, content-addressed.
//! - `keys/<chain-key>` — a small JSON record `{digest, diff_id, size}` pointing
//!   at the blob.
//!
//! All cache I/O is best-effort: any failure leaves the build uncached but does
//! NOT fail the build.

use std::path::{Path, PathBuf};

use a3s_box_core::dirs_home;
use serde::{Deserialize, Serialize};

use super::layer::{sha256_bytes, LayerInfo};

/// Default cap on the total size of cached layer blobs (2 GiB). Override with
/// `A3S_BOX_BUILDCACHE_MAX_BYTES`. When the cap is exceeded after a store, the
/// oldest blobs are evicted (FIFO) until the total is back under the cap; a
/// later build that needs an evicted layer simply re-runs the instruction.
const DEFAULT_MAX_BYTES: u64 = 2 * 1024 * 1024 * 1024;

fn configured_max_bytes() -> u64 {
    std::env::var("A3S_BOX_BUILDCACHE_MAX_BYTES")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(DEFAULT_MAX_BYTES)
}

/// On-disk record stored at `keys/<chain-key>`.
#[derive(Debug, Serialize, Deserialize)]
struct KeyRecord {
    digest: String,
    diff_id: String,
    size: u64,
}

/// A cache hit: a previously produced layer that can be reused.
pub(crate) struct CachedLayer {
    /// Path to the cached layer tar.gz blob.
    pub(crate) blob_path: PathBuf,
    /// SHA256 digest (hex, no prefix) of the compressed layer.
    pub(crate) digest: String,
    /// diff_id (SHA256 of the uncompressed content).
    pub(crate) diff_id: String,
    /// Size in bytes of the compressed layer.
    pub(crate) size: u64,
}

/// Layer-level build cache rooted at `~/.a3s/buildcache`.
pub(crate) struct BuildCache {
    dir: PathBuf,
}

impl BuildCache {
    /// Open the build cache, creating its directory layout if needed.
    ///
    /// Returns `None` (so the build proceeds uncached) if the directories
    /// cannot be created.
    pub(crate) fn open() -> Option<Self> {
        Self::open_in(dirs_home().join("buildcache"))
    }

    /// Open a build cache rooted at an explicit directory.
    fn open_in(dir: PathBuf) -> Option<Self> {
        std::fs::create_dir_all(dir.join("blobs")).ok()?;
        std::fs::create_dir_all(dir.join("keys")).ok()?;
        Some(Self { dir })
    }

    /// Compute the next chain key from the previous key, the canonical
    /// instruction representation, and an optional input hash.
    ///
    /// The key is `sha256(prev_key + "\n" + instruction_repr + ("\n" + input_hash)?)`.
    /// This makes the key order-sensitive and sensitive to every instruction
    /// (including config-only ones like ENV/WORKDIR, which affect later RUNs).
    pub(crate) fn chain(
        prev_key: &str,
        instruction_repr: &str,
        input_hash: Option<&str>,
    ) -> String {
        let mut buf = String::with_capacity(prev_key.len() + instruction_repr.len() + 1);
        buf.push_str(prev_key);
        buf.push('\n');
        buf.push_str(instruction_repr);
        if let Some(h) = input_hash {
            buf.push('\n');
            buf.push_str(h);
        }
        sha256_bytes(buf.as_bytes())
    }

    /// Look up a cached layer by chain key.
    ///
    /// Returns the cached layer only if its key record exists AND the
    /// referenced blob is still present on disk.
    pub(crate) fn lookup(&self, key: &str) -> Option<CachedLayer> {
        let key_path = self.dir.join("keys").join(key);
        let bytes = std::fs::read(&key_path).ok()?;
        let record: KeyRecord = serde_json::from_slice(&bytes).ok()?;

        let blob_path = self.dir.join("blobs").join(&record.digest);
        if !blob_path.exists() {
            return None;
        }

        Some(CachedLayer {
            blob_path,
            digest: record.digest,
            diff_id: record.diff_id,
            size: record.size,
        })
    }

    /// Store a produced layer under the given chain key.
    ///
    /// Copies `layer.path` to `blobs/<layer.digest>` (only if absent) and writes
    /// the `keys/<key>` record. Best-effort: I/O errors are ignored.
    pub(crate) fn store(&self, key: &str, layer: &LayerInfo, diff_id: &str) {
        let blob_path = self.dir.join("blobs").join(&layer.digest);
        if !blob_path.exists() && std::fs::copy(&layer.path, &blob_path).is_err() {
            return;
        }

        let record = KeyRecord {
            digest: layer.digest.clone(),
            diff_id: diff_id.to_string(),
            size: layer.size,
        };
        if let Ok(bytes) = serde_json::to_vec(&record) {
            let _ = std::fs::write(self.dir.join("keys").join(key), bytes);
        }

        self.prune_to(configured_max_bytes());
    }

    /// Evict oldest layer blobs (by modification time) until the total blob
    /// size is at or below `cap` bytes. Best-effort; key records that point at
    /// an evicted blob simply miss on the next lookup (and the instruction is
    /// re-run), so eviction can never corrupt a build.
    fn prune_to(&self, cap: u64) {
        let blobs_dir = self.dir.join("blobs");
        let Ok(read_dir) = std::fs::read_dir(&blobs_dir) else {
            return;
        };

        let mut blobs: Vec<(std::time::SystemTime, u64, PathBuf)> = Vec::new();
        let mut total: u64 = 0;
        for entry in read_dir.flatten() {
            let Ok(meta) = entry.metadata() else { continue };
            if !meta.is_file() {
                continue;
            }
            let len = meta.len();
            total += len;
            let mtime = meta.modified().unwrap_or(std::time::UNIX_EPOCH);
            blobs.push((mtime, len, entry.path()));
        }

        if total <= cap {
            return;
        }

        blobs.sort_by_key(|(mtime, _, _)| *mtime); // oldest first
        for (_, len, path) in blobs {
            if total <= cap {
                break;
            }
            if std::fs::remove_file(&path).is_ok() {
                total = total.saturating_sub(len);
            }
        }
    }
}

/// Hash the content of COPY/ADD source files for cache invalidation.
///
/// Each `src` pattern is resolved under `context_dir`. Files are hashed
/// recursively for directories, in a deterministic (sorted by relative path)
/// order; for each file the hash absorbs `relpath + len + bytes`. A changed
/// file therefore changes the resulting hash and invalidates the cache.
///
/// Returns `None` if any source cannot be read (so the caller treats it as a
/// cache miss and falls back to executing the instruction).
pub(crate) fn hash_context_sources(context_dir: &Path, src_patterns: &[String]) -> Option<String> {
    use sha2::{Digest, Sha256};

    let mut files: Vec<(PathBuf, PathBuf)> = Vec::new();
    for src in src_patterns {
        // Match handle_copy/handle_add: a leading slash is context-relative, not
        // a host absolute path (which `Path::join` would otherwise jump to).
        let src_path = context_dir.join(src.trim_start_matches('/'));
        if !src_path.exists() {
            return None;
        }
        if src_path.is_dir() {
            collect_files(&src_path, &src_path, &mut files)?;
        } else {
            let rel = PathBuf::from(src);
            files.push((rel, src_path));
        }
    }

    // Deterministic order regardless of filesystem traversal order.
    files.sort_by(|a, b| a.0.cmp(&b.0));

    let mut hasher = Sha256::new();
    for (rel, full) in &files {
        let bytes = std::fs::read(full).ok()?;
        hasher.update(rel.to_string_lossy().as_bytes());
        hasher.update(b"\0");
        hasher.update((bytes.len() as u64).to_le_bytes());
        hasher.update(&bytes);
    }
    Some(hex::encode(hasher.finalize()))
}

/// Recursively collect `(relative_path, full_path)` pairs for files under `root`.
fn collect_files(root: &Path, current: &Path, out: &mut Vec<(PathBuf, PathBuf)>) -> Option<()> {
    for entry in std::fs::read_dir(current).ok()? {
        let entry = entry.ok()?;
        let path = entry.path();
        if path.is_dir() {
            collect_files(root, &path, out)?;
        } else {
            let rel = path.strip_prefix(root).ok()?.to_path_buf();
            out.push((rel, path));
        }
    }
    Some(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    /// Test constructor: open a cache at an explicit directory.
    fn open_at(dir: &Path) -> BuildCache {
        BuildCache::open_in(dir.to_path_buf()).expect("open build cache at temp dir")
    }

    #[test]
    fn test_chain_is_deterministic() {
        let a = BuildCache::chain("prev", "RUN echo hi", None);
        let b = BuildCache::chain("prev", "RUN echo hi", None);
        assert_eq!(a, b);
        assert_eq!(a.len(), 64); // SHA256 hex
    }

    #[test]
    fn test_chain_is_order_sensitive() {
        // Different repr -> different key.
        let a = BuildCache::chain("prev", "RUN echo a", None);
        let b = BuildCache::chain("prev", "RUN echo b", None);
        assert_ne!(a, b);

        // Different prev key -> different key (order of instructions matters).
        let c = BuildCache::chain("prev1", "RUN echo a", None);
        let d = BuildCache::chain("prev2", "RUN echo a", None);
        assert_ne!(c, d);
    }

    #[test]
    fn test_chain_input_hash_changes_key() {
        let none = BuildCache::chain("prev", "COPY . /app", None);
        let some = BuildCache::chain("prev", "COPY . /app", Some("deadbeef"));
        assert_ne!(none, some);

        let other = BuildCache::chain("prev", "COPY . /app", Some("cafebabe"));
        assert_ne!(some, other);
    }

    #[test]
    fn test_store_then_lookup_round_trips() {
        let tmp = TempDir::new().unwrap();
        let cache_dir = tmp.path().join("buildcache");
        let cache = open_at(&cache_dir);

        // Create a fake layer blob to be cached.
        let layer_path = tmp.path().join("layer.tar.gz");
        fs::write(&layer_path, b"fake layer contents").unwrap();
        let layer = LayerInfo {
            path: layer_path,
            digest: "abc123def456".to_string(),
            size: 19,
        };

        let key = BuildCache::chain("", "RUN echo hi", None);
        assert!(cache.lookup(&key).is_none());

        cache.store(&key, &layer, "diff-id-xyz");

        let hit = cache
            .lookup(&key)
            .expect("expected a cache hit after store");
        assert_eq!(hit.digest, "abc123def456");
        assert_eq!(hit.diff_id, "diff-id-xyz");
        assert_eq!(hit.size, 19);
        assert!(hit.blob_path.exists());
        assert_eq!(fs::read(&hit.blob_path).unwrap(), b"fake layer contents");
    }

    #[test]
    fn test_lookup_misses_when_blob_removed() {
        let tmp = TempDir::new().unwrap();
        let cache = open_at(&tmp.path().join("buildcache"));

        let layer_path = tmp.path().join("layer.tar.gz");
        fs::write(&layer_path, b"data").unwrap();
        let layer = LayerInfo {
            path: layer_path,
            digest: "deadbeef".to_string(),
            size: 4,
        };
        let key = BuildCache::chain("", "RUN x", None);
        cache.store(&key, &layer, "diff");

        // Remove the blob; the key record remains but lookup must miss.
        fs::remove_file(tmp.path().join("buildcache/blobs/deadbeef")).unwrap();
        assert!(cache.lookup(&key).is_none());
    }

    #[test]
    fn test_hash_context_sources_detects_change() {
        let ctx = TempDir::new().unwrap();
        fs::write(ctx.path().join("a.txt"), "hello").unwrap();
        fs::create_dir(ctx.path().join("sub")).unwrap();
        fs::write(ctx.path().join("sub/b.txt"), "world").unwrap();

        let srcs = vec![".".to_string()];
        let h1 = hash_context_sources(ctx.path(), &srcs).unwrap();
        let h2 = hash_context_sources(ctx.path(), &srcs).unwrap();
        assert_eq!(h1, h2, "stable hash for unchanged content");

        // Change a file -> different hash.
        fs::write(ctx.path().join("a.txt"), "HELLO").unwrap();
        let h3 = hash_context_sources(ctx.path(), &srcs).unwrap();
        assert_ne!(h1, h3, "changed content must change the hash");
    }

    #[test]
    fn test_prune_evicts_until_under_cap() {
        let tmp = TempDir::new().unwrap();
        let cache = open_at(&tmp.path().join("buildcache"));

        // Store three ~100-byte blobs under distinct keys/digests.
        let payload = vec![b'x'; 100];
        for i in 0..3 {
            let src = tmp.path().join(format!("src{i}"));
            fs::write(&src, &payload).unwrap();
            let layer = LayerInfo {
                path: src,
                digest: format!("digest{i:040}"),
                size: payload.len() as u64,
            };
            cache.store(
                &BuildCache::chain("", &format!("RUN step {i}"), None),
                &layer,
                "d",
            );
        }

        let blobs_dir = tmp.path().join("buildcache/blobs");
        let total = |dir: &Path| -> u64 {
            fs::read_dir(dir)
                .unwrap()
                .flatten()
                .map(|e| e.metadata().unwrap().len())
                .sum()
        };
        assert_eq!(total(&blobs_dir), 300, "three blobs stored");

        // Cap at 150 bytes: prune must leave the total at or below the cap.
        cache.prune_to(150);
        assert!(
            total(&blobs_dir) <= 150,
            "prune must bring total under the cap"
        );
        assert!(total(&blobs_dir) > 0, "prune must keep what fits");
    }

    #[test]
    fn test_hash_context_sources_missing_source_is_none() {
        let ctx = TempDir::new().unwrap();
        let srcs = vec!["does-not-exist".to_string()];
        assert!(hash_context_sources(ctx.path(), &srcs).is_none());
    }
}
