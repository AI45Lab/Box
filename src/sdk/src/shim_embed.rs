//! Embedded shim binary — extract to `~/.a3s/bin/` on first use.
//!
//! The `a3s-box-shim` binary is compiled and embedded into the SDK at build time
//! via `include_bytes!()`. At runtime, `ensure_shim()` extracts it to
//! `{home_dir}/bin/a3s-box-shim` with a version sidecar file for staleness checks.

use std::path::{Path, PathBuf};

use a3s_box_core::error::{BoxError, Result};

/// Embedded shim binary bytes (set by build.rs via `A3S_SHIM_BINARY_PATH` env var).
///
/// When `A3S_SHIM_BINARY_PATH` is not set (e.g., during `cargo check` or doc builds),
/// this falls back to an empty slice and `ensure_shim()` will skip extraction,
/// deferring to the normal `find_shim()` search path.
#[cfg(feature = "embed-shim")]
static SHIM_BINARY: &[u8] = include_bytes!(env!("A3S_SHIM_BINARY_PATH"));

#[cfg(not(feature = "embed-shim"))]
static SHIM_BINARY: &[u8] = &[];

/// SDK version used as the shim version tag.
const SHIM_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Ensure the shim binary is available at `{home_dir}/bin/a3s-box-shim`.
///
/// - If the embedded binary is empty (feature disabled), returns `None` and
///   the caller should fall back to `VmController::find_shim()`.
/// - If the binary exists and the version matches, returns the existing path.
/// - Otherwise, writes the embedded bytes, sets executable permission, writes
///   a `.version` sidecar, and on macOS signs with Hypervisor entitlement.
pub fn ensure_shim(home_dir: &Path) -> Result<Option<PathBuf>> {
    if SHIM_BINARY.is_empty() {
        tracing::debug!("No embedded shim binary, deferring to find_shim()");
        return Ok(None);
    }

    let bin_dir = home_dir.join("bin");
    std::fs::create_dir_all(&bin_dir).map_err(|e| {
        BoxError::ConfigError(format!(
            "Failed to create bin directory {}: {}",
            bin_dir.display(),
            e
        ))
    })?;

    let shim_path = bin_dir.join("a3s-box-shim");
    let version_path = bin_dir.join("a3s-box-shim.version");

    // Check if already extracted with matching version
    if shim_path.exists() {
        if let Ok(existing_version) = std::fs::read_to_string(&version_path) {
            if existing_version.trim() == SHIM_VERSION {
                tracing::debug!(
                    path = %shim_path.display(),
                    version = SHIM_VERSION,
                    "Shim binary already up-to-date"
                );
                return Ok(Some(shim_path));
            }
        }
    }

    // Write the shim binary
    tracing::info!(
        path = %shim_path.display(),
        version = SHIM_VERSION,
        size_bytes = SHIM_BINARY.len(),
        "Extracting embedded shim binary"
    );

    std::fs::write(&shim_path, SHIM_BINARY).map_err(|e| {
        BoxError::ConfigError(format!(
            "Failed to write shim binary to {}: {}",
            shim_path.display(),
            e
        ))
    })?;

    // Set executable permission
    set_executable(&shim_path)?;

    // Write version sidecar
    std::fs::write(&version_path, SHIM_VERSION).map_err(|e| {
        BoxError::ConfigError(format!(
            "Failed to write shim version file {}: {}",
            version_path.display(),
            e
        ))
    })?;

    // On macOS, fix library paths and sign with Hypervisor.framework entitlement
    #[cfg(target_os = "macos")]
    {
        fix_macos_library_paths(&shim_path, &bin_dir, home_dir)?;
        sign_with_entitlement(&shim_path, home_dir)?;
    }

    Ok(Some(shim_path))
}

/// Set the executable bit on a file (Unix only).
#[cfg(unix)]
fn set_executable(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mut perms = std::fs::metadata(path)
        .map_err(|e| {
            BoxError::ConfigError(format!(
                "Failed to read permissions for {}: {}",
                path.display(),
                e
            ))
        })?
        .permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(path, perms).map_err(|e| {
        BoxError::ConfigError(format!(
            "Failed to set executable permission on {}: {}",
            path.display(),
            e
        ))
    })
}

