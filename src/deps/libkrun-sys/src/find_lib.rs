//! Library detection utilities for finding libkrun on the system.
//!
//! This module provides functions to detect whether a directory contains
//! the libkrun library, with support for both loose matching (any library
//! starting with the prefix) and strict matching (exact library name).

use std::path::{Path, PathBuf};

/// Returns the platform-specific library extensions.
fn library_extensions() -> &'static [&'static str] {
    if cfg!(target_os = "macos") {
        &["dylib"]
    } else if cfg!(target_os = "linux") {
        &["so"]
    } else {
        &["dll"]
    }
}

/// Checks if `dir` contains any library file starting with `prefix`.
/// This is a loose match - `libkrun.dylib`, `libkrun-efi.dylib`, and
/// `libkrun.so.1` all match prefix `libkrun`.
pub fn has_library(dir: &Path, prefix: &str) -> bool {
    let extensions = library_extensions();

    dir.read_dir()
        .ok()
        .map(|entries| {
            entries.filter_map(Result::ok).any(|entry| {
                let name = entry.file_name();
                let filename = name.to_string_lossy();
                let matches_prefix = filename.starts_with(prefix);
                let matches_extension = extensions
                    .iter()
                    .any(|ext| entry.path().extension().is_some_and(|e| e == *ext));
                matches_prefix && matches_extension
            })
        })
        .unwrap_or(false)
}

/// Checks if `dir` contains a library named exactly `lib<name>.<ext>`.
/// This is stricter than `has_library`: it prevents matching sibling
/// libraries like `libkrun-efi.dylib` when looking for `libkrun.dylib`.
/// Both unversioned (`libkrun.dylib`) and versioned (`libkrun.so.1`)
/// library names are accepted.
pub fn has_exact_library(dir: &Path, name: &str) -> bool {
    let extensions = library_extensions();
    let prefix = format!("lib{name}");

    dir.read_dir()
        .ok()
        .map(|entries| {
            entries.filter_map(Result::ok).any(|entry| {
                let filename = entry.file_name();
                let filename_str = filename.to_string_lossy();
                let Some(rest) = filename_str.strip_prefix(&prefix) else {
                    return false;
                };
                // Accept unversioned names like `libkrun.so` and versioned
                // names like `libkrun.so.1` without matching siblings such as
                // `libkrun-efi.so`.
                extensions.iter().any(|ext| {
                    let suffix = format!(".{ext}");
                    rest == suffix
                        || rest.starts_with(&format!("{suffix}."))
                        || (rest.starts_with('.') && rest.ends_with(&suffix))
                })
            })
        })
        .unwrap_or(false)
}

