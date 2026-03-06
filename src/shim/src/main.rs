//! A3S Box Shim - MicroVM subprocess for process isolation.
//!
//! This binary is spawned by VmController to isolate the VM from the host application.
//! libkrun's `krun_start_enter()` performs process takeover, so we need a separate
//! process to prevent the host application from being taken over.
//!
//! # Usage
//! ```bash
//! a3s-box-shim --config '{"box_id": "...", ...}'
//! ```

// Allow large error types - this is a binary, not a library
#![allow(clippy::result_large_err)]

use a3s_box_core::error::{BoxError, Result};
use a3s_box_runtime::krun::KrunContext;
use a3s_box_runtime::vmm::InstanceSpec;
#[cfg(not(target_os = "windows"))]
use a3s_box_runtime::ATTEST_VSOCK_PORT;
#[cfg(not(target_os = "windows"))]
use a3s_box_runtime::EXEC_VSOCK_PORT;
#[cfg(not(target_os = "windows"))]
use a3s_box_runtime::PTY_VSOCK_PORT;
use clap::Parser;
use tracing_subscriber::EnvFilter;

/// A3S Box Shim - MicroVM subprocess
#[derive(Parser, Debug)]
#[command(name = "a3s-box-shim")]
#[command(about = "MicroVM shim process for A3S Box")]
struct Args {
    /// JSON-encoded InstanceSpec configuration
    #[arg(long)]
    config: String,
}

