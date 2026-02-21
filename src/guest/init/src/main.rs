//! Guest init process for a3s-box VM.
//!
//! This process runs as PID 1 inside the MicroVM and is responsible for:
//! - Mounting essential filesystems (/proc, /sys, /dev)
//! - Mounting virtio-fs shares (workspace, user volumes)
//! - Mounting tmpfs volumes
//! - Configuring the guest network
//! - Launching the container entrypoint process
//! - Reaping zombie processes and handling SIGTERM for graceful shutdown

use a3s_box_guest_init::{attest_server, exec_server, namespace, network, pty_server};
use std::process;
use std::sync::atomic::{AtomicBool, Ordering};
use tracing::{error, info, warn};

/// Global flag set by the SIGTERM handler to request graceful shutdown.
static SHUTDOWN_REQUESTED: AtomicBool = AtomicBool::new(false);

/// Container entrypoint configuration parsed from environment variables.
struct ExecConfig {
    /// Container executable path
    executable: String,
    /// Container arguments
    args: Vec<String>,
    /// Container environment variables
    env: Vec<(String, String)>,
    /// Working directory
    workdir: String,
}

impl ExecConfig {
    /// Parse container entrypoint configuration from environment variables.
    ///
    /// Expected environment variables:
    /// - BOX_EXEC_EXEC: container executable path
    /// - BOX_EXEC_ARGC: number of arguments
    /// - BOX_EXEC_ARG_<n>: individual argument values
    /// - BOX_EXEC_ENV_*: container environment variables
    /// - BOX_EXEC_WORKDIR: working directory (defaults to "/")
    fn from_env() -> Self {
        let executable =
            std::env::var("BOX_EXEC_EXEC").unwrap_or_else(|_| "/sbin/init".to_string());

        // Parse args from individual env vars (BOX_EXEC_ARGC + BOX_EXEC_ARG_0..N)
        let args: Vec<String> = match std::env::var("BOX_EXEC_ARGC")
            .ok()
            .and_then(|s| s.parse::<usize>().ok())
        {
            Some(argc) => (0..argc)
                .filter_map(|i| std::env::var(format!("BOX_EXEC_ARG_{}", i)).ok())
                .collect(),
            None => vec![],
        };

        let workdir = std::env::var("BOX_EXEC_WORKDIR").unwrap_or_else(|_| "/".to_string());

        // Collect BOX_EXEC_ENV_* variables
        let env: Vec<(String, String)> = std::env::vars()
            .filter_map(|(key, value)| {
                key.strip_prefix("BOX_EXEC_ENV_")
                    .map(|stripped| (stripped.to_string(), value))
            })
            .collect();

        Self {
            executable,
            args,
            env,
            workdir,
        }
    }
}

/// Sidecar process configuration parsed from environment variables.
struct SidecarConfig {
    /// Sidecar image name (informational only inside the VM — binary is already in rootfs)
    image: String,
    /// Vsock port the sidecar listens on
    vsock_port: u32,
    /// Environment variables for the sidecar
    env: Vec<(String, String)>,
}

impl SidecarConfig {
    /// Parse sidecar configuration from environment variables.
    ///
    /// Returns `None` if `BOX_SIDECAR_IMAGE` is not set.
    fn from_env() -> Option<Self> {
        let image = std::env::var("BOX_SIDECAR_IMAGE").ok()?;
        if image.is_empty() {
            return None;
        }

        let vsock_port = std::env::var("BOX_SIDECAR_VSOCK_PORT")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(4092u32);

        let env_count: usize = std::env::var("BOX_SIDECAR_ENV_COUNT")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);

        let env: Vec<(String, String)> = (0..env_count)
            .filter_map(|i| {
                let raw = std::env::var(format!("BOX_SIDECAR_ENV_{}", i)).ok()?;
                let (key, value) = raw.split_once('=')?;
                Some((key.to_string(), value.to_string()))
            })
            .collect();

        Some(Self {
            image,
            vsock_port,
            env,
        })
    }
}