/// Find a library in common system paths.
/// Returns the first path containing the library, or None.
pub fn find_library_in_common_paths(name: &str) -> Option<PathBuf> {
    #[cfg(target_os = "macos")]
    let common_paths = ["/opt/homebrew/lib", "/usr/local/lib", "/usr/lib"];
    #[cfg(not(target_os = "macos"))]
    let common_paths = ["/usr/local/lib", "/usr/lib", "/usr/lib64"];

    for path in &common_paths {
        let lib_path = Path::new(path);
        if has_exact_library(lib_path, name) {
            return Some(lib_path.to_path_buf());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn create_temp_lib_dir(files: &[&str]) -> TempDir {
        let temp_dir = TempDir::new().unwrap();
        for file in files {
            let path = temp_dir.path().join(file);
            if file.ends_with('/') {
                fs::create_dir_all(&path).unwrap();
            } else {
                fs::File::create(&path).unwrap();
            }
        }
        temp_dir
    }

    #[test]
    fn test_has_library_exact_match() {
        #[cfg(target_os = "macos")]
        let temp = create_temp_lib_dir(&["libkrun.dylib", "libkrun.1.dylib"]);
        #[cfg(target_os = "linux")]
        let temp = create_temp_lib_dir(&["libkrun.so", "libkrun.so.1"]);
        #[cfg(target_os = "windows")]
        let temp = create_temp_lib_dir(&["krun.dll"]);

        assert!(has_library(temp.path(), "libkrun"));
    }

    #[test]
    fn test_has_library_prefix_match() {
        #[cfg(target_os = "macos")]
        let temp = create_temp_lib_dir(&["libkrun.dylib", "libkrun-efi.dylib"]);
        #[cfg(target_os = "linux")]
        let temp = create_temp_lib_dir(&["libkrun.so", "libkrun-efi.so"]);

        // Should match both exact and sibling
        assert!(has_library(temp.path(), "libkrun"));
    }

    #[test]
    fn test_has_library_no_match() {
        let temp = create_temp_lib_dir(&["libother.dylib"]);
        assert!(!has_library(temp.path(), "libkrun"));
    }

    #[test]
    fn test_has_library_empty_dir() {
        let temp = create_temp_lib_dir(&[]);
        assert!(!has_library(temp.path(), "libkrun"));
    }

    #[test]
    fn test_has_exact_library_unversioned() {
        // Test that exact matching distinguishes libkrun from libkrun-efi
        #[cfg(target_os = "macos")]
        let temp = create_temp_lib_dir(&["libkrun.dylib", "libkrun-efi.dylib"]);
        #[cfg(target_os = "linux")]
        let temp = create_temp_lib_dir(&["libkrun.so", "libkrun-efi.so"]);

        // Searching for "krun" should find libkrun.dylib
        assert!(has_exact_library(temp.path(), "krun"));
        // Searching for "krun-efi" should find libkrun-efi.dylib
        assert!(has_exact_library(temp.path(), "krun-efi"));
    }

    #[test]
    fn test_has_exact_library_versioned() {
        #[cfg(target_os = "macos")]
        let temp = create_temp_lib_dir(&["libkrun.dylib", "libkrun.1.dylib"]);
        #[cfg(target_os = "linux")]
        let temp = create_temp_lib_dir(&["libkrun.so.1", "libkrun.so.1.0"]);

        assert!(has_exact_library(temp.path(), "krun"));
    }

    #[test]
    fn test_has_exact_library_no_sibling_match() {
        #[cfg(target_os = "macos")]
        let temp = create_temp_lib_dir(&["libkrun-efi.dylib", "libkrun-efi.1.dylib"]);
        #[cfg(target_os = "linux")]
        let temp = create_temp_lib_dir(&["libkrun-efi.so", "libkrun-efi.so.1"]);

        // Should NOT match libkrun-efi when looking for libkrun
        assert!(!has_exact_library(temp.path(), "krun"));
    }

    #[test]
    fn test_has_exact_library_non_matching_extension() {
        let temp = create_temp_lib_dir(&["libkrun.a", "libkrun.lib"]);

        #[cfg(target_os = "macos")]
        assert!(!has_exact_library(temp.path(), "krun"));
        #[cfg(target_os = "linux")]
        assert!(!has_exact_library(temp.path(), "krun"));
        #[cfg(target_os = "windows")]
        assert!(!has_exact_library(temp.path(), "krun"));
    }

    #[test]
    fn test_has_exact_library_empty_dir() {
        let temp = create_temp_lib_dir(&[]);
        assert!(!has_exact_library(temp.path(), "krun"));
    }

    // macOS-only: exercises `.dylib` matching; `temp` would be unused on other
    // platforms (clippy `-D warnings`).
    #[test]
    #[cfg(target_os = "macos")]
    fn test_has_exact_library_substring_prefix_issue() {
        // Regression: "libkrunner" must not match a lookup for "libkrun" via
        // substring — the rest is "er", not a valid extension.
        let temp = create_temp_lib_dir(&["libkrunner.dylib"]);
        assert!(!has_exact_library(temp.path(), "krun"));
    }

    #[test]
    fn test_find_library_in_common_paths() {
        // This test just verifies the function doesn't panic
        // In CI, the actual libkrun might not be installed
        let result = find_library_in_common_paths("krun");
        // Result depends on whether libkrun is actually installed
        if let Some(path) = result {
            assert!(path.exists());
        }
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn test_has_exact_library_with_version_suffix() {
        // Test versioned library like libkrun.1.dylib
        let temp = create_temp_lib_dir(&["libkrun.1.dylib", "libkrun.2.dylib"]);

        // "krun" prefix gives us "libkrun", rest is ".1.dylib"
        // Should match because it starts with '.' and ends with "dylib"
        assert!(has_exact_library(temp.path(), "krun"));
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn test_has_exact_library_linux_versioned() {
        // Test versioned library like libkrun.so.1.0
        let temp = create_temp_lib_dir(&["libkrun.so.1.0", "libkrun.so.1"]);

        assert!(has_exact_library(temp.path(), "krun"));
    }
}
