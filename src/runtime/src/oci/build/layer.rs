//! Layer creation utilities for image building.
//!
//! Provides filesystem snapshotting, diffing, and tar.gz layer creation
//! for producing OCI image layers from build steps.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use a3s_box_core::error::{BoxError, Result};
use sha2::{Digest, Sha256};

/// Metadata for a single file in a snapshot.
#[derive(Debug, Clone, PartialEq)]
pub struct FileEntry {
    /// Relative path from rootfs root
    pub path: PathBuf,
    /// File size in bytes
    pub size: u64,
    /// Modification time (seconds since epoch)
    pub mtime: i64,
    /// Unix permission/mode bits. A `chmod` (e.g. `RUN chmod +x`) changes only
    /// this — not size or mtime — so it must be part of the change check or the
    /// new mode is silently dropped from the layer.
    pub mode: u32,
    /// Whether this is a directory
    pub is_dir: bool,
}

/// A snapshot of a directory's file state.
#[derive(Debug, Clone)]
pub struct DirSnapshot {
    /// Map of relative path → file entry
    pub entries: HashMap<PathBuf, FileEntry>,
}

impl DirSnapshot {
    /// Take a snapshot of a directory, recording all files and their metadata.
    pub fn capture(root: &Path) -> Result<Self> {
        let mut entries = HashMap::new();
        walk_dir(root, root, &mut entries)?;
        Ok(DirSnapshot { entries })
    }

    /// Compute the diff between this snapshot (before) and another (after).
    ///
    /// Returns paths of files that were added or modified.
    pub fn diff(&self, after: &DirSnapshot) -> Vec<PathBuf> {
        let mut changed = Vec::new();

        for (path, after_entry) in &after.entries {
            match self.entries.get(path) {
                None => {
                    // New file
                    changed.push(path.clone());
                }
                Some(before_entry) => {
                    // Modified: size, mtime, or mode changed. Mode matters for
                    // `RUN chmod` (e.g. making a COPY'd script executable), which
                    // touches neither size nor mtime.
                    if before_entry.size != after_entry.size
                        || before_entry.mtime != after_entry.mtime
                        || before_entry.mode != after_entry.mode
                    {
                        changed.push(path.clone());
                    }
                }
            }
        }

        // Sort for deterministic output
        changed.sort();
        changed
    }
}

/// Recursively walk a directory and collect file entries.
fn walk_dir(root: &Path, current: &Path, entries: &mut HashMap<PathBuf, FileEntry>) -> Result<()> {
    let read_dir = std::fs::read_dir(current).map_err(|e| {
        BoxError::BuildError(format!(
            "Failed to read directory {}: {}",
            current.display(),
            e
        ))
    })?;

    for entry in read_dir {
        let entry = entry
            .map_err(|e| BoxError::BuildError(format!("Failed to read directory entry: {}", e)))?;

        let path = entry.path();
        let relative = path
            .strip_prefix(root)
            .map_err(|e| {
                BoxError::BuildError(format!(
                    "Failed to compute relative path for {}: {}",
                    path.display(),
                    e
                ))
            })?
            .to_path_buf();

        let metadata = entry.metadata().map_err(|e| {
            BoxError::BuildError(format!(
                "Failed to read metadata for {}: {}",
                path.display(),
                e
            ))
        })?;

        let mtime = metadata
            .modified()
            .map(|t| {
                t.duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs() as i64
            })
            .unwrap_or(0);

        #[cfg(unix)]
        let mode = {
            use std::os::unix::fs::PermissionsExt;
            metadata.permissions().mode()
        };
        #[cfg(not(unix))]
        let mode = 0u32;

        entries.insert(
            relative.clone(),
            FileEntry {
                path: relative,
                size: metadata.len(),
                mtime,
                mode,
                is_dir: metadata.is_dir(),
            },
        );

        if metadata.is_dir() {
            walk_dir(root, &path, entries)?;
        }
    }

    Ok(())
}

