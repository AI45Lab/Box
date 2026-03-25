//! VmController - Default VMM backend using shim subprocesses.

use std::path::PathBuf;
use std::process::{Command, Stdio};

use a3s_box_core::error::{BoxError, Result};
use async_trait::async_trait;

use super::handler::ShimHandler;
use super::provider::VmmProvider;
use super::spec::InstanceSpec;
use super::VmHandler;

/// Controller for spawning VM subprocesses.
///
/// Spawns the `a3s-box-shim` binary in a subprocess and returns a ShimHandler
/// for runtime operations. The subprocess isolation ensures that VM process
/// takeover doesn't affect the host application.
pub struct VmController {
    /// Path to the a3s-box-shim binary
    shim_path: PathBuf,
}

impl VmController {
    /// Create a new VmController.
    ///
    /// # Arguments
    /// * `shim_path` - Path to the a3s-box-shim binary
    ///
    /// # Returns
    /// * `Ok(VmController)` - Successfully created controller
    /// * `Err(...)` - Failed to create controller (e.g., binary not found)
    pub fn new(shim_path: PathBuf) -> Result<Self> {
        // Verify that the shim binary exists
        if !shim_path.exists() {
            return Err(BoxError::BoxBootError {
                message: format!("Shim binary not found: {}", shim_path.display()),
                hint: Some("Build the shim with: cargo build -p a3s-box-shim".to_string()),
            });
        }

        // On macOS, ensure the shim has the Hypervisor.framework entitlement
        #[cfg(target_os = "macos")]
        Self::ensure_entitlement(&shim_path)?;

        Ok(Self { shim_path })
    }

    /// Ensure the shim binary has the com.apple.security.hypervisor entitlement.
    ///
    /// On macOS, Hypervisor.framework requires this entitlement. If the binary
    /// was built with `cargo build` directly (without `just build`), it won't
    /// have the entitlement. This method checks and signs it if needed.
    ///
    /// Uses a file lock to prevent race conditions when multiple processes
    /// (e.g., concurrent tests) try to sign the same binary simultaneously.
    #[cfg(target_os = "macos")]
    fn ensure_entitlement(shim_path: &std::path::Path) -> Result<()> {
        use std::fs::File;

        // Fast path: check without lock first
        if Self::has_hypervisor_entitlement(shim_path)? {
            return Ok(());
        }

        // Acquire exclusive file lock to prevent concurrent codesign
        let lock_path = std::env::temp_dir().join("a3s-box-shim-codesign.lock");
        let lock_file = File::create(&lock_path).map_err(|e| BoxError::BoxBootError {
            message: format!("Failed to create codesign lock file: {}", e),
            hint: None,
        })?;

        // flock(LOCK_EX) — blocks until exclusive lock is acquired
        let fd = std::os::unix::io::AsRawFd::as_raw_fd(&lock_file);
        let ret = unsafe { libc::flock(fd, libc::LOCK_EX) };
        if ret != 0 {
            return Err(BoxError::BoxBootError {
                message: format!(
                    "Failed to acquire codesign lock: {}",
                    std::io::Error::last_os_error()
                ),
                hint: None,
            });
        }

        // Re-check after acquiring lock — another process may have signed it
        if Self::has_hypervisor_entitlement(shim_path)? {
            // Lock is released when lock_file is dropped
            return Ok(());
        }

        tracing::info!("Signing shim with Hypervisor.framework entitlement");

        let entitlements_path = Self::find_entitlements_plist(shim_path)?;

        let status = Command::new("codesign")
            .args(["--entitlements"])
            .arg(&entitlements_path)
            .args(["--force", "-s", "-"])
            .arg(shim_path)
            .status()
            .map_err(|e| BoxError::BoxBootError {
                message: format!("Failed to codesign shim: {}", e),
                hint: None,
            })?;

        if !status.success() {
            return Err(BoxError::BoxBootError {
                message: "Failed to sign shim with Hypervisor entitlement".to_string(),
                hint: Some(format!(
                    "Try manually: codesign --entitlements {} --force -s - {}",
                    entitlements_path.display(),
                    shim_path.display()
                )),
            });
        }

        // Lock is released when lock_file is dropped
        Ok(())
    }

