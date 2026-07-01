//! `a3s-box diff` command — Show filesystem changes in a box.
//!
//! Compares the box's rootfs against the original image layers to detect
//! added, changed, and deleted files, similar to `docker diff`.

use std::collections::HashMap;
use std::path::Path;

use clap::Args;

use crate::resolve;
use crate::state::StateFile;

/// Change type for a filesystem entry.
#[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
enum ChangeKind {
    Added,
    Changed,
    Deleted,
}

impl std::fmt::Display for ChangeKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ChangeKind::Added => write!(f, "A"),
            ChangeKind::Changed => write!(f, "C"),
            ChangeKind::Deleted => write!(f, "D"),
        }
    }
}

#[derive(Args)]
pub struct DiffArgs {
    /// Box name or ID
    pub name: String,
}

pub async fn execute(args: DiffArgs) -> Result<(), Box<dyn std::error::Error>> {
    let state = StateFile::load_default()?;
    let record = resolve::resolve(&state, &args.name)?;

    let rootfs_dir = super::resolve_box_rootfs(&record.box_dir).ok_or_else(|| {
        format!(
            "Rootfs not found for box '{}' under {} (looked for merged/ and rootfs/)",
            args.name,
            record.box_dir.display()
        )
    })?;

    // Snapshot the original image to compare against
    let snapshot_path = record.box_dir.join("rootfs_snapshot.json");
    if !snapshot_path.exists() {
        println!("No baseline snapshot found — cannot compute diff.");
        println!("(Snapshot is created at box creation time.)");
        return Ok(());
    }

    let snapshot_data = std::fs::read_to_string(&snapshot_path)
        .map_err(|e| format!("Failed to read snapshot: {e}"))?;
    let baseline: HashMap<String, FileInfo> = serde_json::from_str(&snapshot_data)
        .map_err(|e| format!("Failed to parse snapshot: {e}"))?;

    // Walk current rootfs
    let current = walk_dir(&rootfs_dir)?;

    // Compute diff
    let mut changes = Vec::new();

    // Check for added and changed files
    for (path, info) in &current {
        match baseline.get(path) {
            None => changes.push((ChangeKind::Added, path.clone())),
            Some(base_info) => {
                if info.size != base_info.size || info.mode != base_info.mode {
                    changes.push((ChangeKind::Changed, path.clone()));
                }
            }
        }
    }

    // Check for deleted files
    for path in baseline.keys() {
        if !current.contains_key(path) {
            changes.push((ChangeKind::Deleted, path.clone()));
        }
    }

    changes.sort_by(|a, b| a.1.cmp(&b.1));

    if changes.is_empty() {
        println!("No changes detected.");
    } else {
        for (kind, path) in &changes {
            println!("{kind} {path}");
        }
    }

    Ok(())
}

/// Create the per-box baseline snapshot used by `a3s-box diff`.
///
/// The caller should invoke this after the rootfs is prepared and before user
/// mutations that should appear in later diff output.
pub(crate) fn create_box_baseline_snapshot(
    box_dir: &Path,
) -> Result<(), Box<dyn std::error::Error>> {
    let snapshot_path = box_dir.join("rootfs_snapshot.json");
    if snapshot_path.exists() {
        return Ok(());
    }
    // Resolve the provider's rootfs: `merged` (overlay) is the freshly-mounted
    // pristine image at boot time; `rootfs` (plain provider) likewise.
    if let Some(rootfs_dir) = super::resolve_box_rootfs(box_dir) {
        create_snapshot(&rootfs_dir, &snapshot_path)?;
    }
    Ok(())
}

/// Minimal file metadata for comparison.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct FileInfo {
    pub size: u64,
    pub mode: u32,
    pub is_dir: bool,
}

/// Walk a directory tree and collect file metadata, keyed by relative path.
pub fn walk_dir(root: &Path) -> Result<HashMap<String, FileInfo>, Box<dyn std::error::Error>> {
    let mut map = HashMap::new();
    walk_recursive(root, root, &mut map)?;
    Ok(map)
}

fn walk_recursive(
    root: &Path,
    current: &Path,
    map: &mut HashMap<String, FileInfo>,
) -> Result<(), Box<dyn std::error::Error>> {
    let entries = match std::fs::read_dir(current) {
        Ok(e) => e,
        Err(_) => return Ok(()),
    };

    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        let rel = path
            .strip_prefix(root)
            .map(|p| format!("/{}", p.display()))
            .unwrap_or_default();

        let meta = match entry.metadata() {
            Ok(m) => m,
            Err(_) => continue,
        };

        #[cfg(unix)]
        let mode = {
            use std::os::unix::fs::MetadataExt;
            meta.mode()
        };
        #[cfg(not(unix))]
        let mode = 0u32;

        map.insert(
            rel,
            FileInfo {
                size: meta.len(),
                mode,
                is_dir: meta.is_dir(),
            },
        );

        if meta.is_dir() {
            walk_recursive(root, &path, map)?;
        }
    }

    Ok(())
}