/// Create a tar.gz layer from a list of changed files in a rootfs.
///
/// Returns the path to the created layer file and its SHA256 digest.
/// When `chown` is `Some((uid, gid))`, all tar entry headers are stamped with
/// that uid/gid (regardless of the host filesystem owner), which is how Docker
/// implements `COPY --chown` without requiring elevated permissions.
pub fn create_layer(
    rootfs: &Path,
    changed_files: &[PathBuf],
    output_path: &Path,
) -> Result<LayerInfo> {
    create_layer_with_chown(rootfs, changed_files, output_path, None)
}

/// Internal: `create_layer` with optional uid/gid override for tar headers.
pub(super) fn create_layer_with_chown(
    rootfs: &Path,
    changed_files: &[PathBuf],
    output_path: &Path,
    chown: Option<(u32, u32)>,
) -> Result<LayerInfo> {
    use flate2::write::GzEncoder;
    use flate2::Compression;

    let file = std::fs::File::create(output_path).map_err(|e| {
        BoxError::BuildError(format!(
            "Failed to create layer file {}: {}",
            output_path.display(),
            e
        ))
    })?;

    let encoder = GzEncoder::new(file, Compression::default());
    let mut builder = tar::Builder::new(encoder);
    // Preserve symlinks (e.g. created by `RUN ln -s`) as symlink entries.
    builder.follow_symlinks(false);

    for relative_path in changed_files {
        let full_path = rootfs.join(relative_path);
        // No-follow stat: captures symlinks (incl. dangling ones) without
        // following, so a symlink is added as a symlink, not its target.
        let meta = match std::fs::symlink_metadata(&full_path) {
            Ok(meta) => meta,
            Err(_) => continue,
        };

        if meta.is_dir() {
            append_dir_with_chown(&mut builder, relative_path, &full_path, chown)?;
        } else {
            append_file_with_chown(&mut builder, relative_path, &full_path, &meta, chown)?;
        }
    }

    // Finish the tar archive AND the gzip stream so every byte is flushed to
    // disk before we hash the file. `Builder::finish()` alone only writes the
    // tar trailer into the still-buffered GzEncoder; the gzip data is not
    // flushed to the file until the encoder is dropped/finished. Hashing before
    // that flush would digest an incomplete file (the bug that gave every layer
    // the same digest of the partial 10-byte gzip header).
    let encoder = builder
        .into_inner()
        .map_err(|e| BoxError::BuildError(format!("Failed to finalize layer tar: {}", e)))?;
    encoder
        .finish()
        .map_err(|e| BoxError::BuildError(format!("Failed to finalize layer gzip: {}", e)))?;

    // Compute SHA256 digest of the layer file
    let digest = sha256_file(output_path)?;
    let size = std::fs::metadata(output_path).map(|m| m.len()).unwrap_or(0);

    Ok(LayerInfo {
        path: output_path.to_path_buf(),
        digest,
        size,
    })
}

/// Create a tar.gz layer from an entire directory (used for COPY).
///
/// All files under `src_dir` are added to the layer with paths relative
/// to `target_prefix` (the destination path inside the image).
pub fn create_layer_from_dir(
    src_dir: &Path,
    target_prefix: &Path,
    output_path: &Path,
) -> Result<LayerInfo> {
    create_layer_from_dir_with_chown(src_dir, target_prefix, output_path, None)
}