#[cfg(not(unix))]
fn set_executable(_path: &Path) -> Result<()> {
    Ok(())
}

/// Fix library paths on macOS so the shim can find libkrun at runtime.
///
/// On macOS, when a binary has the Hypervisor entitlement, dyld ignores
/// DYLD_LIBRARY_PATH. This function:
/// 1. Looks for libkrun in the home lib directory first (~/.a3s/lib)
/// 2. Falls back to common build artifact locations
/// 3. Copies it to the same directory as the shim
/// 4. Patches the shim's library dependency to use @executable_path
#[cfg(target_os = "macos")]
fn fix_macos_library_paths(shim_path: &Path, bin_dir: &Path, home_dir: &Path) -> Result<()> {
    use std::process::Command;

    // First check home lib directory (where we install libkrun during setup)
    let home_lib = home_dir.join("lib");
    let mut libkrun_paths = vec![home_lib.clone()];

    // Add common build artifact locations
    libkrun_paths.push(PathBuf::from(
        "/Users/roylin/Desktop/code/a3s/crates/box/src/deps/libkrun-sys/vendor/libkrun/target/release",
    ));
    libkrun_paths.push(PathBuf::from(
        "/Users/roylin/Desktop/code/a3s/target/release",
    ));
    libkrun_paths.push(PathBuf::from("/Users/roylin/Desktop/code/a3s/target/debug"));

    let libkrun_dir = libkrun_paths
        .iter()
        .find(|p| p.join("libkrun.dylib").exists());

    let libkrun_dir = match libkrun_dir {
        Some(dir) => dir.clone(),
        None => {
            tracing::warn!(
                "libkrun not found, VM boot may fail. \
                 Install libkrun to ~/.a3s/lib or ensure build artifacts are preserved."
            );
            return Ok(()); // Non-fatal - VM boot may still work if libkrun is found elsewhere
        }
    };

    let libkrun_src = libkrun_dir.join("libkrun.dylib");
    let libkrun_alias_src = libkrun_dir.join("libkrun.1.dylib");
    let libkrun_dst = bin_dir.join("libkrun.dylib");
    let libkrun_alias_dst = bin_dir.join("libkrun.1.dylib");

    // Copy libkrun.dylib if not already present or if source is newer
    let should_copy = !libkrun_dst.exists()
        || std::fs::metadata(&libkrun_src)
            .ok()
            .and_then(|s| s.modified().ok())
            > std::fs::metadata(&libkrun_dst)
                .ok()
                .and_then(|s| s.modified().ok());

    if should_copy {
        tracing::info!(
            src = %libkrun_src.display(),
            dst = %libkrun_dst.display(),
            "Copying libkrun to shim directory"
        );
        std::fs::copy(&libkrun_src, &libkrun_dst)
            .map_err(|e| BoxError::ConfigError(format!("Failed to copy libkrun: {}", e)))?;

        // Copy the versioned alias too
        if libkrun_alias_src.exists() {
            std::fs::copy(&libkrun_alias_src, &libkrun_alias_dst).ok();
        }
    }

    // Patch the shim's library dependency to use @executable_path
    // This changes "libkrun.1.dylib" to "@executable_path/libkrun.1.dylib"
    let status = Command::new("install_name_tool")
        .args([
            "-change",
            "libkrun.1.dylib",
            "@executable_path/libkrun.1.dylib",
            shim_path.to_str().unwrap(),
        ])
        .status()
        .map_err(|e| BoxError::ExecError(format!("Failed to run install_name_tool: {}", e)))?;

    if !status.success() {
        tracing::warn!("Failed to patch shim library path (install_name_tool failed)");
    }

    // Also fix the install name in libkrun.dylib to use @executable_path
    let libkrun_install_name_status = Command::new("install_name_tool")
        .args([
            "-id",
            "@executable_path/libkrun.dylib",
            libkrun_dst.to_str().unwrap(),
        ])
        .status();

    if let Ok(s) = libkrun_install_name_status {
        if s.success() {
            tracing::debug!("Patched libkrun install name to @executable_path/libkrun.dylib");
        }
    }

    Ok(())
}