/// Create a baseline snapshot of a rootfs directory.
///
/// Called at box creation time to record the initial filesystem state.
pub fn create_snapshot(
    rootfs_dir: &Path,
    snapshot_path: &Path,
) -> Result<(), Box<dyn std::error::Error>> {
    let map = walk_dir(rootfs_dir)?;
    let json = serde_json::to_string(&map)?;
    std::fs::write(snapshot_path, json)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_walk_dir_empty() {
        let dir = tempfile::tempdir().unwrap();
        let map = walk_dir(dir.path()).unwrap();
        assert!(map.is_empty());
    }

    #[test]
    fn test_walk_dir_with_files() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("hello.txt"), "world").unwrap();
        std::fs::create_dir(dir.path().join("subdir")).unwrap();
        std::fs::write(dir.path().join("subdir").join("nested.txt"), "data").unwrap();

        let map = walk_dir(dir.path()).unwrap();
        assert!(map.contains_key("/hello.txt"));
        assert!(map.contains_key("/subdir"));
        assert!(map.contains_key("/subdir/nested.txt"));
        assert_eq!(map["/hello.txt"].size, 5);
        assert!(!map["/hello.txt"].is_dir);
        assert!(map["/subdir"].is_dir);
    }

    #[test]
    fn test_create_snapshot_and_diff() {
        let dir = tempfile::tempdir().unwrap();
        let rootfs = dir.path().join("rootfs");
        std::fs::create_dir(&rootfs).unwrap();
        std::fs::write(rootfs.join("file1.txt"), "hello").unwrap();

        // Create snapshot
        let snap = dir.path().join("snapshot.json");
        create_snapshot(&rootfs, &snap).unwrap();

        // Parse it back
        let data = std::fs::read_to_string(&snap).unwrap();
        let baseline: HashMap<String, FileInfo> = serde_json::from_str(&data).unwrap();
        assert!(baseline.contains_key("/file1.txt"));
    }

    #[test]
    fn test_change_kind_display() {
        assert_eq!(format!("{}", ChangeKind::Added), "A");
        assert_eq!(format!("{}", ChangeKind::Changed), "C");
        assert_eq!(format!("{}", ChangeKind::Deleted), "D");
    }

    #[test]
    fn test_diff_detects_added() {
        let dir = tempfile::tempdir().unwrap();
        let rootfs = dir.path().join("rootfs");
        std::fs::create_dir(&rootfs).unwrap();
        std::fs::write(rootfs.join("original.txt"), "data").unwrap();

        let snap = dir.path().join("snapshot.json");
        create_snapshot(&rootfs, &snap).unwrap();

        // Add a new file
        std::fs::write(rootfs.join("new.txt"), "added").unwrap();

        let data = std::fs::read_to_string(&snap).unwrap();
        let baseline: HashMap<String, FileInfo> = serde_json::from_str(&data).unwrap();
        let current = walk_dir(&rootfs).unwrap();

        let mut added = Vec::new();
        for path in current.keys() {
            if !baseline.contains_key(path) {
                added.push(path.clone());
            }
        }
        assert!(added.contains(&"/new.txt".to_string()));
    }

    #[test]
    fn test_diff_detects_deleted() {
        let dir = tempfile::tempdir().unwrap();
        let rootfs = dir.path().join("rootfs");
        std::fs::create_dir(&rootfs).unwrap();
        std::fs::write(rootfs.join("to_delete.txt"), "data").unwrap();

        let snap = dir.path().join("snapshot.json");
        create_snapshot(&rootfs, &snap).unwrap();

        // Delete the file
        std::fs::remove_file(rootfs.join("to_delete.txt")).unwrap();

        let data = std::fs::read_to_string(&snap).unwrap();
        let baseline: HashMap<String, FileInfo> = serde_json::from_str(&data).unwrap();
        let current = walk_dir(&rootfs).unwrap();

        let mut deleted = Vec::new();
        for path in baseline.keys() {
            if !current.contains_key(path) {
                deleted.push(path.clone());
            }
        }
        assert!(deleted.contains(&"/to_delete.txt".to_string()));
    }

    #[test]
    fn test_diff_detects_changed() {
        let dir = tempfile::tempdir().unwrap();
        let rootfs = dir.path().join("rootfs");
        std::fs::create_dir(&rootfs).unwrap();
        std::fs::write(rootfs.join("file.txt"), "short").unwrap();

        let snap = dir.path().join("snapshot.json");
        create_snapshot(&rootfs, &snap).unwrap();

        // Modify the file (different size)
        std::fs::write(rootfs.join("file.txt"), "much longer content").unwrap();

        let data = std::fs::read_to_string(&snap).unwrap();
        let baseline: HashMap<String, FileInfo> = serde_json::from_str(&data).unwrap();
        let current = walk_dir(&rootfs).unwrap();

        let mut changed = Vec::new();
        for (path, info) in &current {
            if let Some(base) = baseline.get(path) {
                if info.size != base.size {
                    changed.push(path.clone());
                }
            }
        }
        assert!(changed.contains(&"/file.txt".to_string()));
    }
}