/// Internal: `create_layer_from_dir` with optional uid/gid override.
pub(super) fn create_layer_from_dir_with_chown(
    src_dir: &Path,
    target_prefix: &Path,
    output_path: &Path,
    chown: Option<(u32, u32)>,
) -> Result<LayerInfo> {
    use flate2::write::GzEncoder;
    use flate2::Compression;

    let file = std::fs::File::create(output_path).map_err(|e| {
        BoxError::BuildError(format!(
            "Failed to create layer file {}: {}",
            output_path.display(),
            e
        ))
    })?;

    let encoder = GzEncoder::new(file, Compression::default());
    let mut builder = tar::Builder::new(encoder);
    // Preserve symlinks as symlink entries instead of copying their targets.
    builder.follow_symlinks(false);

    add_dir_to_tar(&mut builder, src_dir, src_dir, target_prefix, chown)?;

    // Finish the tar AND flush the gzip stream to disk before hashing (see
    // `create_layer` for why hashing before the gzip flush is incorrect).
    let encoder = builder
        .into_inner()
        .map_err(|e| BoxError::BuildError(format!("Failed to finalize layer tar: {}", e)))?;
    encoder
        .finish()
        .map_err(|e| BoxError::BuildError(format!("Failed to finalize layer gzip: {}", e)))?;

    let digest = sha256_file(output_path)?;
    let size = std::fs::metadata(output_path).map(|m| m.len()).unwrap_or(0);

    Ok(LayerInfo {
        path: output_path.to_path_buf(),
        digest,
        size,
    })
}

/// Recursively add a directory's contents to a tar builder.
fn add_dir_to_tar<W: std::io::Write>(
    builder: &mut tar::Builder<W>,
    root: &Path,
    current: &Path,
    target_prefix: &Path,
    chown: Option<(u32, u32)>,
) -> Result<()> {
    let entries = std::fs::read_dir(current).map_err(|e| {
        BoxError::BuildError(format!(
            "Failed to read directory {}: {}",
            current.display(),
            e
        ))
    })?;

    for entry in entries {
        let entry =
            entry.map_err(|e| BoxError::BuildError(format!("Failed to read entry: {}", e)))?;

        let path = entry.path();
        let relative = path
            .strip_prefix(root)
            .map_err(|e| BoxError::BuildError(format!("Failed to strip prefix: {}", e)))?;
        let tar_path = target_prefix.join(relative);

        // No-follow type check: a symlink (even one pointing at a directory)
        // must be added as a symlink entry, not recursed into.
        let file_type = entry
            .file_type()
            .map_err(|e| BoxError::BuildError(format!("Failed to stat entry: {}", e)))?;
        let meta = entry
            .metadata()
            .map_err(|e| BoxError::BuildError(format!("Failed to stat entry: {}", e)))?;

        if file_type.is_dir() {
            append_dir_with_chown(builder, &tar_path, &path, chown)?;
            add_dir_to_tar(builder, root, &path, target_prefix, chown)?;
        } else {
            append_file_with_chown(builder, &tar_path, &path, &meta, chown)?;
        }
    }

    Ok(())
}

/// Append a directory entry, overriding uid/gid when `chown` is set.
fn append_dir_with_chown<W: std::io::Write>(
    builder: &mut tar::Builder<W>,
    tar_path: &Path,
    dir_path: &Path,
    chown: Option<(u32, u32)>,
) -> Result<()> {
    if let Some((uid, gid)) = chown {
        let mut header = tar::Header::new_gnu();
        let meta = std::fs::symlink_metadata(dir_path).map_err(|e| {
            BoxError::BuildError(format!("Failed to stat {}: {}", dir_path.display(), e))
        })?;
        header.set_metadata_in_mode(&meta, tar::HeaderMode::Complete);
        header.set_uid(uid as u64);
        header.set_gid(gid as u64);
        header.set_username("").ok();
        header.set_groupname("").ok();
        header.set_cksum();
        builder
            .append_data(&mut header, tar_path, std::io::empty())
            .map_err(|e| BoxError::BuildError(format!("Failed to add dir to layer: {}", e)))
    } else {
        builder
            .append_dir(tar_path, dir_path)
            .map_err(|e| BoxError::BuildError(format!("Failed to add directory to layer: {}", e)))
    }
}