    /// Check if the shim binary already has the Hypervisor entitlement.
    #[cfg(target_os = "macos")]
    fn has_hypervisor_entitlement(shim_path: &std::path::Path) -> Result<bool> {
        let output = Command::new("codesign")
            .args(["-d", "--entitlements", "-", "--xml"])
            .arg(shim_path)
            .output()
            .map_err(|e| BoxError::BoxBootError {
                message: format!("Failed to check entitlements: {}", e),
                hint: None,
            })?;

        let stdout = String::from_utf8_lossy(&output.stdout);
        Ok(stdout.contains("com.apple.security.hypervisor"))
    }

    /// Find the entitlements.plist file.
    #[cfg(target_os = "macos")]
    fn find_entitlements_plist(shim_path: &std::path::Path) -> Result<PathBuf> {
        // Try next to the shim binary
        if let Some(dir) = shim_path.parent() {
            let plist = dir.join("entitlements.plist");
            if plist.exists() {
                return Ok(plist);
            }
        }

        // Try the source tree relative to the shim binary
        // target/debug/a3s-box-shim -> ../../shim/entitlements.plist
        if let Some(dir) = shim_path.parent() {
            for ancestor in dir.ancestors().take(5) {
                let plist = ancestor.join("shim").join("entitlements.plist");
                if plist.exists() {
                    return Ok(plist);
                }
            }
        }

        // Generate a temporary entitlements plist as fallback
        let tmp_plist = std::env::temp_dir().join("a3s-box-entitlements.plist");
        std::fs::write(
            &tmp_plist,
            r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>com.apple.security.hypervisor</key>
    <true/>
</dict>
</plist>
"#,
        )
        .map_err(|e| BoxError::BoxBootError {
            message: format!("Failed to write temporary entitlements plist: {}", e),
            hint: None,
        })?;

        Ok(tmp_plist)
    }

    /// Find the shim binary in common locations.
    ///
    /// Searches in order:
    /// 1. Same directory as current executable
    /// 2. `~/.a3s/bin/` (SDK-extracted shim)
    /// 3. target/debug or target/release (for development)
    /// 4. PATH
    pub fn find_shim() -> Result<PathBuf> {
        // On Windows the binary has a .exe suffix; on other platforms it's empty.
        #[cfg(target_os = "windows")]
        let shim_name = "a3s-box-shim.exe";
        #[cfg(not(target_os = "windows"))]
        let shim_name = "a3s-box-shim";

        // Try same directory as current executable
        if let Ok(exe_path) = std::env::current_exe() {
            if let Some(exe_dir) = exe_path.parent() {
                let shim_path = exe_dir.join(shim_name);
                if shim_path.exists() {
                    return Ok(shim_path);
                }
            }
        }

        // Try ~/.a3s/bin/ (SDK-extracted shim)
        {
            let shim_path = a3s_box_core::dirs_home().join("bin").join(shim_name);
            if shim_path.exists() {
                return Ok(shim_path);
            }
        }

        // Try target directories (for development)
        let target_dirs = ["target/debug", "target/release"];
        for dir in target_dirs {
            let shim_path = PathBuf::from(dir).join(shim_name);
            if shim_path.exists() {
                return Ok(shim_path);
            }
        }

        // Try PATH — use `where` on Windows, `which` elsewhere
        #[cfg(target_os = "windows")]
        let which_cmd = "where";
        #[cfg(not(target_os = "windows"))]
        let which_cmd = "which";

        if let Ok(output) = Command::new(which_cmd).arg(shim_name).output() {
            if output.status.success() {
                let path = String::from_utf8_lossy(&output.stdout)
                    .lines()
                    .next()
                    .unwrap_or("")
                    .trim()
                    .to_string();
                if !path.is_empty() {
                    return Ok(PathBuf::from(path));
                }
            }
        }

        Err(BoxError::BoxBootError {
            message: "Could not find a3s-box-shim binary".to_string(),
            hint: Some("Build the shim with: cargo build -p a3s-box-shim".to_string()),
        })
    }

