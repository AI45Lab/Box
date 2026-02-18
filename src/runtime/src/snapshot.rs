//! VM Snapshot Store — Save and restore VM configuration snapshots.
//!
//! Snapshots are stored as directories under `~/.a3s/snapshots/<id>/`:
//! - `metadata.json` — SnapshotMetadata (config, resources, env, etc.)
//! - `rootfs/` — Copy of the box's rootfs (or symlink to cache)
//!
//! Restore creates a new box from the saved configuration, leveraging
//! rootfs caching for sub-500ms cold start.

use std::path::{Path, PathBuf};

use a3s_box_core::error::{BoxError, Result};
use a3s_box_core::snapshot::SnapshotMetadata;
use a3s_box_core::SnapshotStoreBackend;

/// Persistent store for VM snapshots.
pub struct SnapshotStore {
    /// Root directory for all snapshots
    base_dir: PathBuf,
}

impl SnapshotStore {
    /// Create a new snapshot store at the given directory.
    pub fn new(base_dir: &Path) -> Result<Self> {
        std::fs::create_dir_all(base_dir).map_err(|e| {
            BoxError::Other(format!(
                "Failed to create snapshot directory {}: {}",
                base_dir.display(),
                e
            ))
        })?;
        Ok(Self {
            base_dir: base_dir.to_path_buf(),
        })
    }

    /// Open the default snapshot store at `~/.a3s/snapshots`.
    pub fn default_path() -> Result<Self> {
        let home = dirs::home_dir()
            .map(|h| h.join(".a3s"))
            .unwrap_or_else(|| PathBuf::from(".a3s"));
        Self::new(&home.join("snapshots"))
    }

    /// Save a snapshot with the given metadata and rootfs source.
    ///
    /// Copies the rootfs directory into the snapshot bundle.
    /// Returns the updated metadata with `size_bytes` populated.
    pub fn save(
        &self,
        mut metadata: SnapshotMetadata,
        rootfs_source: &Path,
    ) -> Result<SnapshotMetadata> {
        let snap_dir = self.base_dir.join(&metadata.id);
        if snap_dir.exists() {
            return Err(BoxError::Other(format!(
                "Snapshot '{}' already exists",
                metadata.id
            )));
        }

        std::fs::create_dir_all(&snap_dir).map_err(|e| {
            BoxError::Other(format!(
                "Failed to create snapshot directory {}: {}",
                snap_dir.display(),
                e
            ))
        })?;

        // Copy rootfs if source exists
        let rootfs_dest = snap_dir.join("rootfs");
        if rootfs_source.exists() {
            copy_dir_recursive(rootfs_source, &rootfs_dest)?;
        } else {
            std::fs::create_dir_all(&rootfs_dest).map_err(|e| {
                BoxError::Other(format!("Failed to create snapshot rootfs directory: {}", e))
            })?;
        }

        // Calculate size
        metadata.size_bytes = dir_size(&snap_dir);

        // Write metadata
        let meta_path = snap_dir.join("metadata.json");
        let json = serde_json::to_string_pretty(&metadata).map_err(|e| {
            BoxError::Other(format!("Failed to serialize snapshot metadata: {}", e))
        })?;
        std::fs::write(&meta_path, &json).map_err(|e| {
            BoxError::Other(format!(
                "Failed to write snapshot metadata {}: {}",
                meta_path.display(),
                e
            ))
        })?;

        Ok(metadata)
    }

    /// Load snapshot metadata by ID.
    pub fn get(&self, id: &str) -> Result<Option<SnapshotMetadata>> {
        let meta_path = self.base_dir.join(id).join("metadata.json");
        if !meta_path.exists() {
            return Ok(None);
        }

        let data = std::fs::read_to_string(&meta_path).map_err(|e| {
            BoxError::Other(format!(
                "Failed to read snapshot metadata {}: {}",
                meta_path.display(),
                e
            ))
        })?;
        let metadata: SnapshotMetadata = serde_json::from_str(&data)
            .map_err(|e| BoxError::Other(format!("Failed to parse snapshot metadata: {}", e)))?;
        Ok(Some(metadata))
    }

    /// Get the rootfs path for a snapshot.
    pub fn rootfs_path(&self, id: &str) -> PathBuf {
        self.base_dir.join(id).join("rootfs")
    }