/// Append a file/symlink entry, overriding uid/gid when `chown` is set.
fn append_file_with_chown<W: std::io::Write>(
    builder: &mut tar::Builder<W>,
    tar_path: &Path,
    file_path: &Path,
    meta: &std::fs::Metadata,
    chown: Option<(u32, u32)>,
) -> Result<()> {
    if let Some((uid, gid)) = chown {
        let mut header = tar::Header::new_gnu();
        header.set_metadata_in_mode(meta, tar::HeaderMode::Complete);
        header.set_uid(uid as u64);
        header.set_gid(gid as u64);
        header.set_username("").ok();
        header.set_groupname("").ok();
        header.set_cksum();
        // For symlinks, the data is empty (target is in link_name header field).
        let body: Box<dyn std::io::Read> = if meta.file_type().is_symlink() {
            Box::new(std::io::empty())
        } else {
            Box::new(std::fs::File::open(file_path).map_err(|e| {
                BoxError::BuildError(format!("Failed to open {}: {}", file_path.display(), e))
            })?)
        };
        builder
            .append_data(&mut header, tar_path, body)
            .map_err(|e| BoxError::BuildError(format!("Failed to add file to layer: {}", e)))
    } else {
        builder
            .append_path_with_name(file_path, tar_path)
            .map_err(|e| BoxError::BuildError(format!("Failed to add file to layer: {}", e)))
    }
}

/// Information about a created layer.
#[derive(Debug, Clone)]
pub struct LayerInfo {
    /// Path to the layer tar.gz file
    pub path: PathBuf,
    /// SHA256 digest (hex string, without "sha256:" prefix)
    pub digest: String,
    /// Size in bytes
    pub size: u64,
}

impl LayerInfo {
    /// Get the digest with "sha256:" prefix.
    pub fn prefixed_digest(&self) -> String {
        format!("sha256:{}", self.digest)
    }
}

/// Compute SHA256 digest of a file by streaming its contents.
pub(super) fn sha256_file(path: &Path) -> Result<String> {
    use sha2::Digest as _;
    use std::io::Read as _;

    let mut file = std::fs::File::open(path).map_err(|e| {
        BoxError::BuildError(format!(
            "Failed to open file for hashing {}: {}",
            path.display(),
            e
        ))
    })?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 65536];
    loop {
        let n = file.read(&mut buf).map_err(|e| {
            BoxError::BuildError(format!(
                "Failed to read file for hashing {}: {}",
                path.display(),
                e
            ))
        })?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hex::encode(hasher.finalize()))
}

