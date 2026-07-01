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
    fn configure_shim_stdio(&self, cmd: &mut Command, spec: &InstanceSpec) {
        use std::fs::OpenOptions;

        let Some(console_output) = spec.console_output.as_ref() else {
            cmd.stdout(Stdio::null()).stderr(Stdio::null());
            return;
        };
        let Some(log_dir) = console_output.parent() else {
            cmd.stdout(Stdio::null()).stderr(Stdio::null());
            return;
        };
        if let Err(error) = std::fs::create_dir_all(log_dir) {
            tracing::warn!(
                box_id = %spec.box_id,
                path = %log_dir.display(),
                error = %error,
                "Failed to create shim log directory"
            );
            cmd.stdout(Stdio::null()).stderr(Stdio::null());
            return;
        }

        let stdout_path = log_dir.join("shim.stdout.log");
        let stderr_path = log_dir.join("shim.stderr.log");

        let stdout_file = OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(&stdout_path);
        let stderr_file = OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(&stderr_path);

        match (stdout_file, stderr_file) {
            (Ok(stdout_file), Ok(stderr_file)) => {
                tracing::debug!(
                    box_id = %spec.box_id,
                    stdout = %stdout_path.display(),
                    stderr = %stderr_path.display(),
                    "Redirecting shim stdio to per-box files"
                );
                cmd.stdout(Stdio::from(stdout_file))
                    .stderr(Stdio::from(stderr_file));
            }
            (stdout_result, stderr_result) => {
                if let Err(error) = stdout_result {
                    tracing::warn!(
                        box_id = %spec.box_id,
                        path = %stdout_path.display(),
                        error = %error,
                        "Failed to open shim stdout log file"
                    );
                }
                if let Err(error) = stderr_result {
                    tracing::warn!(
                        box_id = %spec.box_id,
                        path = %stderr_path.display(),
                        error = %error,
                        "Failed to open shim stderr log file"
                    );
                }
                cmd.stdout(Stdio::null()).stderr(Stdio::null());
            }
        }
    }

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
            dirs.push(dir.join("lib"));
        }

        let mut seen = HashSet::new();
        let mut path_entries = Vec::new();
        for dir in dirs {
            if !seen.insert(dir.clone()) {
                continue;
            }
            if dir.join("krun.dll").exists() {
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
        cmd.arg("--config").arg(&config_json).stdin(Stdio::null());
        self.configure_shim_stdio(&mut cmd, spec);

        // KSM page-merging: the shim opts its (guest) memory in via prctl when this
        // env is set; driven by InstanceSpec.ksm (BoxConfig.ksm or A3S_BOX_KSM).
        if spec.ksm {
            cmd.env("A3S_BOX_KSM", "1");
        }

        // Snapshot-fork: set the file-backed-RAM / snapshot-trigger / restore paths
        // for the shim/libkrun. PER-VM values from the InstanceSpec take precedence —
        // this is what lets ONE process (the pool / fork daemon) drive a different
        // template/restore per VM, which a process-global env cannot. Fall back to the
        // process env only when the spec doesn't set a given var (single-VM `run`).
        let snap_env: [(&str, Option<&str>); 3] = [
            ("KRUN_SNAPSHOT_MEM_FILE", spec.snapshot_mem_file.as_deref()),
            ("KRUN_SNAPSHOT_SOCK", spec.snapshot_sock.as_deref()),
            ("KRUN_RESTORE_FROM", spec.restore_from.as_deref()),
        ];
        for (var, spec_val) in snap_env {
            match spec_val {
                Some(val) if !val.is_empty() => {
                    cmd.env(var, val);
                }
                _ => {
                    if let Ok(val) = std::env::var(var) {
                        if !val.is_empty() {
                            cmd.env(var, val);
                        }
                    }
                }
            }
        }

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

        // Put the VMM shim in its own session/process-group so it survives teardown
        // of the launcher's session — e.g. a containerd-shim foreground `a3s-box run`
        // whose process group is reaped on container kill. Without this the libkrun
        // shim (which owns the box's exec.sock) dies with the launcher and `a3s-box
        // exec` fails with "exec socket missing".
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            unsafe {
                cmd.pre_exec(|| {
                    // setsid() fails harmlessly if already a group leader; ignore.
                    libc::setsid();
                    Ok(())
                });
            }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    fn make_fake_shim(dir: &std::path::Path) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;

        let shim_path = dir.join("fake-a3s-box-shim");
        std::fs::write(
            &shim_path,
            r#"#!/bin/sh
printf '%s\n' "$@" > "$A3S_TEST_ARGS_FILE"
printf '%s\n' "$A3S_BOX_KSM" > "$A3S_TEST_KSM_FILE"
printf '%s\n' "$KRUN_SNAPSHOT_MEM_FILE" > "$A3S_TEST_SNAPSHOT_MEM_FILE"
printf '%s\n' "$KRUN_SNAPSHOT_SOCK" > "$A3S_TEST_SNAPSHOT_SOCK_FILE"
printf '%s\n' "$KRUN_RESTORE_FROM" > "$A3S_TEST_RESTORE_FILE"
printf shim-stdout
printf shim-stderr >&2
exec /bin/sleep 30
"#,
        )
        .unwrap();
        std::fs::set_permissions(&shim_path, std::fs::Permissions::from_mode(0o755)).unwrap();
        shim_path
    }

    #[cfg(unix)]
    fn wait_for_file(path: &std::path::Path) {
        for _ in 0..250 {
            if path.exists() {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        panic!("expected file to appear: {}", path.display());
    }

    #[test]
    fn new_reports_missing_shim_with_build_hint() {
        let missing = tempfile::tempdir()
            .unwrap()
            .path()
            .join("missing-a3s-box-shim");

        let error = match VmController::new(missing.clone()) {
            Ok(_) => panic!("missing shim should be rejected"),
            Err(error) => error,
        };
        let message = error.to_string();

        assert!(message.contains("Shim binary not found"));
        assert!(message.contains(&missing.display().to_string()));
        assert!(message.contains("cargo build -p a3s-box-shim"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn start_spawns_shim_with_config_env_and_stdio() {
        let temp = tempfile::tempdir().unwrap();
        let fake_shim = make_fake_shim(temp.path());
        let controller = VmController {
            shim_path: fake_shim,
        };

        let args_file = temp.path().join("shim.args");
        let ksm_file = temp.path().join("shim.ksm");
        let snapshot_mem_file = temp.path().join("shim.snapshot_mem");
        let snapshot_sock_file = temp.path().join("shim.snapshot_sock");
        let restore_file = temp.path().join("shim.restore");

        std::env::set_var("A3S_TEST_ARGS_FILE", &args_file);
        std::env::set_var("A3S_TEST_KSM_FILE", &ksm_file);
        std::env::set_var("A3S_TEST_SNAPSHOT_MEM_FILE", &snapshot_mem_file);
        std::env::set_var("A3S_TEST_SNAPSHOT_SOCK_FILE", &snapshot_sock_file);
        std::env::set_var("A3S_TEST_RESTORE_FILE", &restore_file);

        let socket_dir = temp.path().join("runtime").join("sockets");
        let spec = InstanceSpec {
            box_id: "box-start".to_string(),
            exec_socket_path: socket_dir.join("exec.sock"),
            console_output: Some(temp.path().join("logs").join("console.log")),
            ksm: true,
            snapshot_mem_file: Some("/tmp/a3s-mem".to_string()),
            snapshot_sock: Some("/tmp/a3s-snapshot.sock".to_string()),
            restore_from: Some("/tmp/a3s-restore".to_string()),
            ..Default::default()
        };

        let mut handler = controller.start(&spec).await.unwrap();
        wait_for_file(&args_file);

        assert!(socket_dir.exists());
        let args = std::fs::read_to_string(&args_file).unwrap();
        assert!(args.contains("--config"));
        assert!(args.contains("\"box_id\":\"box-start\""));
        assert_eq!(std::fs::read_to_string(&ksm_file).unwrap().trim(), "1");
        assert_eq!(
            std::fs::read_to_string(&snapshot_mem_file).unwrap().trim(),
            "/tmp/a3s-mem"
        );
        assert_eq!(
            std::fs::read_to_string(&snapshot_sock_file).unwrap().trim(),
            "/tmp/a3s-snapshot.sock"
        );
        assert_eq!(
            std::fs::read_to_string(&restore_file).unwrap().trim(),
            "/tmp/a3s-restore"
        );
        assert_eq!(
            std::fs::read_to_string(temp.path().join("logs").join("shim.stdout.log")).unwrap(),
            "shim-stdout"
        );
        assert_eq!(
            std::fs::read_to_string(temp.path().join("logs").join("shim.stderr.log")).unwrap(),
            "shim-stderr"
        );

        handler.stop(libc::SIGTERM, 1_000).unwrap();

        std::env::remove_var("A3S_TEST_ARGS_FILE");
        std::env::remove_var("A3S_TEST_KSM_FILE");
        std::env::remove_var("A3S_TEST_SNAPSHOT_MEM_FILE");
        std::env::remove_var("A3S_TEST_SNAPSHOT_SOCK_FILE");
        std::env::remove_var("A3S_TEST_RESTORE_FILE");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn start_reports_socket_directory_creation_failure() {
        let temp = tempfile::tempdir().unwrap();
        let controller = VmController {
            shim_path: PathBuf::from("/bin/sh"),
        };
        let socket_dir = temp.path().join("socket-dir-is-file");
        std::fs::write(&socket_dir, "not a directory").unwrap();
        let spec = InstanceSpec {
            box_id: "box-start-error".to_string(),
            exec_socket_path: socket_dir.join("exec.sock"),
            ..Default::default()
        };

        let err = match controller.start(&spec).await {
            Ok(_) => panic!("socket directory creation should fail before spawning the shim"),
            Err(err) => err,
        };

        assert!(err
            .to_string()
            .contains("Failed to create socket directory"));
    }

    #[cfg(unix)]
    #[test]
    fn configure_shim_stdio_writes_per_box_stdout_and_stderr_logs() {
        let temp = tempfile::tempdir().unwrap();
        let controller = VmController {
            shim_path: PathBuf::from("/bin/sh"),
        };
        let spec = InstanceSpec {
            box_id: "box-stdio".to_string(),
            console_output: Some(temp.path().join("logs").join("console.log")),
            ..Default::default()
        };

        let mut cmd = Command::new("sh");
        cmd.arg("-c")
            .arg("printf fresh-stdout; printf fresh-stderr >&2");

        std::fs::create_dir_all(temp.path().join("logs")).unwrap();
        std::fs::write(temp.path().join("logs").join("shim.stdout.log"), "stale").unwrap();
        std::fs::write(temp.path().join("logs").join("shim.stderr.log"), "stale").unwrap();

        controller.configure_shim_stdio(&mut cmd, &spec);
        let status = cmd.status().unwrap();

        assert!(status.success());
        assert_eq!(
            std::fs::read_to_string(temp.path().join("logs").join("shim.stdout.log")).unwrap(),
            "fresh-stdout"
        );
        assert_eq!(
            std::fs::read_to_string(temp.path().join("logs").join("shim.stderr.log")).unwrap(),
            "fresh-stderr"
        );
    }

    #[cfg(unix)]
    #[test]
    fn configure_shim_stdio_creates_missing_log_directory() {
        let temp = tempfile::tempdir().unwrap();
        let controller = VmController {
            shim_path: PathBuf::from("/bin/sh"),
        };
        let spec = InstanceSpec {
            box_id: "box-stdio-dir".to_string(),
            console_output: Some(temp.path().join("missing").join("console.log")),
            ..Default::default()
        };

        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg("printf out; printf err >&2");

        controller.configure_shim_stdio(&mut cmd, &spec);
        let status = cmd.status().unwrap();

        assert!(status.success());
        assert_eq!(
            std::fs::read_to_string(temp.path().join("missing").join("shim.stdout.log")).unwrap(),
            "out"
        );
        assert_eq!(
            std::fs::read_to_string(temp.path().join("missing").join("shim.stderr.log")).unwrap(),
            "err"
        );
    }
}