    /// List all snapshots, sorted by creation time (newest first).
    pub fn list(&self) -> Result<Vec<SnapshotMetadata>> {
        let mut snapshots = Vec::new();

        let entries = match std::fs::read_dir(&self.base_dir) {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(snapshots),
            Err(e) => {
                return Err(BoxError::Other(format!(
                    "Failed to read snapshot directory: {}",
                    e
                )));
            }
        };

        for entry in entries {
            let entry = entry
                .map_err(|e| BoxError::Other(format!("Failed to read snapshot entry: {}", e)))?;
            let meta_path = entry.path().join("metadata.json");
            if meta_path.exists() {
                if let Ok(data) = std::fs::read_to_string(&meta_path) {
                    if let Ok(meta) = serde_json::from_str::<SnapshotMetadata>(&data) {
                        snapshots.push(meta);
                    }
                }
            }
        }

        // Sort newest first
        snapshots.sort_by(|a, b| b.created_at.cmp(&a.created_at));
        Ok(snapshots)
    }

    /// Delete a snapshot by ID.
    pub fn delete(&self, id: &str) -> Result<bool> {
        let snap_dir = self.base_dir.join(id);
        if !snap_dir.exists() {
            return Ok(false);
        }

        std::fs::remove_dir_all(&snap_dir)
            .map_err(|e| BoxError::Other(format!("Failed to delete snapshot {}: {}", id, e)))?;
        Ok(true)
    }

    /// Count the number of snapshots.
    pub fn count(&self) -> Result<usize> {
        Ok(self.list()?.len())
    }

    /// Calculate total size of all snapshots in bytes.
    pub fn total_size(&self) -> Result<u64> {
        Ok(self.list()?.iter().map(|s| s.size_bytes).sum())
    }

    /// Prune old snapshots to stay within limits.
    ///
    /// Removes oldest snapshots first until both `max_count` and `max_bytes`
    /// constraints are satisfied. A value of 0 means unlimited.
    pub fn prune(&self, max_count: usize, max_bytes: u64) -> Result<Vec<String>> {
        let mut snapshots = self.list()?;
        let mut removed = Vec::new();

        // Snapshots are sorted newest-first; remove from the end (oldest)
        while !snapshots.is_empty() {
            let over_count = max_count > 0 && snapshots.len() > max_count;
            let total: u64 = snapshots.iter().map(|s| s.size_bytes).sum();
            let over_size = max_bytes > 0 && total > max_bytes;

            if !over_count && !over_size {
                break;
            }

            // Remove the oldest (last in the list)
            if let Some(oldest) = snapshots.pop() {
                self.delete(&oldest.id)?;
                removed.push(oldest.id);
            }
        }

        Ok(removed)
    }
}