/// Compute SHA256 digest of raw bytes.
pub(super) fn sha256_bytes(data: &[u8]) -> String {
    hex::encode(Sha256::digest(data))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    // --- DirSnapshot ---

    #[test]
    fn test_snapshot_empty_dir() {
        let tmp = TempDir::new().unwrap();
        let snap = DirSnapshot::capture(tmp.path()).unwrap();
        assert!(snap.entries.is_empty());
    }

    #[test]
    fn test_snapshot_with_files() {
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("a.txt"), "hello").unwrap();
        fs::create_dir(tmp.path().join("sub")).unwrap();
        fs::write(tmp.path().join("sub").join("b.txt"), "world").unwrap();

        let snap = DirSnapshot::capture(tmp.path()).unwrap();
        assert!(snap.entries.contains_key(&PathBuf::from("a.txt")));
        assert!(snap.entries.contains_key(&PathBuf::from("sub")));
        assert!(snap.entries.contains_key(&PathBuf::from("sub/b.txt")));
    }

    #[test]
    fn test_snapshot_diff_new_file() {
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("a.txt"), "hello").unwrap();

        let before = DirSnapshot::capture(tmp.path()).unwrap();

        fs::write(tmp.path().join("b.txt"), "world").unwrap();

        let after = DirSnapshot::capture(tmp.path()).unwrap();

        let diff = before.diff(&after);
        assert_eq!(diff, vec![PathBuf::from("b.txt")]);
    }

    #[test]
    fn test_snapshot_diff_modified_file() {
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("a.txt"), "hello").unwrap();

        let before = DirSnapshot::capture(tmp.path()).unwrap();

        // Modify the file (change size)
        fs::write(tmp.path().join("a.txt"), "hello world").unwrap();

        let after = DirSnapshot::capture(tmp.path()).unwrap();

        let diff = before.diff(&after);
        assert_eq!(diff, vec![PathBuf::from("a.txt")]);
    }

    /// Regression: a `chmod`-only change (e.g. `RUN chmod +x`) changes the mode
    /// but not size or mtime, and must still be detected so the new mode lands
    /// in the layer (else a COPY'd script stays non-executable -> exec EACCES).
    #[test]
    #[cfg(unix)]
    fn test_snapshot_diff_detects_chmod_only() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = TempDir::new().unwrap();
        let f = tmp.path().join("entry.sh");
        fs::write(&f, "#!/bin/sh\necho hi\n").unwrap();
        fs::set_permissions(&f, fs::Permissions::from_mode(0o644)).unwrap();

        let before = DirSnapshot::capture(tmp.path()).unwrap();
        // chmod +x: mode 0644 -> 0755, same content/size, same mtime.
        fs::set_permissions(&f, fs::Permissions::from_mode(0o755)).unwrap();
        let after = DirSnapshot::capture(tmp.path()).unwrap();

        assert_eq!(before.diff(&after), vec![PathBuf::from("entry.sh")]);
    }

    #[test]
    fn test_snapshot_diff_no_changes() {
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("a.txt"), "hello").unwrap();

        let before = DirSnapshot::capture(tmp.path()).unwrap();
        let after = DirSnapshot::capture(tmp.path()).unwrap();

        let diff = before.diff(&after);
        assert!(diff.is_empty());
    }

    // --- create_layer ---

    #[test]
    fn test_create_layer_from_files() {
        let rootfs = TempDir::new().unwrap();
        let output_dir = TempDir::new().unwrap();

        fs::write(rootfs.path().join("hello.txt"), "hello").unwrap();
        fs::write(rootfs.path().join("world.txt"), "world").unwrap();

        let output_path = output_dir.path().join("layer.tar.gz");
        let changed = vec![PathBuf::from("hello.txt"), PathBuf::from("world.txt")];

        let info = create_layer(rootfs.path(), &changed, &output_path).unwrap();

        assert!(info.path.exists());
        assert!(info.size > 0);
        assert!(!info.digest.is_empty());
        assert_eq!(info.digest.len(), 64); // SHA256 hex
    }

    #[test]
    fn test_create_layer_empty() {
        let rootfs = TempDir::new().unwrap();
        let output_dir = TempDir::new().unwrap();
        let output_path = output_dir.path().join("layer.tar.gz");

        let info = create_layer(rootfs.path(), &[], &output_path).unwrap();
        assert!(info.path.exists());
    }

    /// Regression: the recorded digest/size must reflect the COMPLETE file
    /// (gzip stream fully flushed), not a partially-written one. Previously the
    /// digest was computed before the GzEncoder was finished, so every layer
    /// recorded the same hash of the 10-byte gzip header.
    #[test]
    fn test_create_layer_digest_matches_completed_file() {
        let rootfs = TempDir::new().unwrap();
        let output_dir = TempDir::new().unwrap();
        fs::write(rootfs.path().join("a.txt"), "AAAA-content").unwrap();

        let out = output_dir.path().join("layer.tar.gz");
        let info = create_layer(rootfs.path(), &[PathBuf::from("a.txt")], &out).unwrap();

        // The digest/size recorded must equal the on-disk file, re-read after
        // the function returned (i.e. the file was complete when hashed).
        let on_disk = sha256_file(&info.path).unwrap();
        let on_disk_size = fs::metadata(&info.path).unwrap().len();
        assert_eq!(
            info.digest, on_disk,
            "recorded digest must match completed file"
        );
        assert_eq!(
            info.size, on_disk_size,
            "recorded size must match completed file"
        );
        assert!(
            info.size > 20,
            "a real one-file layer is larger than an empty gzip header"
        );
    }

    /// Regression: single-file layers with different content must produce
    /// different digests (else the content-addressed store/cache collides).
    #[test]
    fn test_create_layer_distinct_content_distinct_digest() {
        let rootfs = TempDir::new().unwrap();
        let out_dir = TempDir::new().unwrap();

        fs::write(rootfs.path().join("a.txt"), "AAAA-content").unwrap();
        let a = create_layer(
            rootfs.path(),
            &[PathBuf::from("a.txt")],
            &out_dir.path().join("a.tgz"),
        )
        .unwrap();

        fs::write(rootfs.path().join("b.txt"), "BBBB-different-longer").unwrap();
        let b = create_layer(
            rootfs.path(),
            &[PathBuf::from("b.txt")],
            &out_dir.path().join("b.tgz"),
        )
        .unwrap();

        assert_ne!(
            a.digest, b.digest,
            "distinct layer content must yield distinct digests"
        );
    }

    // --- create_layer_from_dir ---

    #[test]
    fn test_create_layer_from_dir() {
        let src = TempDir::new().unwrap();
        let output_dir = TempDir::new().unwrap();

        fs::write(src.path().join("app.py"), "print('hi')").unwrap();
        fs::create_dir(src.path().join("lib")).unwrap();
        fs::write(src.path().join("lib").join("util.py"), "pass").unwrap();

        let output_path = output_dir.path().join("layer.tar.gz");
        let info = create_layer_from_dir(src.path(), Path::new("workspace"), &output_path).unwrap();

        assert!(info.path.exists());
        assert!(info.size > 0);

        // Verify the tar contains files under workspace/
        let file = fs::File::open(&info.path).unwrap();
        let decoder = flate2::read::GzDecoder::new(file);
        let mut archive = tar::Archive::new(decoder);
        let paths: Vec<String> = archive
            .entries()
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.path().unwrap().to_string_lossy().to_string())
            .collect();

        assert!(paths.iter().any(|p| p.contains("workspace/app.py")));
        assert!(paths.iter().any(|p| p.contains("workspace/lib")));
    }

    /// Regression: symlinks must be stored as symlink entries (Docker copies
    /// them verbatim), not followed into a duplicate of their target.
    #[test]
    #[cfg(unix)]
    fn test_create_layer_from_dir_preserves_symlinks() {
        let src = TempDir::new().unwrap();
        let output_dir = TempDir::new().unwrap();
        fs::write(src.path().join("libfoo.so.1"), "real").unwrap();
        std::os::unix::fs::symlink("libfoo.so.1", src.path().join("libfoo.so")).unwrap();

        let out = output_dir.path().join("layer.tar.gz");
        create_layer_from_dir(src.path(), Path::new("lib"), &out).unwrap();

        let file = fs::File::open(&out).unwrap();
        let decoder = flate2::read::GzDecoder::new(file);
        let mut archive = tar::Archive::new(decoder);
        let mut found_symlink = false;
        for entry in archive.entries().unwrap() {
            let entry = entry.unwrap();
            if entry.path().unwrap().to_string_lossy() == "lib/libfoo.so" {
                assert_eq!(entry.header().entry_type(), tar::EntryType::Symlink);
                assert_eq!(
                    entry.link_name().unwrap().unwrap().to_string_lossy(),
                    "libfoo.so.1"
                );
                found_symlink = true;
            }
        }
        assert!(found_symlink, "symlink entry must be present in the layer");
    }

    // --- sha256 ---

    #[test]
    fn test_sha256_file() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("test.txt");
        fs::write(&path, "hello").unwrap();

        let digest = sha256_file(&path).unwrap();
        assert_eq!(digest.len(), 64);
        // Known SHA256 of "hello"
        assert_eq!(
            digest,
            "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"
        );
    }

    #[test]
    fn test_sha256_bytes() {
        let digest = sha256_bytes(b"hello");
        assert_eq!(
            digest,
            "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"
        );
    }

    #[test]
    fn test_layer_info_prefixed_digest() {
        let info = LayerInfo {
            path: PathBuf::from("/tmp/layer.tar.gz"),
            digest: "abc123".to_string(),
            size: 100,
        };
        assert_eq!(info.prefixed_digest(), "sha256:abc123");
    }
}