/// Register a SIGTERM handler that sets the shutdown flag.
///
/// As PID 1 inside the VM, we must explicitly handle SIGTERM — the kernel
/// does not deliver unhandled signals to init. When the host kills the shim
/// process, libkrun triggers a guest shutdown and the kernel sends SIGTERM
/// to PID 1.
#[cfg(target_os = "linux")]
fn register_sigterm_handler() -> Result<(), Box<dyn std::error::Error>> {
    use nix::sys::signal::{sigaction, SaFlags, SigAction, SigHandler, SigSet, Signal};

    let handler = SigHandler::Handler(sigterm_handler);
    let action = SigAction::new(handler, SaFlags::empty(), SigSet::empty());
    unsafe { sigaction(Signal::SIGTERM, &action)? };
    info!("Registered SIGTERM handler");
    Ok(())
}

#[cfg(target_os = "linux")]
extern "C" fn sigterm_handler(_: libc::c_int) {
    SHUTDOWN_REQUESTED.store(true, Ordering::SeqCst);
}

#[cfg(not(target_os = "linux"))]
fn register_sigterm_handler() -> Result<(), Box<dyn std::error::Error>> {
    info!("Skipping SIGTERM handler on non-Linux platform (development mode)");
    Ok(())
}

/// Check if this VM is running in a TEE environment.
///
/// Delegates to `a3s_box_core::tee::is_tee_available()` which checks
/// `A3S_TEE_SIMULATE` env var and `/dev/sev-guest` or `/dev/sev` devices.
fn is_tee_environment() -> bool {
    a3s_box_core::tee::is_tee_available()
}

fn main() {
    // Initialize logging
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    info!("a3s-box guest init starting (PID {})", process::id());

    // Run init process
    if let Err(e) = run_init() {
        error!("Init process failed: {}", e);
        process::exit(1);
    }

    info!("Init process completed successfully");
}

fn run_init() -> Result<(), Box<dyn std::error::Error>> {
    // Step 1: Mount essential filesystems
    mount_essential_filesystems()?;

    // Step 2: Mount virtio-fs shares
    mount_virtio_fs_shares()?;

    // Step 2.5: Mount tmpfs volumes
    mount_tmpfs_volumes()?;

    // Step 3: Configure guest network (if passt mode is active).
    // Network setup may write /etc/resolv.conf — must run before read-only remount.
    network::configure_guest_network()?;

    // Step 3.5: Remount rootfs read-only if BOX_READONLY=1.
    // All writes to / (mount point creation, resolv.conf) must complete first.
    remount_rootfs_readonly()?;

    // Step 4: Register SIGTERM handler before spawning any children
    register_sigterm_handler()?;

    // Step 5: Parse container entrypoint configuration from environment
    let exec_config = ExecConfig::from_env();
    info!(
        executable = %exec_config.executable,
        args = ?exec_config.args,
        workdir = %exec_config.workdir,
        env_count = exec_config.env.len(),
        "Container entrypoint configuration loaded"
    );

    // Step 6: Create namespace config (isolation disabled inside the MicroVM —
    // the VM boundary itself provides isolation, and unshare can interfere with
    // the lightweight kernel's limited namespace support)
    let namespace_config = namespace::NamespaceConfig {
        mount: false,
        pid: false,
        ipc: false,
        uts: false,
        net: false,
        user: false,
        cgroup: false,
    };

    // Step 6.5: Launch sidecar process (if configured)
    // The sidecar runs before the main container so it is ready to intercept
    // traffic when the agent starts. It is not waited on — it runs for the
    // lifetime of the VM and is reaped by the zombie-reaper loop.
    if let Some(sidecar) = SidecarConfig::from_env() {
        info!(
            image = %sidecar.image,
            vsock_port = sidecar.vsock_port,
            "Launching sidecar process"
        );
        launch_sidecar(&sidecar)?;
    }

    // Step 7: Launch container entrypoint
    info!("Launching container entrypoint");

    // Convert args to &str for spawn_isolated
    let args_refs: Vec<&str> = exec_config.args.iter().map(|s| s.as_str()).collect();
    let env_refs: Vec<(&str, &str)> = exec_config
        .env
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();

    let container_pid_raw = namespace::spawn_isolated(
        &namespace_config,
        &exec_config.executable,
        &args_refs,
        &env_refs,
        &exec_config.workdir,
    )?;
    let container_pid = nix::unistd::Pid::from_raw(container_pid_raw as i32);

    info!("Container process started with PID {}", container_pid);

    // Step 8: Start exec server in background thread
    std::thread::spawn(|| {
        if let Err(e) = exec_server::run_exec_server() {
            error!("Exec server failed: {}", e);
        }
    });

    // Step 8.5: Start PTY server in background thread
    std::thread::spawn(|| {
        if let Err(e) = pty_server::run_pty_server() {
            error!("PTY server failed: {}", e);
        }
    });

    // Step 8.6: Start attestation server in background thread (TEE environments only)
    // Only start if TEE simulation is enabled or real SEV-SNP hardware is present.
    if is_tee_environment() {
        std::thread::spawn(|| {
            if let Err(e) = attest_server::run_attest_server() {
                error!("Attestation server failed: {}", e);
            }
        });
    }

    // Step 9: Wait for agent process (reap zombies, handle SIGTERM)
    wait_for_children(container_pid)?;

    Ok(())
}