/// Recursively copy a directory.
fn copy_dir_recursive(src: &Path, dst: &Path) -> Result<()> {
    std::fs::create_dir_all(dst).map_err(|e| {
        BoxError::Other(format!(
            "Failed to create directory {}: {}",
            dst.display(),
            e
        ))
    })?;

    for entry in std::fs::read_dir(src).map_err(|e| {
        BoxError::Other(format!("Failed to read directory {}: {}", src.display(), e))
    })? {
        let entry =
            entry.map_err(|e| BoxError::Other(format!("Failed to read directory entry: {}", e)))?;
        let src_path = entry.path();
        let dst_path = dst.join(entry.file_name());

        if src_path.is_dir() {
            copy_dir_recursive(&src_path, &dst_path)?;
        } else {
            std::fs::copy(&src_path, &dst_path).map_err(|e| {
                BoxError::Other(format!(
                    "Failed to copy {} → {}: {}",
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
fn dir_size(path: &Path) -> u64 {
    let mut total = 0u64;
    if let Ok(entries) = std::fs::read_dir(path) {
        for entry in entries.flatten() {
            let p = entry.path();
            if p.is_dir() {
                total += dir_size(&p);
            } else if let Ok(meta) = p.metadata() {
                total += meta.len();
            }
        }
    }
    total
}

impl SnapshotStoreBackend for SnapshotStore {
    fn save(&self, metadata: SnapshotMetadata, rootfs_source: &Path) -> Result<SnapshotMetadata> {
        self.save(metadata, rootfs_source)
    }

    fn get(&self, id: &str) -> Result<Option<SnapshotMetadata>> {
        self.get(id)
    }

    fn list(&self) -> Result<Vec<SnapshotMetadata>> {
        self.list()
    }

    fn delete(&self, id: &str) -> Result<bool> {
        self.delete(id)
    }

    fn count(&self) -> Result<usize> {
        self.count()
    }

    fn total_size(&self) -> Result<u64> {
        self.total_size()
    }

    fn prune(&self, max_count: usize, max_bytes: u64) -> Result<Vec<String>> {
        self.prune(max_count, max_bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn make_metadata(id: &str, name: &str) -> SnapshotMetadata {
        SnapshotMetadata::new(
            id.to_string(),
            name.to_string(),
            "box-source".to_string(),
            "alpine:latest".to_string(),
        )
    }

    fn make_rootfs(tmp: &TempDir) -> PathBuf {
        let rootfs = tmp.path().join("rootfs");
        std::fs::create_dir_all(&rootfs).unwrap();
        std::fs::write(rootfs.join("bin.sh"), "#!/bin/sh\necho hello").unwrap();
        std::fs::create_dir_all(rootfs.join("etc")).unwrap();
        std::fs::write(rootfs.join("etc/config"), "key=value").unwrap();
        rootfs
    }

    #[test]
    fn test_snapshot_store_new() {
        let tmp = TempDir::new().unwrap();
        let store = SnapshotStore::new(&tmp.path().join("snapshots")).unwrap();
        assert!(store.base_dir.exists());
    }

    #[test]
    fn test_snapshot_save_and_get() {
        let tmp = TempDir::new().unwrap();
        let store = SnapshotStore::new(&tmp.path().join("snapshots")).unwrap();
        let rootfs = make_rootfs(&tmp);

        let meta = make_metadata("snap-1", "first");
        let saved = store.save(meta, &rootfs).unwrap();

        assert_eq!(saved.id, "snap-1");
        assert!(saved.size_bytes > 0);

        let loaded = store.get("snap-1").unwrap().unwrap();
        assert_eq!(loaded.id, "snap-1");
        assert_eq!(loaded.name, "first");
        assert_eq!(loaded.image, "alpine:latest");
        assert_eq!(loaded.size_bytes, saved.size_bytes);
    }

    #[test]
    fn test_snapshot_get_nonexistent() {
        let tmp = TempDir::new().unwrap();
        let store = SnapshotStore::new(&tmp.path().join("snapshots")).unwrap();
        assert!(store.get("nonexistent").unwrap().is_none());
    }

    #[test]
    fn test_snapshot_save_duplicate_fails() {
        let tmp = TempDir::new().unwrap();
        let store = SnapshotStore::new(&tmp.path().join("snapshots")).unwrap();
        let rootfs = make_rootfs(&tmp);

        let meta = make_metadata("snap-dup", "dup");
        store.save(meta.clone(), &rootfs).unwrap();

        let result = store.save(meta, &rootfs);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("already exists"));
    }

    #[test]
    fn test_snapshot_rootfs_copied() {
        let tmp = TempDir::new().unwrap();
        let store = SnapshotStore::new(&tmp.path().join("snapshots")).unwrap();
        let rootfs = make_rootfs(&tmp);

        let meta = make_metadata("snap-fs", "fs-test");
        store.save(meta, &rootfs).unwrap();

        let snap_rootfs = store.rootfs_path("snap-fs");
        assert!(snap_rootfs.join("bin.sh").exists());
        assert!(snap_rootfs.join("etc/config").exists());
        assert_eq!(
            std::fs::read_to_string(snap_rootfs.join("etc/config")).unwrap(),
            "key=value"
        );
    }

    #[test]
    fn test_snapshot_list_empty() {
        let tmp = TempDir::new().unwrap();
        let store = SnapshotStore::new(&tmp.path().join("snapshots")).unwrap();
        let list = store.list().unwrap();
        assert!(list.is_empty());
    }

    #[test]
    fn test_snapshot_list_multiple() {
        let tmp = TempDir::new().unwrap();
        let store = SnapshotStore::new(&tmp.path().join("snapshots")).unwrap();
        let rootfs = make_rootfs(&tmp);

        for i in 0..3 {
            let meta = make_metadata(&format!("snap-{}", i), &format!("snap-{}", i));
            store.save(meta, &rootfs).unwrap();
            std::thread::sleep(std::time::Duration::from_millis(10));
        }

        let list = store.list().unwrap();
        assert_eq!(list.len(), 3);
        // Newest first
        assert_eq!(list[0].id, "snap-2");
        assert_eq!(list[2].id, "snap-0");
    }

    #[test]
    fn test_snapshot_delete() {
        let tmp = TempDir::new().unwrap();
        let store = SnapshotStore::new(&tmp.path().join("snapshots")).unwrap();
        let rootfs = make_rootfs(&tmp);

        let meta = make_metadata("snap-del", "delete-me");
        store.save(meta, &rootfs).unwrap();

        assert!(store.delete("snap-del").unwrap());
        assert!(store.get("snap-del").unwrap().is_none());
    }

    #[test]
    fn test_snapshot_delete_nonexistent() {
        let tmp = TempDir::new().unwrap();
        let store = SnapshotStore::new(&tmp.path().join("snapshots")).unwrap();
        assert!(!store.delete("nope").unwrap());
    }

    #[test]
    fn test_snapshot_count() {
        let tmp = TempDir::new().unwrap();
        let store = SnapshotStore::new(&tmp.path().join("snapshots")).unwrap();
        let rootfs = make_rootfs(&tmp);

        assert_eq!(store.count().unwrap(), 0);

        store.save(make_metadata("s1", "s1"), &rootfs).unwrap();
        store.save(make_metadata("s2", "s2"), &rootfs).unwrap();
        assert_eq!(store.count().unwrap(), 2);
    }

    #[test]
    fn test_snapshot_total_size() {
        let tmp = TempDir::new().unwrap();
        let store = SnapshotStore::new(&tmp.path().join("snapshots")).unwrap();
        let rootfs = make_rootfs(&tmp);

        store.save(make_metadata("s1", "s1"), &rootfs).unwrap();
        let total = store.total_size().unwrap();
        assert!(total > 0);
    }

    #[test]
    fn test_snapshot_prune_by_count() {
        let tmp = TempDir::new().unwrap();
        let store = SnapshotStore::new(&tmp.path().join("snapshots")).unwrap();
        let rootfs = make_rootfs(&tmp);

        for i in 0..5 {
            let meta = make_metadata(&format!("s{}", i), &format!("s{}", i));
            store.save(meta, &rootfs).unwrap();
            std::thread::sleep(std::time::Duration::from_millis(10));
        }

        let removed = store.prune(3, 0).unwrap();
        assert_eq!(removed.len(), 2);
        assert_eq!(store.count().unwrap(), 3);

        // Oldest should be removed
        assert!(store.get("s0").unwrap().is_none());
        assert!(store.get("s1").unwrap().is_none());
        // Newest should remain
        assert!(store.get("s4").unwrap().is_some());
        assert!(store.get("s3").unwrap().is_some());
        assert!(store.get("s2").unwrap().is_some());
    }

    #[test]
    fn test_snapshot_prune_no_limits() {
        let tmp = TempDir::new().unwrap();
        let store = SnapshotStore::new(&tmp.path().join("snapshots")).unwrap();
        let rootfs = make_rootfs(&tmp);

        for i in 0..3 {
            store
                .save(
                    make_metadata(&format!("s{}", i), &format!("s{}", i)),
                    &rootfs,
                )
                .unwrap();
        }

        let removed = store.prune(0, 0).unwrap();
        assert!(removed.is_empty());
        assert_eq!(store.count().unwrap(), 3);
    }

    #[test]
    fn test_snapshot_save_with_empty_rootfs() {
        let tmp = TempDir::new().unwrap();
        let store = SnapshotStore::new(&tmp.path().join("snapshots")).unwrap();
        let empty_rootfs = tmp.path().join("nonexistent_rootfs");

        let meta = make_metadata("snap-empty", "empty");
        let saved = store.save(meta, &empty_rootfs).unwrap();
        assert_eq!(saved.id, "snap-empty");

        // Rootfs dir should still be created (empty)
        assert!(store.rootfs_path("snap-empty").exists());
    }

    #[test]
    fn test_dir_size() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join("sized");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.txt"), "hello").unwrap();
        std::fs::write(dir.join("b.txt"), "world!").unwrap();

        let size = dir_size(&dir);
        assert_eq!(size, 11); // 5 + 6
    }

    #[test]
    fn test_dir_size_nested() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join("nested");
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        std::fs::write(dir.join("a.txt"), "abc").unwrap();
        std::fs::write(dir.join("sub/b.txt"), "defgh").unwrap();

        let size = dir_size(&dir);
        assert_eq!(size, 8); // 3 + 5
    }

    #[test]
    fn test_dir_size_empty() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join("empty");
        std::fs::create_dir_all(&dir).unwrap();
        assert_eq!(dir_size(&dir), 0);
    }

    #[test]
    fn test_copy_dir_recursive() {
        let tmp = TempDir::new().unwrap();
        let src = tmp.path().join("src");
        let dst = tmp.path().join("dst");

        std::fs::create_dir_all(src.join("sub")).unwrap();
        std::fs::write(src.join("a.txt"), "hello").unwrap();
        std::fs::write(src.join("sub/b.txt"), "world").unwrap();

        copy_dir_recursive(&src, &dst).unwrap();

        assert!(dst.join("a.txt").exists());
        assert!(dst.join("sub/b.txt").exists());
        assert_eq!(std::fs::read_to_string(dst.join("a.txt")).unwrap(), "hello");
        assert_eq!(
            std::fs::read_to_string(dst.join("sub/b.txt")).unwrap(),
            "world"
        );
    }
}