fn main() {
    // Initialize logging
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .with_target(false)
        .init();

    if let Err(e) = run() {
        tracing::error!(error = %e, "Shim failed");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let args = Args::parse();

    // Parse configuration
    let spec: InstanceSpec =
        serde_json::from_str(&args.config).map_err(|e| BoxError::BoxBootError {
            message: format!("Failed to parse config: {}", e),
            hint: None,
        })?;

    tracing::info!(
        box_id = %spec.box_id,
        vcpus = spec.vcpus,
        memory_mib = spec.memory_mib,
        rootfs = %spec.rootfs_path.display(),
        "Starting VM"
    );

    // Validate rootfs exists
    if !spec.rootfs_path.exists() {
        return Err(BoxError::BoxBootError {
            message: format!("Rootfs not found: {}", spec.rootfs_path.display()),
            hint: Some("Ensure the guest rootfs is properly set up".to_string()),
        });
    }

    // Validate filesystem mounts exist
    for mount in &spec.fs_mounts {
        if !mount.host_path.exists() {
            return Err(BoxError::BoxBootError {
                message: format!(
                    "Filesystem mount '{}' not found: {}",
                    mount.tag,
                    mount.host_path.display()
                ),
                hint: None,
            });
        }
        tracing::debug!(
            tag = %mount.tag,
            path = %mount.host_path.display(),
            read_only = mount.read_only,
            "Validated filesystem mount"
        );
    }

    // Configure and start VM
    unsafe {
        configure_and_start_vm(&spec)?;
    }

    Ok(())
}

/// Parse a Docker-style ulimit string into a krun rlimit string.
///
/// Input format: "RESOURCE=SOFT:HARD" (e.g., "nofile=1024:4096")
/// Output format: "RESOURCE_NUM=SOFT:HARD" (e.g., "7=1024:4096")
///
/// Returns None if the resource name is unrecognized.
fn parse_ulimit(ulimit: &str) -> Option<String> {
    let (name, limits) = ulimit.split_once('=')?;
    let resource_num = match name.to_lowercase().as_str() {
        "core" => 4,        // RLIMIT_CORE
        "cpu" => 0,         // RLIMIT_CPU
        "data" => 2,        // RLIMIT_DATA
        "fsize" => 1,       // RLIMIT_FSIZE
        "locks" => 10,      // RLIMIT_LOCKS
        "memlock" => 8,     // RLIMIT_MEMLOCK
        "msgqueue" => 12,   // RLIMIT_MSGQUEUE
        "nice" => 13,       // RLIMIT_NICE
        "nofile" => 7,      // RLIMIT_NOFILE
        "nproc" => 6,       // RLIMIT_NPROC
        "rss" => 5,         // RLIMIT_RSS
        "rtprio" => 14,     // RLIMIT_RTPRIO
        "rttime" => 15,     // RLIMIT_RTTIME
        "sigpending" => 11, // RLIMIT_SIGPENDING
        "stack" => 3,       // RLIMIT_STACK
        _ => return None,
    };
    Some(format!("{}={}", resource_num, limits))
}

/// Apply CPU pinning via sched_setaffinity (Linux only).
#[cfg(target_os = "linux")]
fn apply_cpuset(cpuset: &str) -> std::result::Result<(), String> {
    use std::mem;

    // Parse comma-separated CPU IDs (e.g., "0,1,3" or "0-3")
    let cpus = parse_cpuset_spec(cpuset)?;
    if cpus.is_empty() {
        return Err("empty cpuset specification".to_string());
    }

    unsafe {
        let mut set: libc::cpu_set_t = mem::zeroed();
        libc::CPU_ZERO(&mut set);
        for cpu in &cpus {
            libc::CPU_SET(*cpu, &mut set);
        }

        let ret = libc::sched_setaffinity(0, mem::size_of::<libc::cpu_set_t>(), &set);
        if ret != 0 {
            return Err(format!(
                "sched_setaffinity failed: {}",
                std::io::Error::last_os_error()
            ));
        }
    }

    tracing::info!(cpus = ?cpus, "Applied CPU pinning");
    Ok(())
}

/// Parse a cpuset specification like "0,1,3" or "0-3" or "0,2-4,7".
#[cfg(target_os = "linux")]
fn parse_cpuset_spec(spec: &str) -> std::result::Result<Vec<usize>, String> {
    let mut cpus = Vec::new();
    for part in spec.split(',') {
        let part = part.trim();
        if part.contains('-') {
            let range: Vec<&str> = part.split('-').collect();
            if range.len() != 2 {
                return Err(format!("invalid CPU range: {}", part));
            }
            let start: usize = range[0]
                .parse()
                .map_err(|_| format!("invalid CPU number: {}", range[0]))?;
            let end: usize = range[1]
                .parse()
                .map_err(|_| format!("invalid CPU number: {}", range[1]))?;
            if start > end {
                return Err(format!("invalid CPU range: {}-{}", start, end));
            }
            for cpu in start..=end {
                cpus.push(cpu);
            }
        } else {
            let cpu: usize = part
                .parse()
                .map_err(|_| format!("invalid CPU number: {}", part))?;
            cpus.push(cpu);
        }
    }
    Ok(cpus)
}

/// Apply cgroup v2 resource limits (Linux only, best-effort).
///
/// Creates a cgroup under /sys/fs/cgroup/a3s-box/<box_id>/ and writes
/// the appropriate control files. Moves the current process into the cgroup.
#[cfg(target_os = "linux")]
fn apply_cgroup_limits(spec: &InstanceSpec) {
    let limits = &spec.resource_limits;
    let has_cgroup_limits = limits.cpu_shares.is_some()
        || limits.cpu_quota.is_some()
        || limits.memory_reservation.is_some()
        || limits.memory_swap.is_some();

    if !has_cgroup_limits {
        return;
    }

    let cgroup_path = format!("/sys/fs/cgroup/a3s-box/{}", spec.box_id);

    // Create cgroup directory
    if std::fs::create_dir_all(&cgroup_path).is_err() {
        tracing::debug!(
            path = cgroup_path,
            "Cannot create cgroup directory (requires root or cgroup delegation)"
        );
        return;
    }

    // cpu.weight (from --cpu-shares)
    // Docker shares range: 2-262144, cgroup v2 weight range: 1-10000
    // Conversion: weight = 1 + ((shares - 2) * 9999) / 262142
    if let Some(shares) = limits.cpu_shares {
        let weight = 1 + ((shares.clamp(2, 262144) - 2) * 9999) / 262142;
        if let Err(e) = std::fs::write(format!("{}/cpu.weight", cgroup_path), weight.to_string()) {
            tracing::debug!(error = %e, "Failed to set cpu.weight");
        } else {
            tracing::info!(shares, weight, "Applied CPU shares");
        }
    }

    // cpu.max (from --cpu-quota / --cpu-period)
    if let Some(quota) = limits.cpu_quota {
        let period = limits.cpu_period.unwrap_or(100_000);
        let quota_str = if quota < 0 {
            "max".to_string()
        } else {
            quota.to_string()
        };
        let value = format!("{} {}", quota_str, period);
        if let Err(e) = std::fs::write(format!("{}/cpu.max", cgroup_path), &value) {
            tracing::debug!(error = %e, "Failed to set cpu.max");
        } else {
            tracing::info!(cpu_max = value, "Applied CPU quota");
        }
    }

    // memory.low (from --memory-reservation)
    if let Some(reservation) = limits.memory_reservation {
        if let Err(e) = std::fs::write(
            format!("{}/memory.low", cgroup_path),
            reservation.to_string(),
        ) {
            tracing::debug!(error = %e, "Failed to set memory.low");
        } else {
            tracing::info!(bytes = reservation, "Applied memory reservation");
        }
    }

    // memory.swap.max (from --memory-swap)
    if let Some(swap) = limits.memory_swap {
        let value = if swap < 0 {
            "max".to_string()
        } else {
            swap.to_string()
        };
        if let Err(e) = std::fs::write(format!("{}/memory.swap.max", cgroup_path), &value) {
            tracing::debug!(error = %e, "Failed to set memory.swap.max");
        } else {
            tracing::info!(memory_swap = value, "Applied memory swap limit");
        }
    }

    // Move current process into the cgroup
    let pid = std::process::id();
    if let Err(e) = std::fs::write(format!("{}/cgroup.procs", cgroup_path), pid.to_string()) {
        tracing::debug!(error = %e, "Failed to move process into cgroup");
    } else {
        tracing::info!(cgroup = cgroup_path, "Moved shim process into cgroup");
    }
}

/// Configure libkrun context and start the VM.
///
/// # Safety
/// This function calls unsafe libkrun FFI functions.
/// It performs process takeover on success - the function never returns.
unsafe fn configure_and_start_vm(spec: &InstanceSpec) -> Result<()> {
    // Initialize libkrun logging
    tracing::debug!("Initializing libkrun logging");
    if let Err(e) = KrunContext::init_logging() {
        tracing::warn!(error = %e, "Failed to initialize libkrun logging");
    }

    // Create libkrun context
    tracing::debug!("Creating libkrun context");
    let ctx = KrunContext::create()?;

    // Configure VM resources
    tracing::debug!(
        vcpus = spec.vcpus,
        memory_mib = spec.memory_mib,
        "Setting VM config"
    );
    ctx.set_vm_config(spec.vcpus, spec.memory_mib)?;

    // Raise RLIMIT_NOFILE to maximum - CRITICAL for virtio-fs
    #[cfg(unix)]
    {
        use libc::{getrlimit, rlimit, setrlimit, RLIMIT_NOFILE};
        let mut rlim = rlimit {
            rlim_cur: 0,
            rlim_max: 0,
        };
        if getrlimit(RLIMIT_NOFILE, &mut rlim) == 0 {
            rlim.rlim_cur = rlim.rlim_max;
            if setrlimit(RLIMIT_NOFILE, &rlim) != 0 {
                tracing::warn!("Failed to raise RLIMIT_NOFILE");
            } else {
                tracing::debug!(limit = rlim.rlim_cur, "RLIMIT_NOFILE raised");
            }
        }
    }

    // Configure guest rlimits
    let mut rlimits = vec![
        "7=1048576:1048576".to_string(), // RLIMIT_NOFILE = 7
    ];

    // Apply pids_limit as RLIMIT_NPROC (resource 6)
    if let Some(pids_limit) = spec.resource_limits.pids_limit {
        rlimits.push(format!("6={}:{}", pids_limit, pids_limit));
        tracing::info!(pids_limit, "Applying PID limit via RLIMIT_NPROC");
    } else {
        rlimits.push("6=4096:8192".to_string()); // Default RLIMIT_NPROC
    }

    // Apply custom ulimits (--ulimit RESOURCE=SOFT:HARD)
    for ulimit in &spec.resource_limits.ulimits {
        if let Some(rlimit_str) = parse_ulimit(ulimit) {
            rlimits.push(rlimit_str);
            tracing::info!(ulimit, "Applying custom ulimit");
        } else {
            tracing::warn!(ulimit, "Ignoring unrecognized ulimit format");
        }
    }

    tracing::debug!(rlimits = ?rlimits, "Configuring guest rlimits");
    ctx.set_rlimits(&rlimits)?;

    // Add filesystem mounts via virtiofs
    tracing::info!("Adding filesystem mounts via virtiofs:");
    for mount in &spec.fs_mounts {
        let path_str = mount
            .host_path
            .to_str()
            .ok_or_else(|| BoxError::BoxBootError {
                message: format!("Invalid path: {}", mount.host_path.display()),
                hint: None,
            })?;

        tracing::info!(
            "  {} → {} ({})",
            mount.tag,
            mount.host_path.display(),
            if mount.read_only { "ro" } else { "rw" }
        );
        ctx.add_virtiofs(&mount.tag, path_str)?;
    }

    // Set root filesystem
    let rootfs_str = spec
        .rootfs_path
        .to_str()
        .ok_or_else(|| BoxError::BoxBootError {
            message: format!("Invalid rootfs path: {}", spec.rootfs_path.display()),
            hint: None,
        })?;
    tracing::debug!(rootfs = rootfs_str, "Setting root filesystem");
    ctx.set_root(rootfs_str)?;

    // Set working directory
    tracing::debug!(workdir = %spec.workdir, "Setting working directory");
    ctx.set_workdir(&spec.workdir)?;

    // Set entrypoint
    tracing::debug!(
        executable = %spec.entrypoint.executable,
        args = ?spec.entrypoint.args,
        "Setting entrypoint"
    );
    ctx.set_exec(
        &spec.entrypoint.executable,
        &spec.entrypoint.args,
        &spec.entrypoint.env,
    )?;

    // Configure exec communication channel (Unix socket bridged to vsock port 4089)
    #[cfg(not(target_os = "windows"))]
    {
    let exec_socket_str = spec
        .exec_socket_path
        .to_str()
        .ok_or_else(|| BoxError::BoxBootError {
            message: format!(
                "Invalid exec socket path: {}",
                spec.exec_socket_path.display()
            ),
            hint: None,
        })?;
    tracing::debug!(
        socket_path = exec_socket_str,
        guest_port = EXEC_VSOCK_PORT,
        "Configuring vsock bridge for exec"
    );
    ctx.add_vsock_port(EXEC_VSOCK_PORT, exec_socket_str, true)?;

    // Configure PTY communication channel (Unix socket bridged to vsock port 4090)
    if !spec.pty_socket_path.as_os_str().is_empty() {
        let pty_socket_str =
            spec.pty_socket_path
                .to_str()
                .ok_or_else(|| BoxError::BoxBootError {
                    message: format!(
                        "Invalid PTY socket path: {}",
                        spec.pty_socket_path.display()
                    ),
                    hint: None,
                })?;
        tracing::debug!(
            socket_path = pty_socket_str,
            guest_port = PTY_VSOCK_PORT,
            "Configuring vsock bridge for PTY"
        );
        ctx.add_vsock_port(PTY_VSOCK_PORT, pty_socket_str, true)?;
    }

    // Configure attestation communication channel (Unix socket bridged to vsock port 4091)
    if !spec.attest_socket_path.as_os_str().is_empty() {
        let attest_socket_str =
            spec.attest_socket_path
                .to_str()
                .ok_or_else(|| BoxError::BoxBootError {
                    message: format!(
                        "Invalid attestation socket path: {}",
                        spec.attest_socket_path.display()
                    ),
                    hint: None,
                })?;
        tracing::debug!(
            socket_path = attest_socket_str,
            guest_port = ATTEST_VSOCK_PORT,
            "Configuring vsock bridge for attestation"
        );
        ctx.add_vsock_port(ATTEST_VSOCK_PORT, attest_socket_str, true)?;
    }
    } // end #[cfg(not(target_os = "windows"))]

    // Note: A3S_TEE_SIMULATE is already included in spec.entrypoint.env
    // (added by vm.rs when simulate mode is on) and passed to the guest init
    // via krun_set_exec's envp parameter. Do NOT call set_env here — libkrun's
    // krun_set_env overwrites (not appends) the environment, which would erase
    // all BOX_EXEC_* vars set by set_exec.
    if spec
        .entrypoint
        .env
        .iter()
        .any(|(k, _)| k == "A3S_TEE_SIMULATE")
    {
        tracing::info!("TEE simulation mode: A3S_TEE_SIMULATE=1 included in entrypoint env");
    }

    // Configure TSI port mappings if specified
    if !spec.port_map.is_empty() {
        tracing::info!(port_map = ?spec.port_map, "Configuring TSI port mappings");
        ctx.set_port_map(&spec.port_map)?;
    }

    // Configure networking: passt (virtio-net) or TSI (default)
    #[cfg(not(target_os = "windows"))]
    if let Some(ref net_config) = spec.network {
        tracing::info!(
            ip = %net_config.ip_address,
            gateway = %net_config.gateway,
            mac = ?net_config.mac_address,
            socket = %net_config.passt_socket_path.display(),
            "Configuring passt virtio-net networking"
        );

        let socket_str =
            net_config
                .passt_socket_path
                .to_str()
                .ok_or_else(|| BoxError::BoxBootError {
                    message: format!(
                        "Invalid passt socket path: {}",
                        net_config.passt_socket_path.display()
                    ),
                    hint: None,
                })?;

        ctx.add_net_unixstream(socket_str, &net_config.mac_address)?;

        // Network env vars (A3S_NET_IP, A3S_NET_GATEWAY, A3S_NET_DNS) are now
        // injected into spec.entrypoint.env by vm.rs, so they are passed via
        // krun_set_exec's envp alongside all BOX_EXEC_* vars. Do NOT call
        // ctx.set_env here — libkrun's krun_set_env overwrites (not appends)
        // the environment, which would erase all vars set by set_exec.
    }

    // Configure user/group from OCI USER directive
    if let Some(ref user) = spec.user {
        apply_user_config(&ctx, user)?;
    }

    // Configure console output if specified
    if let Some(console_path) = &spec.console_output {
        let console_str = console_path
            .to_str()
            .ok_or_else(|| BoxError::BoxBootError {
                message: format!("Invalid console output path: {}", console_path.display()),
                hint: None,
            })?;
        tracing::debug!(console_path = console_str, "Redirecting console output");
        ctx.set_console_output(console_str)?;
    }

    // Configure TEE if specified (only available on Linux with SEV support)
    #[cfg(target_os = "linux")]
    if let Some(ref tee_config) = spec.tee_config {
        tracing::info!(
            tee_type = %tee_config.tee_type,
            config_path = %tee_config.config_path.display(),
            "Configuring TEE"
        );

        // Enable split IRQ chip (required for TEE)
        ctx.enable_split_irqchip()?;

        // Set TEE configuration file
        let tee_config_str = tee_config.config_path.to_str().ok_or_else(|| {
            BoxError::TeeConfig(format!(
                "Invalid TEE config path: {}",
                tee_config.config_path.display()
            ))
        })?;
        ctx.set_tee_config(tee_config_str)?;

        tracing::info!("TEE configured successfully");
    }

    #[cfg(not(target_os = "linux"))]
    if spec.tee_config.is_some() {
        tracing::warn!("TEE configuration is only supported on Linux; ignoring");
    }

    // Apply CPU pinning via sched_setaffinity (Linux only)
    #[cfg(target_os = "linux")]
    if let Some(ref cpuset) = spec.resource_limits.cpuset_cpus {
        if let Err(e) = apply_cpuset(cpuset) {
            tracing::warn!(cpuset = cpuset, error = %e, "Failed to apply CPU pinning");
        }
    }

    // Apply cgroup v2 resource limits (Linux only, best-effort)
    #[cfg(target_os = "linux")]
    apply_cgroup_limits(spec);

    // Start VM (process takeover - never returns on success)
    tracing::info!(box_id = %spec.box_id, "Starting VM (process takeover)");
    let status = ctx.start_enter();

    // If we reach here, either:
    // 1. VM failed to start (negative status)
    // 2. VM started and guest exited (non-negative status)
    if status < 0 {
        if status == -22 {
            return Err(BoxError::BoxBootError {
                message: "libkrun returned EINVAL - invalid configuration".to_string(),
                hint: Some("Check VM configuration (rootfs, entrypoint, etc.)".to_string()),
            });
        }
        Err(BoxError::BoxBootError {
            message: format!("VM failed to start with status {}", status),
            hint: None,
        })
    } else {
        // VM started and guest exited — propagate the guest exit code to the host.
        tracing::info!(exit_status = status, "VM exited");
        std::process::exit(status);
    }
}

/// Apply OCI USER directive to the krun context.
///
/// Supports formats:
/// - "uid" (e.g., "1000")
/// - "uid:gid" (e.g., "1000:1000")
/// - Non-numeric names are logged and skipped (would require /etc/passwd lookup)
unsafe fn apply_user_config(ctx: &KrunContext, user: &str) -> Result<()> {
    if user.is_empty() {
        return Ok(());
    }

    let parts: Vec<&str> = user.split(':').collect();
    let uid_str = parts[0];
    let gid_str = parts.get(1).copied();

    // Parse UID
    match uid_str.parse::<u32>() {
        Ok(uid) => {
            tracing::info!(uid, "Setting VM user from OCI USER directive");
            ctx.set_uid(uid)?;
        }
        Err(_) => {
            // Non-numeric user name — would need /etc/passwd lookup inside rootfs
            tracing::warn!(
                user = uid_str,
                "Non-numeric USER directive; skipping (name lookup not yet supported)"
            );
            return Ok(());
        }
    }

    // Parse GID if present
    if let Some(gid_str) = gid_str {
        match gid_str.parse::<u32>() {
            Ok(gid) => {
                tracing::info!(gid, "Setting VM group from OCI USER directive");
                ctx.set_gid(gid)?;
            }
            Err(_) => {
                tracing::warn!(
                    group = gid_str,
                    "Non-numeric group in USER directive; skipping"
                );
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_ulimit_nofile() {
        assert_eq!(
            parse_ulimit("nofile=1024:4096"),
            Some("7=1024:4096".to_string())
        );
    }

    #[test]
    fn test_parse_ulimit_nproc() {
        assert_eq!(parse_ulimit("nproc=256:512"), Some("6=256:512".to_string()));
    }

    #[test]
    fn test_parse_ulimit_stack() {
        assert_eq!(
            parse_ulimit("stack=8192:8192"),
            Some("3=8192:8192".to_string())
        );
    }

    #[test]
    fn test_parse_ulimit_core() {
        assert_eq!(parse_ulimit("core=0:0"), Some("4=0:0".to_string()));
    }

    #[test]
    fn test_parse_ulimit_case_insensitive() {
        assert_eq!(
            parse_ulimit("NOFILE=1024:4096"),
            Some("7=1024:4096".to_string())
        );
        assert_eq!(parse_ulimit("Nproc=100:200"), Some("6=100:200".to_string()));
    }

    #[test]
    fn test_parse_ulimit_unknown() {
        assert_eq!(parse_ulimit("unknown=1:2"), None);
    }

    #[test]
    fn test_parse_ulimit_no_equals() {
        assert_eq!(parse_ulimit("nofile"), None);
    }

    #[test]
    fn test_parse_ulimit_all_resources() {
        assert!(parse_ulimit("cpu=10:20").is_some());
        assert!(parse_ulimit("fsize=100:200").is_some());
        assert!(parse_ulimit("data=100:200").is_some());
        assert!(parse_ulimit("locks=100:200").is_some());
        assert!(parse_ulimit("memlock=100:200").is_some());
        assert!(parse_ulimit("msgqueue=100:200").is_some());
        assert!(parse_ulimit("nice=10:20").is_some());
        assert!(parse_ulimit("rss=100:200").is_some());
        assert!(parse_ulimit("rtprio=10:20").is_some());
        assert!(parse_ulimit("rttime=100:200").is_some());
        assert!(parse_ulimit("sigpending=100:200").is_some());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn test_parse_cpuset_spec_single() {
        assert_eq!(parse_cpuset_spec("0").unwrap(), vec![0]);
        assert_eq!(parse_cpuset_spec("3").unwrap(), vec![3]);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn test_parse_cpuset_spec_list() {
        assert_eq!(parse_cpuset_spec("0,1,3").unwrap(), vec![0, 1, 3]);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn test_parse_cpuset_spec_range() {
        assert_eq!(parse_cpuset_spec("0-3").unwrap(), vec![0, 1, 2, 3]);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn test_parse_cpuset_spec_mixed() {
        assert_eq!(parse_cpuset_spec("0,2-4,7").unwrap(), vec![0, 2, 3, 4, 7]);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn test_parse_cpuset_spec_invalid_range() {
        assert!(parse_cpuset_spec("3-1").is_err());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn test_parse_cpuset_spec_invalid_number() {
        assert!(parse_cpuset_spec("abc").is_err());
    }
}
