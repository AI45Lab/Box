//! Guest init process for a3s-box VM.
//!
//! This process runs as PID 1 inside the MicroVM and is responsible for:
//! - Mounting essential filesystems (/proc, /sys, /dev)
//! - Mounting virtio-fs shares (workspace, user volumes)
//! - Mounting tmpfs volumes
//! - Configuring the guest network
//! - Launching the container entrypoint process
//! - Reaping zombie processes and handling SIGTERM for graceful shutdown

use a3s_box_guest_init::{
    attest_server, exec_server, host_config, namespace, network, port_forward, pty_server,
};
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
    /// Container user (`uid`, `uid:gid`, `root`, or a name resolved via the
    /// image `/etc/passwd`). Applied to the main process before exec.
    user: Option<String>,
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

        // Optional container user (image USER directive or CLI --user).
        let user = std::env::var("BOX_EXEC_USER")
            .ok()
            .filter(|u| !u.is_empty());

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
            user,
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

/// Raw fd of `/dev/kmsg`, opened ONCE before any chroot/pivot and kept open for
/// the process lifetime. An open file description survives `pivot_root`/`chroot`
/// (it is independent of the path), so reusing this fd avoids the gap where the
/// new root has no `/dev/kmsg` yet — which would otherwise leak a few lines back
/// to the console mid-boot.
static KMSG_FD: std::sync::OnceLock<Option<std::os::unix::io::RawFd>> = std::sync::OnceLock::new();

/// Writer for guest-init's OWN tracing. Routes it to the kernel log
/// (`/dev/kmsg`) instead of the VM console so it never pollutes container logs:
/// the container inherits the console for its stdout/stderr, and Docker-style
/// `logs` must show only that, not runtime internals (init/exec/pty chatter).
/// A `<7>` (debug) priority prefix keeps these lines below the guest kernel's
/// console loglevel (4), so they never echo back to the console. Falls back to
/// stdout when `/dev/kmsg` is unavailable (e.g. non-Linux), preserving the old
/// behavior rather than dropping logs.
enum InitLogWriter {
    Kmsg(std::os::unix::io::RawFd),
    Stdout(std::io::Stdout),
}

impl std::io::Write for InitLogWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        match self {
            InitLogWriter::Kmsg(fd) => {
                // /dev/kmsg treats each write() as one record: prefix the
                // priority and flatten embedded newlines so a formatted event
                // stays a single kernel-log record.
                let mut record = Vec::with_capacity(buf.len() + 13);
                record.extend_from_slice(b"<7>a3s-init: ");
                record.extend(buf.iter().map(|&b| if b == b'\n' { b' ' } else { b }));
                // SAFETY: *fd is a valid, process-lifetime fd to /dev/kmsg; a
                // failed write is intentionally ignored (logging must never panic).
                unsafe {
                    libc::write(*fd, record.as_ptr() as *const libc::c_void, record.len());
                }
                Ok(buf.len())
            }
            InitLogWriter::Stdout(out) => out.write(buf),
        }
    }

    fn flush(&mut self) -> std::io::Result<()> {
        match self {
            InitLogWriter::Kmsg(_) => Ok(()),
            InitLogWriter::Stdout(out) => out.flush(),
        }
    }
}

fn make_init_log_writer() -> InitLogWriter {
    match KMSG_FD.get().copied().flatten() {
        Some(fd) => InitLogWriter::Kmsg(fd),
        None => InitLogWriter::Stdout(std::io::stdout()),
    }
}

