//! Utility functions for the build engine.

use std::collections::HashMap;
use std::path::Path;

use a3s_box_core::error::{BoxError, Result};

use super::super::layer::sha256_bytes;

/// Check if a filename looks like a tar archive.
pub(super) fn is_tar_archive(name: &str) -> bool {
    let lower = name.to_lowercase();
    lower.ends_with(".tar")
        || lower.ends_with(".tar.gz")
        || lower.ends_with(".tgz")
        || lower.ends_with(".tar.bz2")
        || lower.ends_with(".tar.xz")
}

/// Extract a tar archive to a destination directory.
pub(super) fn extract_tar_to_dst(archive_path: &Path, dst: &Path) -> Result<()> {
    use flate2::read::GzDecoder;
    use std::io::BufReader;

    std::fs::create_dir_all(dst).map_err(|e| {
        BoxError::BuildError(format!(
            "Failed to create extraction directory {}: {}",
            dst.display(),
            e
        ))
    })?;

    let file = std::fs::File::open(archive_path).map_err(|e| {
        BoxError::BuildError(format!(
            "Failed to open archive {}: {}",
            archive_path.display(),
            e
        ))
    })?;

    let name = archive_path.to_str().unwrap_or("").to_lowercase();

    if name.ends_with(".tar.gz") || name.ends_with(".tgz") {
        let decoder = GzDecoder::new(BufReader::new(file));
        let mut archive = tar::Archive::new(decoder);
        archive.unpack(dst).map_err(|e| {
            BoxError::BuildError(format!(
                "Failed to extract tar.gz {}: {}",
                archive_path.display(),
                e
            ))
        })?;
    } else if name.ends_with(".tar") {
        let mut archive = tar::Archive::new(BufReader::new(file));
        archive.unpack(dst).map_err(|e| {
            BoxError::BuildError(format!(
                "Failed to extract tar {}: {}",
                archive_path.display(),
                e
            ))
        })?;
    } else {
        // .tar.bz2, .tar.xz — not supported yet, fall back to plain copy
        tracing::warn!(
            path = archive_path.to_str().unwrap_or(""),
            "Unsupported archive format for auto-extraction, copying as-is"
        );
        let target = dst.join(
            archive_path
                .file_name()
                .unwrap_or_else(|| std::ffi::OsStr::new("archive")),
        );
        std::fs::copy(archive_path, &target)
            .map_err(|e| BoxError::BuildError(format!("Failed to copy archive: {}", e)))?;
    }

    Ok(())
}

/// Resolve a path relative to a working directory.
///
/// If `path` is absolute, return it as-is. Otherwise, join with `workdir`.
pub(super) fn resolve_path(workdir: &str, path: &str) -> String {
    if path.starts_with('/') {
        path.to_string()
    } else {
        format!("{}/{}", workdir.trim_end_matches('/'), path)
    }
}

/// Expand `${VAR}` and `$VAR` references in a string using build args.
pub(super) fn expand_args(s: &str, args: &HashMap<String, String>) -> String {
    let mut result = s.to_string();
    for (key, value) in args {
        result = result.replace(&format!("${{{}}}", key), value);
        result = result.replace(&format!("${}", key), value);
    }
    result
}

/// Compute the diff_id (SHA256 of uncompressed layer content).
pub(super) fn compute_diff_id(layer_path: &Path) -> Result<String> {
    let data = std::fs::read(layer_path)
        .map_err(|e| BoxError::BuildError(format!("Failed to read layer for diff_id: {}", e)))?;

    // Decompress gzip to get raw tar
    use flate2::read::GzDecoder;
    use std::io::Read;

    let decoder = GzDecoder::new(&data[..]);
    let mut uncompressed = Vec::new();
    std::io::BufReader::new(decoder)
        .read_to_end(&mut uncompressed)
        .map_err(|e| {
            BoxError::BuildError(format!("Failed to decompress layer for diff_id: {}", e))
        })?;

    Ok(sha256_bytes(&uncompressed))
}

/// Recursively copy a directory.
pub(super) fn copy_dir_recursive(src: &Path, dst: &Path) -> Result<()> {
    std::fs::create_dir_all(dst).map_err(|e| {
        BoxError::BuildError(format!(
            "Failed to create directory {}: {}",
            dst.display(),
            e
        ))
    })?;

    for entry in std::fs::read_dir(src).map_err(|e| {
        BoxError::BuildError(format!("Failed to read directory {}: {}", src.display(), e))
    })? {
        let entry =
            entry.map_err(|e| BoxError::BuildError(format!("Failed to read entry: {}", e)))?;
        let src_path = entry.path();
        let dst_path = dst.join(entry.file_name());

        if src_path.is_dir() {
            copy_dir_recursive(&src_path, &dst_path)?;
        } else {
            std::fs::copy(&src_path, &dst_path).map_err(|e| {
                BoxError::BuildError(format!(
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

/// Format a byte size as a human-readable string.
pub(super) fn format_size(bytes: u64) -> String {
    if bytes >= 1024 * 1024 * 1024 {
        format!("{:.1} GB", bytes as f64 / (1024.0 * 1024.0 * 1024.0))
    } else if bytes >= 1024 * 1024 {
        format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
    } else if bytes >= 1024 {
        format!("{:.1} KB", bytes as f64 / 1024.0)
    } else {
        format!("{} B", bytes)
    }
}