/// Launch the sidecar process as a background co-process.
///
/// The sidecar binary is expected to be present in the rootfs at a well-known
/// path. It is spawned with its configured environment variables and runs
/// independently of the main container process.
///
/// The sidecar is NOT waited on — it runs for the lifetime of the VM and is
/// reaped by the zombie-reaper loop in `wait_for_children`.
fn launch_sidecar(config: &SidecarConfig) -> Result<(), Box<dyn std::error::Error>> {
    // The sidecar binary path: conventionally /usr/bin/sidecar or derived from image name.
    // Inside the VM the sidecar image is already extracted into the rootfs by the runtime.
    // We look for the binary at /usr/bin/<basename> where basename is the last component
    // of the image reference (e.g., "safeclaw" from "ghcr.io/a3s-lab/safeclaw:latest").
    let binary_name = config
        .image
        .split('/')
        .last()
        .and_then(|s| s.split(':').next())
        .unwrap_or("sidecar");

    let binary_path = format!("/usr/bin/{}", binary_name);

    let mut cmd = std::process::Command::new(&binary_path);

    // Inject sidecar-specific env vars
    for (key, value) in &config.env {
        cmd.env(key, value);
    }

    // Pass vsock port so the sidecar knows where to listen
    cmd.env("SIDECAR_VSOCK_PORT", config.vsock_port.to_string());

    match cmd.spawn() {
        Ok(child) => {
            info!(
                binary = %binary_path,
                pid = child.id(),
                vsock_port = config.vsock_port,
                "Sidecar process launched"
            );
            // Intentionally leak the Child handle — the zombie-reaper loop
            // in wait_for_children will reap it when it exits.
            std::mem::forget(child);
            Ok(())
        }
        Err(e) => {
            // Non-fatal: log and continue. The main container should still start
            // even if the sidecar binary is missing (e.g., in development).
            warn!(
                binary = %binary_path,
                error = %e,
                "Failed to launch sidecar — continuing without it"
            );
            Ok(())
        }
    }
}

/// Mount essential filesystems (/proc, /sys, /dev).
fn mount_essential_filesystems() -> Result<(), Box<dyn std::error::Error>> {
    info!("Mounting essential filesystems");

    // Note: mount() signature differs between Linux and macOS in nix crate
    // On Linux: mount(source, target, fstype, flags, data)
    // On macOS: mount(source, target, flags, data)
    // This code is meant to run on Linux inside the VM

    #[cfg(target_os = "linux")]
    {
        use nix::mount::{mount, MsFlags};

        // Mount /proc (ignore EBUSY — kernel may have already mounted it)
        match mount(
            Some("proc"),
            "/proc",
            Some("proc"),
            MsFlags::empty(),
            None::<&str>,
        ) {
            Ok(()) => {}
            Err(nix::errno::Errno::EBUSY) => {
                info!("/proc already mounted, skipping");
            }
            Err(e) => return Err(e.into()),
        }

        // Mount /sys (ignore EBUSY)
        match mount(
            Some("sysfs"),
            "/sys",
            Some("sysfs"),
            MsFlags::empty(),
            None::<&str>,
        ) {
            Ok(()) => {}
            Err(nix::errno::Errno::EBUSY) => {
                info!("/sys already mounted, skipping");
            }
            Err(e) => return Err(e.into()),
        }

        // Mount /dev (devtmpfs, ignore EBUSY)
        match mount(
            Some("devtmpfs"),
            "/dev",
            Some("devtmpfs"),
            MsFlags::empty(),
            None::<&str>,
        ) {
            Ok(()) => {}
            Err(nix::errno::Errno::EBUSY) => {
                info!("/dev already mounted, skipping");
            }
            Err(e) => return Err(e.into()),
        }
    }

    #[cfg(not(target_os = "linux"))]
    {
        // On non-Linux platforms (e.g., macOS for development),
        // skip mounting as this code won't actually run
        info!("Skipping mount on non-Linux platform (development mode)");
    }

    Ok(())
}

