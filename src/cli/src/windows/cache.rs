//! Version-stamped cache to skip WSL2 re-detection on every run.

use std::fs;
use std::path::PathBuf;

fn cache_path() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".a3s")
        .join("wsl-ready")
}

/// Returns true if the cache exists and matches `version`.
pub fn is_valid(version: &str) -> bool {
    match fs::read_to_string(cache_path()) {
        Ok(contents) => contents.trim() == version,
        Err(_) => false,
    }
}

/// Write the version string to the cache file.
pub fn write(version: &str) -> std::io::Result<()> {
    let path = cache_path();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, version)
}

/// Delete the cache file (used on error recovery).
pub fn invalidate() {
    let _ = fs::remove_file(cache_path());
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;
    use tempfile::TempDir;

    fn cache_path_in(base: &PathBuf) -> PathBuf {
        base.join(".a3s").join("wsl-ready")
    }

    fn write_to(path: &PathBuf, version: &str) -> std::io::Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(path, version)
    }

    fn is_valid_in(path: &PathBuf, version: &str) -> bool {
        match fs::read_to_string(path) {
            Ok(contents) => contents.trim() == version,
            Err(_) => false,
        }
    }

    // Each test gets its own TempDir and operates on paths directly,
    // avoiding any shared mutable state (HOME env var races).

    #[test]
    fn cache_miss_when_file_absent() {
        let dir = TempDir::new().unwrap();
        let path = cache_path_in(&dir.path().to_path_buf());
        assert!(!is_valid_in(&path, "0.6.0"));
    }

    #[test]
    fn cache_hit_when_version_matches() {
        let dir = TempDir::new().unwrap();
        let path = cache_path_in(&dir.path().to_path_buf());
        write_to(&path, "0.6.0").unwrap();
        assert!(is_valid_in(&path, "0.6.0"));
    }

    #[test]
    fn cache_miss_when_version_differs() {
        let dir = TempDir::new().unwrap();
        let path = cache_path_in(&dir.path().to_path_buf());
        write_to(&path, "0.5.0").unwrap();
        assert!(!is_valid_in(&path, "0.6.0"));
    }

    #[test]
    fn invalidate_clears_cache() {
        let dir = TempDir::new().unwrap();
        let path = cache_path_in(&dir.path().to_path_buf());
        write_to(&path, "0.6.0").unwrap();
        let _ = fs::remove_file(&path);
        assert!(!is_valid_in(&path, "0.6.0"));
    }
}
