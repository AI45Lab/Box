//! OCI layer extraction utilities.
//!
//! Handles extraction of OCI image layers (gzip, zstd, or uncompressed tar).

use a3s_box_core::error::{BoxError, Result};
use flate2::read::GzDecoder;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;
use tar::Archive;

/// Extract a single OCI layer (tar.gz) to target directory.
///
/// # Arguments
///
/// * `layer_path` - Path to the layer tarball (*.tar.gz)
/// * `target_dir` - Directory to extract files into
///
/// # Errors
///
/// Returns error if:
/// - Layer file doesn't exist
/// - Decompression fails
/// - Extraction fails
/// - Target directory cannot be created
pub fn extract_layer(layer_path: &Path, target_dir: &Path) -> Result<()> {
    // Validate layer exists
    if !layer_path.exists() {
        return Err(BoxError::OciImageError(format!(
            "Layer file not found: {}",
            layer_path.display()
        )));
    }

    // Create target directory
    std::fs::create_dir_all(target_dir).map_err(|e| {
        BoxError::OciImageError(format!(
            "Failed to create target directory {}: {}",
            target_dir.display(),
            e
        ))
    })?;

    // Open layer file
    let mut file = File::open(layer_path).map_err(|e| {
        BoxError::OciImageError(format!(
            "Failed to open layer file {}: {}",
            layer_path.display(),
            e
        ))
    })?;

    // Detect the layer's compression from its magic bytes — OCI layers are gzip
    // (1f 8b), zstd (28 b5 2f fd, e.g. buildkit/nerdctl `--compression zstd`), or
    // an uncompressed tar. Peek, rewind, then pick the matching decoder; relying
    // on the media type alone would miss layers stored without one.
    let mut magic = [0u8; 4];
    let read = file.read(&mut magic).map_err(|e| {
        BoxError::OciImageError(format!(
            "Failed to read layer header {}: {e}",
            layer_path.display()
        ))
    })?;
    file.seek(SeekFrom::Start(0)).map_err(|e| {
        BoxError::OciImageError(format!(
            "Failed to rewind layer {}: {e}",
            layer_path.display()
        ))
    })?;

    let decoder: Box<dyn Read> = if read >= 2 && magic[0] == 0x1f && magic[1] == 0x8b {
        Box::new(GzDecoder::new(file))
    } else if read >= 4 && magic == [0x28, 0xb5, 0x2f, 0xfd] {
        Box::new(zstd::stream::read::Decoder::new(file).map_err(|e| {
            BoxError::OciImageError(format!(
                "Failed to init zstd decoder for {}: {e}",
                layer_path.display()
            ))
        })?)
    } else {
        // Uncompressed tar (some registries / `--compression none`).
        Box::new(file)
    };

    // Extract the tar archive, applying OCI whiteout semantics so files deleted
    // in an upper layer do not reappear from lower layers:
    //   - `.wh.<name>`    deletes the sibling `<name>` already materialized
    //   - `.wh..wh..opq`  clears all prior contents of its parent directory
    // Whiteout markers themselves are never written into the rootfs. Normal
    // entries are delegated to `unpack_in`, preserving the same symlink /
    // hardlink / permission / mtime fidelity that `unpack` provides.
    let mut archive = Archive::new(decoder);
    archive.set_preserve_permissions(true);
    archive.set_preserve_mtime(true);
    archive.set_overwrite(true);
    #[cfg(unix)]
    {
        archive.set_unpack_xattrs(true);
        // Restore the uid/gid stamped in the layer tar headers so `COPY --chown`
        // ownership (and non-root ownership baked into base-image layers) is
        // preserved in the rootfs instead of collapsing to root. tar performs a
        // chown for this, which only succeeds as root — gate on euid 0 so a
        // non-privileged extraction does not fail with EPERM.
        if unsafe { libc::geteuid() } == 0 {
            archive.set_preserve_ownerships(true);
        }
    }

    let entries = archive
        .entries()
        .map_err(|e| BoxError::OciImageError(format!("Failed to read layer entries: {e}")))?;

    for entry in entries {
        let mut entry = entry
            .map_err(|e| BoxError::OciImageError(format!("Failed to read layer entry: {e}")))?;
        let path = entry
            .path()
            .map_err(|e| BoxError::OciImageError(format!("Invalid layer entry path: {e}")))?
            .into_owned();

        // Defensively reject path-traversal entries (`unpack_in` also guards this).
        if path
            .components()
            .any(|c| matches!(c, std::path::Component::ParentDir))
        {
            tracing::warn!(path = %path.display(), "Skipping layer entry with '..' component");
            continue;
        }

        let file_name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");

        if file_name == ".wh..wh..opq" {
            // Opaque directory marker: discard everything already extracted into
            // the parent directory from lower layers, keeping the directory.
            if let Some(parent) = path.parent() {
                if let Ok(read) = std::fs::read_dir(target_dir.join(parent)) {
                    for child in read.flatten() {
                        remove_path(&child.path());
                    }
                }
            }
            continue;
        }

        if let Some(victim_name) = file_name.strip_prefix(".wh.") {
            // Whiteout marker: remove the named sibling from a lower layer.
            if let Some(parent) = path.parent() {
                remove_path(&target_dir.join(parent).join(victim_name));
            }
            continue;
        }

        entry.unpack_in(target_dir).map_err(|e| {
            BoxError::OciImageError(format!(
                "Failed to extract layer to {}: {}",
                target_dir.display(),
                e
            ))
        })?;
    }

    tracing::debug!(
        layer = %layer_path.display(),
        target = %target_dir.display(),
        "Extracted OCI layer"
    );

    Ok(())
}