/// Mount virtio-fs shares for workspace and user volumes.
fn mount_virtio_fs_shares() -> Result<(), Box<dyn std::error::Error>> {
    info!("Mounting virtio-fs shares");

    #[cfg(target_os = "linux")]
    {
        use nix::mount::{mount, MsFlags};

        // Ensure workspace mount point exists
        std::fs::create_dir_all("/workspace").ok();

        // Mount workspace share
        mount(
            Some("workspace"),
            "/workspace",
            Some("virtiofs"),
            MsFlags::empty(),
            None::<&str>,
        )?;

        // Mount user-defined volumes from environment variables.
        // Format: BOX_VOL_<index>=<tag>:<guest_path>[:ro]
        mount_user_volumes()?;
    }

    #[cfg(not(target_os = "linux"))]
    {
        info!("Skipping virtio-fs mount on non-Linux platform (development mode)");
    }

    Ok(())
}

/// Mount user-defined volumes passed via BOX_VOL_* environment variables.
///
/// Each variable has the format: `<tag>:<guest_path>[:ro]`
#[cfg(target_os = "linux")]
fn mount_user_volumes() -> Result<(), Box<dyn std::error::Error>> {
    use nix::mount::{mount, MsFlags};

    let mut index = 0;
    loop {
        let env_key = format!("BOX_VOL_{}", index);
        match std::env::var(&env_key) {
            Ok(value) => {
                let parts: Vec<&str> = value.split(':').collect();
                if parts.len() < 2 {
                    error!("Invalid volume spec in {}: {}", env_key, value);
                    index += 1;
                    continue;
                }

                let tag = parts[0];
                let guest_path = parts[1];
                let read_only = parts.get(2).map(|&m| m == "ro").unwrap_or(false);

                info!(
                    tag = tag,
                    guest_path = guest_path,
                    read_only = read_only,
                    "Mounting user volume"
                );

                // Ensure mount point exists
                std::fs::create_dir_all(guest_path)?;

                let flags = if read_only {
                    MsFlags::MS_RDONLY
                } else {
                    MsFlags::empty()
                };
                mount(Some(tag), guest_path, Some("virtiofs"), flags, None::<&str>)?;

                index += 1;
            }
            Err(_) => break,
        }
    }

    if index > 0 {
        info!("Mounted {} user volume(s)", index);
    }

    Ok(())
}

/// Mount tmpfs volumes passed via BOX_TMPFS_* environment variables.
///
/// Each variable has the format: `<path>[:<options>]`
/// Options are passed directly to mount (e.g., "size=100m").
fn mount_tmpfs_volumes() -> Result<(), Box<dyn std::error::Error>> {
    #[cfg(target_os = "linux")]
    {
        use nix::mount::{mount, MsFlags};

        let mut index = 0;
        loop {
            let env_key = format!("BOX_TMPFS_{}", index);
            match std::env::var(&env_key) {
                Ok(value) => {
                    // Format: "/path" or "/path:options"
                    let (path, options) = match value.split_once(':') {
                        Some((p, opts)) => (p, Some(opts.to_string())),
                        None => (value.as_str(), None),
                    };

                    info!(
                        path = path,
                        options = ?options,
                        "Mounting tmpfs"
                    );

                    // Ensure mount point exists
                    std::fs::create_dir_all(path)?;

                    mount(
                        None::<&str>,
                        path,
                        Some("tmpfs"),
                        MsFlags::empty(),
                        options.as_deref(),
                    )?;

                    index += 1;
                }
                Err(_) => break,
            }
        }

        if index > 0 {
            info!("Mounted {} tmpfs volume(s)", index);
        }
    }

    #[cfg(not(target_os = "linux"))]
    {
        info!("Skipping tmpfs mount on non-Linux platform (development mode)");
    }

    Ok(())
}

