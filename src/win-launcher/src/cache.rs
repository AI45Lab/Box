//! Version-stamped cache to skip WSL2 re-detection on every run.

use std::fs;
use std::path::PathBuf;

fn cache_path() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".a3s")
        .join("wsl-ready")
}

/// Returns true if the cache at `path` exists and matches `version`.
fn is_valid_at(path: &PathBuf, version: &str) -> bool {
    match fs::read_to_string(path) {
        Ok(contents) => contents.trim() == version,
        Err(_) => false,
    }
}

/// Write `version` to the cache file at `path`.
fn write_at(path: &PathBuf, version: &str) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, version)
}

/// Delete the cache file at `path`.
fn invalidate_at(path: &PathBuf) {
    let _ = fs::remove_file(path);
}

/// Returns true if the cache exists and matches `version`.
pub fn is_valid(version: &str) -> bool {
    is_valid_at(&cache_path(), version)
}

/// Write the version string to the cache file.
pub fn write(version: &str) -> std::io::Result<()> {
    write_at(&cache_path(), version)
}

/// Delete the cache file (used on error recovery).
pub fn invalidate() {
    invalidate_at(&cache_path());
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn temp_cache_path(dir: &TempDir) -> PathBuf {
        dir.path().join(".a3s").join("wsl-ready")
    }

    #[test]
    fn cache_miss_when_file_absent() {
        let dir = TempDir::new().unwrap();
        let path = temp_cache_path(&dir);
        assert!(!is_valid_at(&path, "0.6.0"));
    }

    #[test]
    fn cache_hit_when_version_matches() {
        let dir = TempDir::new().unwrap();
        let path = temp_cache_path(&dir);
        write_at(&path, "0.6.0").unwrap();
        assert!(is_valid_at(&path, "0.6.0"));
    }

    #[test]
    fn cache_miss_when_version_differs() {
        let dir = TempDir::new().unwrap();
        let path = temp_cache_path(&dir);
        write_at(&path, "0.5.0").unwrap();
        assert!(!is_valid_at(&path, "0.6.0"));
    }

    #[test]
    fn invalidate_clears_cache() {
        let dir = TempDir::new().unwrap();
        let path = temp_cache_path(&dir);
        write_at(&path, "0.6.0").unwrap();
        invalidate_at(&path); // calls real production function
        assert!(!is_valid_at(&path, "0.6.0"));
    }
}