/// Remove a file or directory tree for an applied whiteout, ignoring a missing
/// target. Uses `symlink_metadata` so a symlink is removed as a link, not
/// followed into a lower layer.
fn remove_path(path: &Path) {
    let Ok(meta) = std::fs::symlink_metadata(path) else {
        return;
    };
    let result = if meta.is_dir() {
        std::fs::remove_dir_all(path)
    } else {
        std::fs::remove_file(path)
    };
    if let Err(e) = result {
        tracing::warn!(path = %path.display(), error = %e, "Failed to apply whiteout deletion");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn test_extract_layer_creates_target_directory() {
        let temp_dir = TempDir::new().unwrap();
        let layer_path = temp_dir.path().join("layer.tar.gz");
        let target_dir = temp_dir.path().join("extracted");

        // Create a minimal tar.gz file
        create_test_layer(&layer_path, &[("test.txt", b"hello")]);

        // Extract layer
        extract_layer(&layer_path, &target_dir).unwrap();

        // Verify target directory was created
        assert!(target_dir.exists());
        assert!(target_dir.is_dir());
    }

    #[test]
    fn test_extract_layer_extracts_files() {
        let temp_dir = TempDir::new().unwrap();
        let layer_path = temp_dir.path().join("layer.tar.gz");
        let target_dir = temp_dir.path().join("extracted");

        // Create layer with test files
        create_test_layer(
            &layer_path,
            &[("file1.txt", b"content1"), ("dir/file2.txt", b"content2")],
        );

        // Extract layer
        extract_layer(&layer_path, &target_dir).unwrap();

        // Verify files were extracted
        assert!(target_dir.join("file1.txt").exists());
        assert!(target_dir.join("dir/file2.txt").exists());

        // Verify content
        let content1 = fs::read_to_string(target_dir.join("file1.txt")).unwrap();
        assert_eq!(content1, "content1");

        let content2 = fs::read_to_string(target_dir.join("dir/file2.txt")).unwrap();
        assert_eq!(content2, "content2");
    }

    #[test]
    fn test_extract_layer_nonexistent_file() {
        let temp_dir = TempDir::new().unwrap();
        let layer_path = temp_dir.path().join("nonexistent.tar.gz");
        let target_dir = temp_dir.path().join("extracted");

        // Try to extract non-existent layer
        let result = extract_layer(&layer_path, &target_dir);

        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("Layer file not found"));
    }

    #[test]
    fn test_extract_layer_multiple_layers_to_same_target() {
        let temp_dir = TempDir::new().unwrap();
        let layer1_path = temp_dir.path().join("layer1.tar.gz");
        let layer2_path = temp_dir.path().join("layer2.tar.gz");
        let target_dir = temp_dir.path().join("extracted");

        // Create two layers
        create_test_layer(&layer1_path, &[("base.txt", b"base content")]);
        create_test_layer(&layer2_path, &[("app.txt", b"app content")]);

        // Extract both layers to same target
        extract_layer(&layer1_path, &target_dir).unwrap();
        extract_layer(&layer2_path, &target_dir).unwrap();

        // Verify both files exist
        assert!(target_dir.join("base.txt").exists());
        assert!(target_dir.join("app.txt").exists());
    }

    #[test]
    fn test_extract_layer_overwrites_existing_files() {
        let temp_dir = TempDir::new().unwrap();
        let layer1_path = temp_dir.path().join("layer1.tar.gz");
        let layer2_path = temp_dir.path().join("layer2.tar.gz");
        let target_dir = temp_dir.path().join("extracted");

        // Create two layers with same filename
        create_test_layer(&layer1_path, &[("file.txt", b"version 1")]);
        create_test_layer(&layer2_path, &[("file.txt", b"version 2")]);

        // Extract first layer
        extract_layer(&layer1_path, &target_dir).unwrap();
        let content1 = fs::read_to_string(target_dir.join("file.txt")).unwrap();
        assert_eq!(content1, "version 1");

        // Extract second layer (should overwrite)
        extract_layer(&layer2_path, &target_dir).unwrap();
        let content2 = fs::read_to_string(target_dir.join("file.txt")).unwrap();
        assert_eq!(content2, "version 2");
    }

    #[test]
    fn test_extract_layer_applies_whiteout() {
        let temp_dir = TempDir::new().unwrap();
        let layer1 = temp_dir.path().join("layer1.tar.gz");
        let layer2 = temp_dir.path().join("layer2.tar.gz");
        let target = temp_dir.path().join("extracted");

        create_test_layer(
            &layer1,
            &[("dir/keep.txt", b"keep"), ("dir/removed.txt", b"bye")],
        );
        // Upper layer whites out dir/removed.txt
        create_test_layer(&layer2, &[("dir/.wh.removed.txt", b"")]);

        extract_layer(&layer1, &target).unwrap();
        assert!(target.join("dir/removed.txt").exists());

        extract_layer(&layer2, &target).unwrap();
        assert!(target.join("dir/keep.txt").exists(), "sibling must survive");
        assert!(
            !target.join("dir/removed.txt").exists(),
            "whiteout must delete the file from the lower layer"
        );
        assert!(
            !target.join("dir/.wh.removed.txt").exists(),
            "whiteout marker must not be written to the rootfs"
        );
    }

    #[test]
    fn test_extract_layer_applies_opaque_directory() {
        let temp_dir = TempDir::new().unwrap();
        let layer1 = temp_dir.path().join("l1.tar.gz");
        let layer2 = temp_dir.path().join("l2.tar.gz");
        let target = temp_dir.path().join("ex");

        create_test_layer(&layer1, &[("d/old1.txt", b"a"), ("d/old2.txt", b"b")]);
        // Opaque marker clears prior dir contents; new.txt is added afterward.
        create_test_layer(&layer2, &[("d/.wh..wh..opq", b""), ("d/new.txt", b"c")]);

        extract_layer(&layer1, &target).unwrap();
        extract_layer(&layer2, &target).unwrap();

        assert!(!target.join("d/old1.txt").exists());
        assert!(!target.join("d/old2.txt").exists());
        assert!(target.join("d/new.txt").exists());
        assert!(!target.join("d/.wh..wh..opq").exists());
    }

    // Helper function to create a test tar.gz layer
    fn create_test_layer(path: &Path, files: &[(&str, &[u8])]) {
        use flate2::write::GzEncoder;
        use flate2::Compression;
        use tar::Builder;

        let file = File::create(path).unwrap();
        let encoder = GzEncoder::new(file, Compression::default());
        let mut builder = Builder::new(encoder);

        for (name, content) in files {
            let mut header = tar::Header::new_gnu();
            header.set_size(content.len() as u64);
            header.set_mode(0o644);
            // Set uid/gid explicitly: a bare GNU header leaves those octal fields
            // blank, which makes a root-side extraction with preserved ownership
            // fail to parse the uid ("numeric field was not a number"). Real OCI
            // layers always carry valid uid/gid fields.
            header.set_uid(0);
            header.set_gid(0);
            header.set_cksum();

            builder.append_data(&mut header, name, *content).unwrap();
        }

        builder.finish().unwrap();
    }

    fn write_test_tar<W: std::io::Write>(writer: W, files: &[(&str, &[u8])]) {
        use tar::Builder;
        let mut builder = Builder::new(writer);
        for (name, content) in files {
            let mut header = tar::Header::new_gnu();
            header.set_size(content.len() as u64);
            header.set_mode(0o644);
            header.set_uid(0);
            header.set_gid(0);
            header.set_cksum();
            builder.append_data(&mut header, name, *content).unwrap();
        }
        builder.finish().unwrap();
    }

    #[test]
    fn test_extract_layer_handles_zstd() {
        let temp_dir = TempDir::new().unwrap();
        let layer_path = temp_dir.path().join("layer.tar.zst");
        let target_dir = temp_dir.path().join("extracted");
        {
            let file = File::create(&layer_path).unwrap();
            let encoder = zstd::stream::write::Encoder::new(file, 0)
                .unwrap()
                .auto_finish();
            write_test_tar(encoder, &[("z.txt", b"zstd-content")]);
        }

        extract_layer(&layer_path, &target_dir).unwrap();
        assert_eq!(
            fs::read_to_string(target_dir.join("z.txt")).unwrap(),
            "zstd-content"
        );
    }

    #[test]
    fn test_extract_layer_handles_uncompressed_tar() {
        let temp_dir = TempDir::new().unwrap();
        let layer_path = temp_dir.path().join("layer.tar");
        let target_dir = temp_dir.path().join("extracted");
        write_test_tar(File::create(&layer_path).unwrap(), &[("p.txt", b"plain")]);

        extract_layer(&layer_path, &target_dir).unwrap();
        assert_eq!(
            fs::read_to_string(target_dir.join("p.txt")).unwrap(),
            "plain"
        );
    }
}