/// Sign the shim binary with com.apple.security.hypervisor entitlement (macOS).
#[cfg(target_os = "macos")]
fn sign_with_entitlement(shim_path: &Path, home_dir: &Path) -> Result<()> {
    // Write entitlements plist next to the shim
    let entitlements_path = home_dir.join("bin").join("entitlements.plist");
    if !entitlements_path.exists() {
        let plist = r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>com.apple.security.hypervisor</key>
    <true/>
</dict>
</plist>"#;
        std::fs::write(&entitlements_path, plist).map_err(|e| {
            BoxError::ConfigError(format!("Failed to write entitlements plist: {}", e))
        })?;
    }

    // Check if already signed
    let check = std::process::Command::new("codesign")
        .args(["-d", "--entitlements", ":-"])
        .arg(shim_path)
        .output();

    if let Ok(output) = check {
        if output.status.success() {
            let stdout = String::from_utf8_lossy(&output.stdout);
            if stdout.contains("com.apple.security.hypervisor") {
                tracing::debug!("Shim already signed with hypervisor entitlement");
                return Ok(());
            }
        }
    }

    tracing::info!(path = %shim_path.display(), "Signing shim with hypervisor entitlement");
    let status = std::process::Command::new("codesign")
        .args(["--sign", "-", "--entitlements"])
        .arg(&entitlements_path)
        .arg("--force")
        .arg(shim_path)
        .status()
        .map_err(|e| BoxError::ExecError(format!("Failed to run codesign: {}", e)))?;

    if !status.success() {
        tracing::warn!(
            path = %shim_path.display(),
            "codesign failed (non-fatal) — VM boot may fail without hypervisor entitlement"
        );
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_shim_version_is_set() {
        assert!(!SHIM_VERSION.is_empty());
    }

    #[test]
    fn test_ensure_shim_no_embed() {
        // Without embed-shim feature, SHIM_BINARY is empty → returns None
        if SHIM_BINARY.is_empty() {
            let tmp = tempfile::TempDir::new().unwrap();
            let result = ensure_shim(tmp.path()).unwrap();
            assert!(result.is_none());
        }
    }

    #[test]
    fn test_ensure_shim_extracts_binary() {
        if SHIM_BINARY.is_empty() {
            // Skip when not embedded
            return;
        }
        let tmp = tempfile::TempDir::new().unwrap();
        let result = ensure_shim(tmp.path()).unwrap();
        assert!(result.is_some());

        let shim_path = result.unwrap();
        assert!(shim_path.exists());
        // The extracted binary may differ in size from the embedded one because
        // macOS codesign replaces the signature on write. Just verify it's non-empty.
        let extracted_len = std::fs::read(&shim_path).unwrap().len();
        assert!(extracted_len > 0);

        // Version file should exist
        let version_path = tmp.path().join("bin").join("a3s-box-shim.version");
        assert_eq!(
            std::fs::read_to_string(version_path).unwrap().trim(),
            SHIM_VERSION
        );
    }

    #[test]
    fn test_ensure_shim_idempotent() {
        if SHIM_BINARY.is_empty() {
            return;
        }
        let tmp = tempfile::TempDir::new().unwrap();

        // First call extracts
        let path1 = ensure_shim(tmp.path()).unwrap().unwrap();
        // Second call returns same path without re-extracting
        let path2 = ensure_shim(tmp.path()).unwrap().unwrap();
        assert_eq!(path1, path2);
    }

    #[cfg(unix)]
    #[test]
    fn test_set_executable() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::TempDir::new().unwrap();
        let file = tmp.path().join("test-bin");
        std::fs::write(&file, b"test").unwrap();

        set_executable(&file).unwrap();

        let mode = std::fs::metadata(&file).unwrap().permissions().mode();
        assert_eq!(mode & 0o755, 0o755);
    }

    #[test]
    fn test_ensure_shim_creates_bin_dir() {
        let tmp = tempfile::TempDir::new().unwrap();
        let bin_dir = tmp.path().join("bin");
        assert!(!bin_dir.exists());

        let _ = ensure_shim(tmp.path());
        // bin dir should be created even if SHIM_BINARY is empty
        // (only if non-empty, but the function returns early if empty)
        if !SHIM_BINARY.is_empty() {
            assert!(bin_dir.exists());
        }
    }
}