/// Remount the container rootfs as read-only if `BOX_READONLY=1` is set.
///
/// Called after all filesystem setup (mounts, network config) so that no
/// further writes to `/` are needed before the container process launches.
/// Virtiofs and tmpfs shares are separate mountpoints and remain writable.
#[cfg(target_os = "linux")]
fn remount_rootfs_readonly() -> Result<(), Box<dyn std::error::Error>> {
    if std::env::var("BOX_READONLY").as_deref() != Ok("1") {
        return Ok(());
    }

    use nix::mount::{mount, MsFlags};

    info!("Remounting rootfs as read-only (--read-only)");
    mount(
        None::<&str>,
        "/",
        None::<&str>,
        MsFlags::MS_REMOUNT | MsFlags::MS_RDONLY,
        None::<&str>,
    )?;
    info!("Rootfs remounted read-only");
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn remount_rootfs_readonly() -> Result<(), Box<dyn std::error::Error>> {
    Ok(())
}

/// Wait for all child processes and reap zombies.
///
/// Only exits when `container_pid` exits. Other child processes (e.g., from
/// exec server) are reaped silently.
fn wait_for_children(container_pid: nix::unistd::Pid) -> Result<(), Box<dyn std::error::Error>> {
    use nix::sys::wait::{waitpid, WaitPidFlag, WaitStatus};
    use nix::unistd::Pid;

    /// Maximum time to wait for children after forwarding SIGTERM (5 seconds).
    const CHILD_SHUTDOWN_TIMEOUT_MS: u64 = 5000;

    info!("Waiting for child processes");

    loop {
        // Check if shutdown was requested via SIGTERM
        if SHUTDOWN_REQUESTED.load(Ordering::SeqCst) {
            info!("SIGTERM received, initiating graceful shutdown");
            graceful_shutdown(CHILD_SHUTDOWN_TIMEOUT_MS);
            return Ok(());
        }

        match waitpid(Pid::from_raw(-1), Some(WaitPidFlag::WNOHANG)) {
            Ok(WaitStatus::Exited(pid, status)) => {
                if pid == container_pid {
                    info!("Container process {} exited with status {}", pid, status);
                    process::exit(status);
                } else {
                    info!(
                        "Child process {} exited with status {} (reaped)",
                        pid, status
                    );
                }
            }
            Ok(WaitStatus::Signaled(pid, signal, _)) => {
                if pid == container_pid {
                    error!("Container process {} killed by signal {:?}", pid, signal);
                    process::exit(128 + signal as i32);
                } else if SHUTDOWN_REQUESTED.load(Ordering::SeqCst) {
                    info!(
                        "Child process {} terminated by signal {:?} during shutdown",
                        pid, signal
                    );
                } else {
                    info!(
                        "Child process {} killed by signal {:?} (reaped)",
                        pid, signal
                    );
                }
            }
            Ok(WaitStatus::StillAlive) => {
                // No children to reap, sleep briefly
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
            Ok(_) => {
                // Other status, continue waiting
            }
            Err(nix::errno::Errno::ECHILD) => {
                // No more children
                info!("No more child processes");
                break;
            }
            Err(e) => {
                return Err(format!("waitpid failed: {}", e).into());
            }
        }
    }

    Ok(())
}

/// Perform graceful shutdown: forward SIGTERM to children, wait, then force-kill.
fn graceful_shutdown(timeout_ms: u64) {
    // Step 1: Send SIGTERM to all processes (except ourselves, PID 1)
    #[cfg(target_os = "linux")]
    {
        info!("Forwarding SIGTERM to all child processes");
        // kill(-1, SIGTERM) sends to all processes except PID 1
        unsafe {
            libc::kill(-1, libc::SIGTERM);
        }
    }

    // Step 2: Wait for children to exit with timeout
    let start = std::time::Instant::now();
    loop {
        use nix::sys::wait::{waitpid, WaitPidFlag, WaitStatus};
        use nix::unistd::Pid;

        match waitpid(Pid::from_raw(-1), Some(WaitPidFlag::WNOHANG)) {
            Ok(WaitStatus::Exited(pid, status)) => {
                info!(
                    "Child {} exited with status {} during shutdown",
                    pid, status
                );
            }
            Ok(WaitStatus::Signaled(pid, signal, _)) => {
                info!("Child {} terminated by {:?} during shutdown", pid, signal);
            }
            Ok(WaitStatus::StillAlive) => {
                if start.elapsed().as_millis() > timeout_ms as u128 {
                    warn!("Shutdown timeout reached, sending SIGKILL to remaining children");
                    #[cfg(target_os = "linux")]
                    unsafe {
                        libc::kill(-1, libc::SIGKILL);
                    }
                    // Reap any remaining
                    loop {
                        match waitpid(Pid::from_raw(-1), Some(WaitPidFlag::WNOHANG)) {
                            Ok(WaitStatus::StillAlive) | Err(nix::errno::Errno::ECHILD) => break,
                            _ => continue,
                        }
                    }
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            Ok(_) => {
                // Other status, continue
            }
            Err(nix::errno::Errno::ECHILD) => {
                info!("All children exited during shutdown");
                break;
            }
            Err(e) => {
                warn!("waitpid error during shutdown: {}", e);
                break;
            }
        }
    }

    // Step 3: Sync filesystem buffers
    info!("Syncing filesystem buffers");
    #[cfg(target_os = "linux")]
    unsafe {
        libc::sync();
    }

    info!("Graceful shutdown complete");
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set_sidecar_env(image: &str, vsock_port: u32, env: &[(&str, &str)]) {
        std::env::set_var("BOX_SIDECAR_IMAGE", image);
        std::env::set_var("BOX_SIDECAR_VSOCK_PORT", vsock_port.to_string());
        std::env::set_var("BOX_SIDECAR_ENV_COUNT", env.len().to_string());
        for (i, (k, v)) in env.iter().enumerate() {
            std::env::set_var(format!("BOX_SIDECAR_ENV_{}", i), format!("{}={}", k, v));
        }
    }

    fn clear_sidecar_env() {
        std::env::remove_var("BOX_SIDECAR_IMAGE");
        std::env::remove_var("BOX_SIDECAR_VSOCK_PORT");
        std::env::remove_var("BOX_SIDECAR_ENV_COUNT");
        for i in 0..10 {
            std::env::remove_var(format!("BOX_SIDECAR_ENV_{}", i));
        }
    }

    /// All sidecar env tests run sequentially in a single test to avoid
    /// env var race conditions (env vars are process-global).
    #[test]
    fn test_sidecar_config_from_env() {
        // Subtest 1: no env vars → None
        clear_sidecar_env();
        assert!(SidecarConfig::from_env().is_none());

        // Subtest 2: empty image → None
        std::env::set_var("BOX_SIDECAR_IMAGE", "");
        assert!(SidecarConfig::from_env().is_none());
        std::env::remove_var("BOX_SIDECAR_IMAGE");

        // Subtest 3: basic config
        set_sidecar_env("safeclaw:latest", 4092, &[]);
        let config = SidecarConfig::from_env().unwrap();
        assert_eq!(config.image, "safeclaw:latest");
        assert_eq!(config.vsock_port, 4092);
        assert!(config.env.is_empty());
        clear_sidecar_env();

        // Subtest 4: with env vars
        set_sidecar_env(
            "ghcr.io/a3s-lab/safeclaw:latest",
            4092,
            &[("LOG_LEVEL", "debug"), ("MODE", "proxy")],
        );
        let config = SidecarConfig::from_env().unwrap();
        assert_eq!(config.image, "ghcr.io/a3s-lab/safeclaw:latest");
        assert_eq!(config.env.len(), 2);
        assert_eq!(
            config.env[0],
            ("LOG_LEVEL".to_string(), "debug".to_string())
        );
        assert_eq!(config.env[1], ("MODE".to_string(), "proxy".to_string()));
        clear_sidecar_env();

        // Subtest 5: default vsock port
        std::env::set_var("BOX_SIDECAR_IMAGE", "safeclaw:latest");
        std::env::remove_var("BOX_SIDECAR_VSOCK_PORT");
        std::env::remove_var("BOX_SIDECAR_ENV_COUNT");
        let config = SidecarConfig::from_env().unwrap();
        assert_eq!(config.vsock_port, 4092);
        clear_sidecar_env();

        // Subtest 6: custom vsock port
        set_sidecar_env("safeclaw:latest", 5000, &[]);
        let config = SidecarConfig::from_env().unwrap();
        assert_eq!(config.vsock_port, 5000);
        clear_sidecar_env();
    }
}
