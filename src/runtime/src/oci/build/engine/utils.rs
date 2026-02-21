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
        || lower.ends_with(".tbz2")
        || lower.ends_with(".tar.xz")
        || lower.ends_with(".txz")
}

/// Extract a tar archive to a destination directory.
pub(super) fn extract_tar_to_dst(archive_path: &Path, dst: &Path) -> Result<()> {
    use bzip2::read::BzDecoder;
    use flate2::read::GzDecoder;
    use std::io::BufReader;
    use xz2::read::XzDecoder;

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
    } else if name.ends_with(".tar.bz2") || name.ends_with(".tbz2") {
        let decoder = BzDecoder::new(BufReader::new(file));
        let mut archive = tar::Archive::new(decoder);
        archive.unpack(dst).map_err(|e| {
            BoxError::BuildError(format!(
                "Failed to extract tar.bz2 {}: {}",
                archive_path.display(),
                e
            ))
        })?;
    } else if name.ends_with(".tar.xz") || name.ends_with(".txz") {
        let decoder = XzDecoder::new(BufReader::new(file));
        let mut archive = tar::Archive::new(decoder);
        archive.unpack(dst).map_err(|e| {
            BoxError::BuildError(format!(
                "Failed to extract tar.xz {}: {}",
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
        return Err(BoxError::BuildError(format!(
            "Unsupported archive format: {}",
            archive_path.display()
        )));
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

#[cfg(test)]
mod tests {
    use super::*;

    // --- is_tar_archive tests ---

    #[test]
    fn test_is_tar_archive_tar() {
        assert!(is_tar_archive("file.tar"));
    }

    #[test]
    fn test_is_tar_archive_tar_gz() {
        assert!(is_tar_archive("file.tar.gz"));
        assert!(is_tar_archive("file.tgz"));
    }

    #[test]
    fn test_is_tar_archive_tar_bz2() {
        assert!(is_tar_archive("file.tar.bz2"));
        assert!(is_tar_archive("file.tbz2"));
    }

    #[test]
    fn test_is_tar_archive_tar_xz() {
        assert!(is_tar_archive("file.tar.xz"));
        assert!(is_tar_archive("file.txz"));
    }

    #[test]
    fn test_is_tar_archive_case_insensitive() {
        assert!(is_tar_archive("FILE.TAR.GZ"));
        assert!(is_tar_archive("Data.Tar.Bz2"));
        assert!(is_tar_archive("ARCHIVE.TAR.XZ"));
    }

    #[test]
    fn test_is_tar_archive_non_archive() {
        assert!(!is_tar_archive("file.txt"));
        assert!(!is_tar_archive("file.zip"));
        assert!(!is_tar_archive("file.gz"));
        assert!(!is_tar_archive("file.bz2"));
        assert!(!is_tar_archive("file.xz"));
    }

    // --- extract_tar_to_dst tests ---

    /// Helper: create a tar archive with a single file.
    fn create_test_tar(
        dir: &std::path::Path,
        filename: &str,
        content: &[u8],
    ) -> std::path::PathBuf {
        let tar_path = dir.join(filename);
        let file = std::fs::File::create(&tar_path).unwrap();
        let mut builder = tar::Builder::new(file);

        let mut header = tar::Header::new_gnu();
        header.set_size(content.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        builder
            .append_data(&mut header, "test.txt", content)
            .unwrap();
        builder.finish().unwrap();

        tar_path
    }

    #[test]
    fn test_extract_plain_tar() {
        let tmp = tempfile::tempdir().unwrap();
        let tar_path = create_test_tar(tmp.path(), "test.tar", b"hello tar");

        let dst = tmp.path().join("out");
        extract_tar_to_dst(&tar_path, &dst).unwrap();
        assert!(dst.join("test.txt").exists());
    }

    #[test]
    fn test_extract_tar_gz() {
        use flate2::write::GzEncoder;
        use flate2::Compression;
        use std::io::Write;

        let tmp = tempfile::tempdir().unwrap();

        // Create a tar in memory
        let mut tar_data = Vec::new();
        {
            let mut builder = tar::Builder::new(&mut tar_data);
            let mut header = tar::Header::new_gnu();
            let content = b"hello gzip";
            header.set_size(content.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            builder
                .append_data(&mut header, "test.txt", &content[..])
                .unwrap();
            builder.finish().unwrap();
        }

        // Gzip compress
        let gz_path = tmp.path().join("test.tar.gz");
        let gz_file = std::fs::File::create(&gz_path).unwrap();
        let mut encoder = GzEncoder::new(gz_file, Compression::default());
        encoder.write_all(&tar_data).unwrap();
        encoder.finish().unwrap();

        let dst = tmp.path().join("out");
        extract_tar_to_dst(&gz_path, &dst).unwrap();
        assert!(dst.join("test.txt").exists());
    }

    #[test]
    fn test_extract_tar_bz2() {
        use bzip2::write::BzEncoder;
        use bzip2::Compression;
        use std::io::Write;

        let tmp = tempfile::tempdir().unwrap();

        // Create a tar in memory
        let mut tar_data = Vec::new();
        {
            let mut builder = tar::Builder::new(&mut tar_data);
            let mut header = tar::Header::new_gnu();
            let content = b"hello bzip2";
            header.set_size(content.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            builder
                .append_data(&mut header, "test.txt", &content[..])
                .unwrap();
            builder.finish().unwrap();
        }

        // Bzip2 compress
        let bz2_path = tmp.path().join("test.tar.bz2");
        let bz2_file = std::fs::File::create(&bz2_path).unwrap();
        let mut encoder = BzEncoder::new(bz2_file, Compression::default());
        encoder.write_all(&tar_data).unwrap();
        encoder.finish().unwrap();

        let dst = tmp.path().join("out");
        extract_tar_to_dst(&bz2_path, &dst).unwrap();
        assert!(dst.join("test.txt").exists());
    }

    #[test]
    fn test_extract_tar_xz() {
        use std::io::Write;
        use xz2::write::XzEncoder;

        let tmp = tempfile::tempdir().unwrap();

        // Create a tar in memory
        let mut tar_data = Vec::new();
        {
            let mut builder = tar::Builder::new(&mut tar_data);
            let mut header = tar::Header::new_gnu();
            let content = b"hello xz";
            header.set_size(content.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            builder
                .append_data(&mut header, "test.txt", &content[..])
                .unwrap();
            builder.finish().unwrap();
        }

        // XZ compress
        let xz_path = tmp.path().join("test.tar.xz");
        let xz_file = std::fs::File::create(&xz_path).unwrap();
        let mut encoder = XzEncoder::new(xz_file, 6);
        encoder.write_all(&tar_data).unwrap();
        encoder.finish().unwrap();

        let dst = tmp.path().join("out");
        extract_tar_to_dst(&xz_path, &dst).unwrap();
        assert!(dst.join("test.txt").exists());
    }

    #[test]
    fn test_extract_nonexistent_file_fails() {
        let tmp = tempfile::tempdir().unwrap();
        let result = extract_tar_to_dst(
            &tmp.path().join("nonexistent.tar.gz"),
            &tmp.path().join("out"),
        );
        assert!(result.is_err());
    }

    // --- resolve_path tests ---

    #[test]
    fn test_resolve_path_absolute() {
        assert_eq!(resolve_path("/work", "/etc/config"), "/etc/config");
    }

    #[test]
    fn test_resolve_path_relative() {
        assert_eq!(resolve_path("/work", "src/main.rs"), "/work/src/main.rs");
    }

    // --- format_size tests ---

    #[test]
    fn test_format_size_bytes() {
        assert_eq!(format_size(42), "42 B");
    }

    #[test]
    fn test_format_size_kb() {
        assert_eq!(format_size(2048), "2.0 KB");
    }

    #[test]
    fn test_format_size_mb() {
        assert_eq!(format_size(5 * 1024 * 1024), "5.0 MB");
    }

    #[test]
    fn test_format_size_gb() {
        assert_eq!(format_size(2 * 1024 * 1024 * 1024), "2.0 GB");
    }
}