    #[cfg(target_os = "windows")]
    fn windows_shim_path_env(shim_path: &std::path::Path) -> Option<std::ffi::OsString> {
        use std::collections::HashSet;

        let mut dirs = Vec::<PathBuf>::new();
        if let Ok(dir) = std::env::var("LIBKRUN_DIR") {
            dirs.push(PathBuf::from(dir));
        }
        if let Some(dir) = option_env!("LIBKRUN_DIR") {
            dirs.push(PathBuf::from(dir));
        }
        if let Some(dir) = shim_path.parent() {
            dirs.push(dir.to_path_buf());
        }

        let mut seen = HashSet::new();
        let mut path_entries = Vec::new();
        for dir in dirs {
            if !seen.insert(dir.clone()) {
                continue;
            }
            if dir.join("krun.dll").exists() && dir.join("libkrunfw.dll").exists() {
                path_entries.push(dir);
            }
        }

        if path_entries.is_empty() {
            return None;
        }

        let mut merged = std::ffi::OsString::new();
        for entry in path_entries {
            if !merged.is_empty() {
                merged.push(";");
            }
            merged.push(entry);
        }
        if let Some(existing) = std::env::var_os("PATH") {
            if !merged.is_empty() {
                merged.push(";");
            }
            merged.push(existing);
        }
        Some(merged)
    }
}

#[async_trait]
impl VmmProvider for VmController {
    async fn start(&self, spec: &InstanceSpec) -> Result<Box<dyn VmHandler>> {
        tracing::debug!(
            box_id = %spec.box_id,
            vcpus = spec.vcpus,
            memory_mib = spec.memory_mib,
            "Starting VM subprocess"
        );

        // Serialize the config for passing to subprocess
        let config_json = serde_json::to_string(spec).map_err(|e| BoxError::BoxBootError {
            message: format!("Failed to serialize config: {}", e),
            hint: None,
        })?;

        tracing::trace!(config = %config_json, "VM configuration");

        // Ensure socket directory exists
        if let Some(socket_dir) = spec.exec_socket_path.parent() {
            std::fs::create_dir_all(socket_dir).map_err(|e| BoxError::BoxBootError {
                message: format!(
                    "Failed to create socket directory {}: {}",
                    socket_dir.display(),
                    e
                ),
                hint: None,
            })?;
        }

        // Spawn shim subprocess
        #[cfg(target_os = "macos")]
        tracing::info!(
            shim = %self.shim_path.display(),
            box_id = %spec.box_id,
            net_socket_fd = spec.network.as_ref().and_then(|net| net.net_socket_fd),
            net_proxy_fd = spec.network.as_ref().and_then(|net| net.net_proxy_fd),
            "Spawning shim subprocess"
        );
        #[cfg(not(target_os = "macos"))]
        tracing::info!(
            shim = %self.shim_path.display(),
            box_id = %spec.box_id,
            "Spawning shim subprocess"
        );

        let mut cmd = Command::new(&self.shim_path);
        cmd.arg("--config")
            .arg(&config_json)
            .stdin(Stdio::null())
            .stdout(Stdio::inherit()) // Inherit for debugging
            .stderr(Stdio::inherit());

        // On macOS, set DYLD_LIBRARY_PATH to help find libkrunfw
        #[cfg(target_os = "macos")]
        {
            let mut dylib_paths = Vec::new();
            let bundled_lib_dir = self
                .shim_path
                .parent()
                .and_then(|dir| dir.parent())
                .map(|dir| dir.join("lib"));
            if let Some(path) = bundled_lib_dir.filter(|path| path.exists()) {
                dylib_paths.push(path);
            }
            let home_lib_dir = a3s_box_core::dirs_home().join("lib");
            if home_lib_dir.exists() {
                dylib_paths.push(home_lib_dir);
            }
            if let Some(existing) = std::env::var_os("DYLD_LIBRARY_PATH") {
                dylib_paths.extend(std::env::split_paths(&existing));
            } else {
                dylib_paths.push(std::path::PathBuf::from("/opt/homebrew/lib"));
            }
            if let Ok(joined) = std::env::join_paths(dylib_paths) {
                cmd.env("DYLD_LIBRARY_PATH", joined);
            }
        }

        #[cfg(target_os = "windows")]
        if let Some(path) = Self::windows_shim_path_env(&self.shim_path) {
            cmd.env("PATH", path);
        }

        let child = cmd.spawn().map_err(|e| BoxError::BoxBootError {
            message: format!("Failed to spawn shim: {}", e),
            hint: Some(format!("Shim path: {}", self.shim_path.display())),
        })?;

        let pid = child.id();
        tracing::info!(
            box_id = %spec.box_id,
            pid = pid,
            "Shim subprocess spawned"
        );

        // Create handler for the running VM
        let handler = ShimHandler::from_child(child, spec.box_id.clone());

        Ok(Box::new(handler))
    }
}