fn main() {
    // Open /dev/kmsg once (before any chroot) and keep it open for the whole
    // process via into_raw_fd, so guest-init's logs reach the kernel log
    // reliably across the pivot. Container logs stay clean (see InitLogWriter).
    use std::os::unix::io::IntoRawFd;
    let kmsg_fd = std::fs::OpenOptions::new()
        .write(true)
        .open("/dev/kmsg")
        .ok()
        .map(|file| file.into_raw_fd());
    let _ = KMSG_FD.set(kmsg_fd);

    // Initialize logging. guest-init's own logs go to the kernel log, NOT the
    // console, to keep container logs clean.
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_ansi(false)
        .with_writer(make_init_log_writer)
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

    // Step 2.25: Mount devpts after the final rootfs is active so PTY
    // allocation inside exec/attach sessions can open /dev/ptmx.
    mount_devpts()?;

    // Step 2.5: Mount tmpfs volumes
    mount_tmpfs_volumes()?;

    // Step 3: Configure guest network (if passt mode is active).
    // Network setup may write /etc/resolv.conf — must run before read-only remount.
    network::configure_guest_network()?;

    // Step 3.25: Apply hostname while the rootfs is still writable.
    host_config::apply_from_env()?;

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

    // Ensure the working directory exists — Docker creates a missing WORKDIR /
    // `-w` path before chdir. Best-effort: a pre-existing dir is fine, and a
    // read-only rootfs (where creation fails) matches Docker's inability to
    // create it there.
    if !exec_config.workdir.is_empty() && exec_config.workdir != "/" {
        if let Err(e) = std::fs::create_dir_all(&exec_config.workdir) {
            warn!(
                workdir = %exec_config.workdir,
                error = %e,
                "Could not pre-create working directory (continuing)"
            );
        }
    }

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
        exec_config.user.as_deref(),
    )?;
    let container_pid = nix::unistd::Pid::from_raw(container_pid_raw as i32);

    info!("Container process started with PID {}", container_pid);

    // Make the main container PID available to the exec server so a host
    // graceful-stop request (signal-main control frame) can deliver the
    // STOPSIGNAL to it. Must be set before the exec server thread starts.
    exec_server::set_container_pid(container_pid_raw as i32);

    expose_container_env_to_exec(&exec_config);

    // Step 8: Start exec server in background thread
    std::thread::spawn(|| {
        if let Err(e) = exec_server::run_exec_server() {
            error!("Exec server failed: {}", e);
        }
    });

    // Step 8.25: Start Windows host-port forward control client when enabled.
    std::thread::spawn(|| {
        if let Err(e) = port_forward::run_port_forward_client() {
            error!("Port-forward client failed: {}", e);
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

fn expose_container_env_to_exec(config: &ExecConfig) {
    for (key, value) in &config.env {
        if key.is_empty() || key.contains(['=', '\0']) || value.contains('\0') {
            warn!(key, "Skipping invalid container environment entry for exec");
            continue;
        }
        std::env::set_var(key, value);
    }
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
        .next_back()
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

/// Mount devpts for guest-side PTY allocation.
#[cfg(target_os = "linux")]
fn mount_devpts() -> Result<(), Box<dyn std::error::Error>> {
    use nix::mount::{mount, MsFlags};

    std::fs::create_dir_all("/dev/pts")?;
    match mount(
        Some("devpts"),
        "/dev/pts",
        Some("devpts"),
        MsFlags::empty(),
        Some("mode=0620,ptmxmode=0666"),
    ) {
        Ok(()) => {
            info!("Mounted devpts at /dev/pts");
            Ok(())
        }
        Err(nix::errno::Errno::EBUSY) => {
            info!("/dev/pts already mounted, skipping");
            Ok(())
        }
        Err(e) => Err(e.into()),
    }
}

#[cfg(not(target_os = "linux"))]
fn mount_devpts() -> Result<(), Box<dyn std::error::Error>> {
    Ok(())
}

/// Mount virtio-fs shares for workspace and user volumes.
fn mount_virtio_fs_shares() -> Result<(), Box<dyn std::error::Error>> {
    info!("Mounting virtio-fs shares");

    #[cfg(target_os = "linux")]
    {
        use nix::mount::{mount, MsFlags};

        // CRITICAL: Mount the root filesystem first
        // libkrun's krun_set_root() adds a virtiofs device with tag "/dev/root"
        // We need to check if this device exists and mount it
        info!("Checking for root filesystem virtiofs device");

        // Check if /dev/root virtiofs is available by trying to mount it to a temp location
        std::fs::create_dir_all("/mnt/newroot").ok();

        match mount(
            Some("/dev/root"),
            "/mnt/newroot",
            Some("virtiofs"),
            MsFlags::empty(),
            None::<&str>,
        ) {
            Ok(_) => {
                info!("Successfully mounted /dev/root to /mnt/newroot");

                // Now we need to pivot to the new root
                // First, move essential mounts to the new root
                std::fs::create_dir_all("/mnt/newroot/proc").ok();
                std::fs::create_dir_all("/mnt/newroot/sys").ok();
                std::fs::create_dir_all("/mnt/newroot/dev").ok();

                // Move mounts: MS_PRIVATE first to allow MS_MOVE on shared mounts (sysfs).
                let mut proc_moved = false;
                let mut sys_moved = false;
                let mut dev_moved = false;

                // Make mounts private so MS_MOVE works
                let _ = mount(
                    Some(""),
                    "/proc",
                    None::<&str>,
                    MsFlags::MS_PRIVATE,
                    None::<&str>,
                );
                let _ = mount(
                    Some(""),
                    "/sys",
                    None::<&str>,
                    MsFlags::MS_PRIVATE | MsFlags::MS_REC,
                    None::<&str>,
                );
                let _ = mount(
                    Some(""),
                    "/dev",
                    None::<&str>,
                    MsFlags::MS_PRIVATE,
                    None::<&str>,
                );

                if let Err(e) = mount(
                    Some("/proc"),
                    "/mnt/newroot/proc",
                    None::<&str>,
                    MsFlags::MS_MOVE,
                    None::<&str>,
                ) {
                    warn!("Failed to move /proc: {}", e);
                } else {
                    proc_moved = true;
                }

                if let Err(e) = mount(
                    Some("/sys"),
                    "/mnt/newroot/sys",
                    None::<&str>,
                    MsFlags::MS_MOVE,
                    None::<&str>,
                ) {
                    warn!("Failed to move /sys: {}", e);
                } else {
                    sys_moved = true;
                }

                if let Err(e) = mount(
                    Some("/dev"),
                    "/mnt/newroot/dev",
                    None::<&str>,
                    MsFlags::MS_MOVE,
                    None::<&str>,
                ) {
                    warn!("Failed to move /dev: {}", e);
                } else {
                    dev_moved = true;
                }

                // Change directory to new root
                std::env::set_current_dir("/mnt/newroot")?;

                // Pivot root via chroot
                use nix::unistd::{chdir, chroot};
                chroot("/mnt/newroot")?;
                chdir("/")?;

                // Re-mount any filesystems that couldn't be moved (MS_MOVE failed).
                // This ensures /proc, /sys, /dev are available in the new rootfs.
                if !proc_moved {
                    if let Err(e) = mount(
                        Some("proc"),
                        "/proc",
                        Some("proc"),
                        MsFlags::empty(),
                        None::<&str>,
                    ) {
                        warn!("Failed to remount /proc after chroot: {}", e);
                    }
                }
                if !sys_moved {
                    if let Err(e) = mount(
                        Some("sysfs"),
                        "/sys",
                        Some("sysfs"),
                        MsFlags::empty(),
                        None::<&str>,
                    ) {
                        warn!("Failed to remount /sys after chroot: {}", e);
                    } else {
                        info!("Remounted /sys after chroot (MS_MOVE failed)");
                    }
                }
                if !dev_moved {
                    if let Err(e) = mount(
                        Some("devtmpfs"),
                        "/dev",
                        Some("devtmpfs"),
                        MsFlags::empty(),
                        None::<&str>,
                    ) {
                        warn!("Failed to remount /dev after chroot: {}", e);
                    }
                }

                info!("Successfully pivoted to new root filesystem");
            }
            Err(e) => {
                warn!("No /dev/root virtiofs device found or failed to mount: {}. Using existing root.", e);
                // This is OK - it means we're already on the correct root or root wasn't set via virtiofs
            }
        }

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
                // Flags after the guest path may appear in any order: "ro", "file".
                // The host decides "file" (it can stat the source); the guest obeys.
                let read_only = parts[2..].contains(&"ro");
                let is_file = parts[2..].contains(&"file");

                let flags = if read_only {
                    MsFlags::MS_RDONLY
                } else {
                    MsFlags::empty()
                };

                if is_file {
                    // Single-file bind mount. The shim shares a temp DIRECTORY
                    // containing the file (virtio-fs cannot share a bare file), so
                    // mount that share at a private location and bind just the file
                    // onto guest_path. This preserves the target's parent directory
                    // (e.g. /etc) instead of clobbering it with the share.
                    let file_name = guest_path.rsplit('/').next().unwrap_or(guest_path);
                    let private_mp = format!("/run/.a3s-filemounts/{}", index);
                    std::fs::create_dir_all(&private_mp)?;
                    mount(
                        Some(tag),
                        private_mp.as_str(),
                        Some("virtiofs"),
                        MsFlags::empty(),
                        None::<&str>,
                    )?;

                    let src = format!("{}/{}", private_mp, file_name);
                    if !std::path::Path::new(&src).exists() {
                        warn!("File mount source {} missing in share {}", src, tag);
                    }

                    // Ensure the target parent and an (empty) target file exist so
                    // the bind has somewhere to land.
                    if let Some(last_slash) = guest_path.rfind('/') {
                        let parent = &guest_path[..last_slash];
                        if !parent.is_empty() {
                            std::fs::create_dir_all(parent)?;
                        }
                    }
                    if !std::path::Path::new(guest_path).exists() {
                        std::fs::File::create(guest_path)?;
                    }

                    // Bind the file, then remount read-only if requested (a bind
                    // mount needs a separate MS_REMOUNT pass to apply MS_RDONLY).
                    mount(
                        Some(src.as_str()),
                        guest_path,
                        None::<&str>,
                        MsFlags::MS_BIND,
                        None::<&str>,
                    )?;
                    if read_only {
                        mount(
                            None::<&str>,
                            guest_path,
                            None::<&str>,
                            MsFlags::MS_BIND | MsFlags::MS_REMOUNT | MsFlags::MS_RDONLY,
                            None::<&str>,
                        )?;
                    }
                    info!(
                        tag = tag,
                        guest_path = guest_path,
                        read_only = read_only,
                        "Mounted file volume (bind; parent directory preserved)"
                    );
                } else {
                    // Directory mount: mount the virtio-fs share directly at guest_path.
                    std::fs::create_dir_all(guest_path)?;
                    mount(Some(tag), guest_path, Some("virtiofs"), flags, None::<&str>)?;
                    info!(
                        tag = tag,
                        guest_path = guest_path,
                        read_only = read_only,
                        "Mounted user volume"
                    );
                }

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

    // A direct `MS_REMOUNT|MS_RDONLY` of the virtio-fs root often fails with
    // EBUSY. Fall back to the bind-remount trick (bind / onto itself, then
    // remount that bind read-only), which succeeds where a direct remount
    // cannot. If both fail, log and continue WRITABLE — a non-enforced
    // --read-only is far less harmful than killing the container outright.
    let direct = mount(
        None::<&str>,
        "/",
        None::<&str>,
        MsFlags::MS_REMOUNT | MsFlags::MS_RDONLY,
        None::<&str>,
    );
    if direct.is_ok() {
        info!("Rootfs remounted read-only");
        return Ok(());
    }

    let bind = mount(Some("/"), "/", None::<&str>, MsFlags::MS_BIND, None::<&str>).and_then(|_| {
        mount(
            None::<&str>,
            "/",
            None::<&str>,
            MsFlags::MS_REMOUNT | MsFlags::MS_BIND | MsFlags::MS_RDONLY,
            None::<&str>,
        )
    });
    match bind {
        Ok(()) => info!("Rootfs remounted read-only (via bind)"),
        Err(error) => warn!(
            %error,
            direct_error = ?direct.err(),
            "Could not remount rootfs read-only; container runs writable"
        ),
    }
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn remount_rootfs_readonly() -> Result<(), Box<dyn std::error::Error>> {
    Ok(())
}

/// Wait for the main container process.
///
/// Exec and PTY requests run in other guest-init threads and wait for their
/// own child processes. The main supervision loop must not call waitpid(-1),
/// otherwise it can reap those children before the request handler observes
/// their exit status.
fn wait_for_children(container_pid: nix::unistd::Pid) -> Result<(), Box<dyn std::error::Error>> {
    use nix::sys::wait::{waitpid, WaitPidFlag, WaitStatus};

    /// Maximum time to wait for children after forwarding SIGTERM (5 seconds).
    const CHILD_SHUTDOWN_TIMEOUT_MS: u64 = 5000;

    info!("Waiting for container process {}", container_pid);

    loop {
        // Check if shutdown was requested via SIGTERM
        if SHUTDOWN_REQUESTED.load(Ordering::SeqCst) {
            info!("SIGTERM received, initiating graceful shutdown");
            graceful_shutdown(CHILD_SHUTDOWN_TIMEOUT_MS);
            return Ok(());
        }

        match waitpid(container_pid, Some(WaitPidFlag::WNOHANG)) {
            Ok(WaitStatus::Exited(pid, status)) => {
                info!("Container process {} exited with status {}", pid, status);
                persist_exit_code(status);
                process::exit(status);
            }
            Ok(WaitStatus::Signaled(pid, signal, _)) => {
                error!("Container process {} killed by signal {:?}", pid, signal);
                persist_exit_code(128 + signal as i32);
                process::exit(128 + signal as i32);
            }
            Ok(WaitStatus::StillAlive) => {
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
            Ok(_) => {
                // Other status, continue waiting
            }
            Err(nix::errno::Errno::ECHILD) => {
                info!("Container process {} is no longer a child", container_pid);
                break;
            }
            Err(e) => {
                return Err(format!("waitpid failed: {}", e).into());
            }
        }
    }

    Ok(())
}

/// Persist the container's exit code to the overlay rootfs so the host can read
/// it after the VM halts. libkrun's `start_enter` takes over and `exit()`s the
/// shim process, so the host cannot `waitpid` the VM for a detached `run -d`; the
/// CLI state reconcile instead reads `<box_dir>/upper/.a3s_exit_code`, which is
/// this file surfaced through the overlay upperdir. Best-effort, with `sync_all`
/// so the write reaches the host before PID 1 exits and the VM halts.
fn persist_exit_code(code: i32) {
    use std::io::Write;
    if let Ok(mut file) = std::fs::File::create("/.a3s_exit_code") {
        let _ = write!(file, "{code}");
        let _ = file.sync_all();
    }
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
