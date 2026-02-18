//! Linux namespace isolation for agent and business code.
//!
//! Provides utilities to spawn processes in isolated namespaces
//! with seccomp filtering, capability dropping, and no-new-privileges.

#[cfg(target_os = "linux")]
use nix::sched::{unshare, CloneFlags};

use nix::unistd::{fork, ForkResult};
use std::os::unix::process::CommandExt;
use std::process::Command;
use thiserror::Error;

/// Namespace isolation errors.
#[derive(Debug, Error)]
pub enum NamespaceError {
    #[error("Fork failed: {0}")]
    ForkFailed(#[from] nix::Error),

    #[error("Unshare failed: {0}")]
    UnshareFailed(nix::Error),

    #[error("Exec failed: {0}")]
    ExecFailed(std::io::Error),

    #[error("Invalid command: {0}")]
    InvalidCommand(String),

    #[error("Security setup failed: {0}")]
    SecurityFailed(String),
}

/// Namespace configuration for process isolation.
#[derive(Debug, Clone)]
pub struct NamespaceConfig {
    /// Separate filesystem view (mount namespace)
    pub mount: bool,

    /// Separate process tree (PID namespace)
    pub pid: bool,

    /// Separate IPC (IPC namespace)
    pub ipc: bool,

    /// Separate hostname (UTS namespace)
    pub uts: bool,

    /// Separate network (network namespace)
    /// Usually false to allow agent-business communication
    pub net: bool,

    /// Separate user/group IDs (user namespace)
    /// Enables rootless containers with UID/GID remapping
    pub user: bool,

    /// Separate cgroup view (cgroup namespace)
    pub cgroup: bool,
}

impl Default for NamespaceConfig {
    fn default() -> Self {
        Self {
            mount: true,
            pid: true,
            ipc: true,
            uts: true,
            net: false, // Share network for communication
            user: false, // Disabled by default (requires UID mapping setup)
            cgroup: false, // Disabled by default
        }
    }
}

impl NamespaceConfig {
    /// Create a namespace config with all isolation enabled.
    pub fn full_isolation() -> Self {
        Self {
            mount: true,
            pid: true,
            ipc: true,
            uts: true,
            net: true,
            user: true,
            cgroup: true,
        }
    }

    /// Create a namespace config with minimal isolation (mount + PID only).
    pub fn minimal() -> Self {
        Self {
            mount: true,
            pid: true,
            ipc: false,
            uts: false,
            net: false,
            user: false,
            cgroup: false,
        }
    }

    /// Convert to CloneFlags for unshare().
    #[cfg(target_os = "linux")]
    fn to_clone_flags(&self) -> CloneFlags {
        let mut flags = CloneFlags::empty();

        if self.mount {
            flags |= CloneFlags::CLONE_NEWNS;
        }
        if self.pid {
            flags |= CloneFlags::CLONE_NEWPID;
        }
        if self.ipc {
            flags |= CloneFlags::CLONE_NEWIPC;
        }
        if self.uts {
            flags |= CloneFlags::CLONE_NEWUTS;
        }
        if self.net {
            flags |= CloneFlags::CLONE_NEWNET;
        }
        if self.user {
            flags |= CloneFlags::CLONE_NEWUSER;
        }
        if self.cgroup {
            flags |= CloneFlags::CLONE_NEWCGROUP;
        }

        flags
    }

    /// Stub for non-Linux platforms (development only).
    #[cfg(not(target_os = "linux"))]
    #[allow(dead_code)]
    fn to_clone_flags(&self) -> u32 {
        0 // Placeholder for non-Linux
    }
}

/// Spawn a process in isolated namespaces.
///
/// # Arguments
///
/// * `config` - Namespace isolation configuration
/// * `command` - Path to executable
/// * `args` - Command arguments
/// * `env` - Environment variables (key-value pairs)
/// * `workdir` - Working directory
///
/// # Returns
///
/// PID of the spawned process in the parent namespace.
///
/// # Errors
///
/// Returns error if fork, unshare, or exec fails.
pub fn spawn_isolated(
    config: &NamespaceConfig,
    command: &str,
    args: &[&str],
    env: &[(&str, &str)],
    workdir: &str,
) -> Result<u32, NamespaceError> {
    tracing::info!(
        command = %command,
        args = ?args,
        workdir = %workdir,
        "Spawning process in isolated namespace"
    );

    // Fork to create child process
    match unsafe { fork() }.map_err(NamespaceError::ForkFailed)? {
        ForkResult::Child => {
            // Child process: create namespaces and exec
            if let Err(e) = child_process(config, command, args, env, workdir) {
                tracing::error!("Child process failed: {}", e);
                std::process::exit(1);
            }
            unreachable!("exec should not return");
        }
        ForkResult::Parent { child } => {
            // Parent process: return child PID
            let pid = child.as_raw() as u32;
            tracing::info!(pid = pid, "Child process spawned");
            Ok(pid)
        }
    }
}

/// Child process logic: create namespaces and exec command.
#[cfg(target_os = "linux")]
fn child_process(
    config: &NamespaceConfig,
    command: &str,
    args: &[&str],
    env: &[(&str, &str)],
    workdir: &str,
) -> Result<(), NamespaceError> {
    // Create new namespaces
    let flags = config.to_clone_flags();
    unshare(flags).map_err(NamespaceError::UnshareFailed)?;

    tracing::debug!("Namespaces created: {:?}", config);

    // If PID namespace was created, we need to fork again
    // so the child becomes PID 1 in the new namespace
    if config.pid {
        match unsafe { fork() }.map_err(NamespaceError::ForkFailed)? {
            ForkResult::Child => {
                // This is PID 1 in the new namespace
                tracing::debug!("Now PID 1 in new namespace");
            }
            ForkResult::Parent { child } => {
                // Wait for the child (PID 1 in new namespace)
                use nix::sys::wait::{waitpid, WaitStatus};

                match waitpid(child, None) {
                    Ok(WaitStatus::Exited(_, status)) => {
                        std::process::exit(status);
                    }
                    Ok(WaitStatus::Signaled(_, signal, _)) => {
                        tracing::error!("Child killed by signal {:?}", signal);
                        std::process::exit(128 + signal as i32);
                    }
                    Ok(_) => {
                        std::process::exit(1);
                    }
                    Err(e) => {
                        tracing::error!("waitpid failed: {}", e);
                        std::process::exit(1);
                    }
                }
            }
        }
    }

    // Execute the command
    let mut cmd = Command::new(command);
    cmd.args(args).current_dir(workdir);

    // Set environment variables
    for (key, value) in env {
        cmd.env(key, value);
    }

    // Apply security restrictions before exec
    apply_security_before_exec(&mut cmd)?;

    tracing::debug!("Executing command: {} {:?}", command, args);

    // Replace current process with the command
    let err = cmd.exec();

    // If exec returns, it failed
    Err(NamespaceError::ExecFailed(err))
}

/// Apply security restrictions (seccomp, no-new-privileges, capabilities)
/// before exec using the pre_exec hook.
///
/// Reads security configuration from `A3S_SEC_*` environment variables
/// set by the host runtime.
#[cfg(target_os = "linux")]
fn apply_security_before_exec(cmd: &mut Command) -> Result<(), NamespaceError> {
    use a3s_box_core::security::{SeccompMode, SecurityConfig};

    let config = SecurityConfig::from_env_vars();

    // Privileged mode: skip all security restrictions
    if config.privileged {
        tracing::info!("Privileged mode: skipping security restrictions");
        return Ok(());
    }

    tracing::debug!(
        seccomp = ?config.seccomp,
        no_new_privs = config.no_new_privileges,
        cap_add = ?config.cap_add,
        cap_drop = ?config.cap_drop,
        "Applying security configuration"
    );

    let no_new_privs = config.no_new_privileges;
    let seccomp_mode = config.seccomp.clone();
    let cap_drop = config.cap_drop.clone();

    // Use pre_exec to apply security in the child process right before exec
    // SAFETY: pre_exec runs after fork, before exec. We only call
    // async-signal-safe operations (prctl, seccomp).
    unsafe {
        cmd.pre_exec(move || {
            // 1. Set no-new-privileges
            if no_new_privs {
                let ret = libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0);
                if ret != 0 {
                    return Err(std::io::Error::last_os_error());
                }
            }

            // 2. Drop capabilities
            if should_drop_caps(&cap_drop) {
                drop_capabilities(&cap_drop)?;
            }

            // 3. Apply seccomp filter
            match &seccomp_mode {
                SeccompMode::Default => {
                    apply_default_seccomp()?;
                }
                SeccompMode::Unconfined => {
                    // No seccomp filter
                }
                SeccompMode::Custom(path) => {
                    // Custom seccomp profiles are not yet supported.
                    // Fail loudly rather than silently falling through to
                    // no filter, which would give a false sense of security.
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::Unsupported,
                        format!(
                            "custom seccomp profile '{}' is not supported; \
                             use seccomp=default or seccomp=unconfined",
                            path
                        ),
                    ));
                }
            }

            Ok(())
        });
    }

    Ok(())
}

/// Check if we should drop capabilities.
#[cfg(target_os = "linux")]
fn should_drop_caps(cap_drop: &[String]) -> bool {
    !cap_drop.is_empty()
}

/// Drop Linux capabilities using prctl.
///
/// Drops capabilities from the bounding set AND clears the effective,
/// permitted, and inheritable sets to prevent retention of already-held caps.
/// Supports "ALL" to drop all capabilities, or individual capability names.
#[cfg(target_os = "linux")]
fn drop_capabilities(cap_drop: &[String]) -> Result<(), std::io::Error> {
    // Map capability names to their Linux constants
    let drop_all = cap_drop.iter().any(|c| c == "ALL");

    if drop_all {
        // Drop all capabilities by clearing the bounding set
        // Iterate through all known capabilities (0..CAP_LAST_CAP)
        for cap in 0..=40_i32 {
            // PR_CAPBSET_DROP = 24
            let ret = unsafe { libc::prctl(24, cap, 0, 0, 0) };
            if ret != 0 {
                let err = std::io::Error::last_os_error();
                // EINVAL means capability doesn't exist, which is fine
                if err.raw_os_error() != Some(libc::EINVAL) {
                    return Err(err);
                }
            }
        }
    } else {
        for cap_name in cap_drop {
            if let Some(cap_num) = cap_name_to_number(cap_name) {
                let ret = unsafe { libc::prctl(24, cap_num, 0, 0, 0) };
                if ret != 0 {
                    let err = std::io::Error::last_os_error();
                    if err.raw_os_error() != Some(libc::EINVAL) {
                        return Err(err);
                    }
                }
            }
        }
    }

    // Also clear the effective, permitted, and inheritable capability sets.
    // The bounding set only limits future execve() — processes that already
    // hold capabilities in their effective set retain them without this step.
    clear_ambient_and_inheritable_caps()?;

    Ok(())
}

/// Clear ambient and inheritable capability sets.
///
/// This ensures that dropped capabilities cannot be re-acquired through
/// execve() or ambient capability inheritance.
#[cfg(target_os = "linux")]
fn clear_ambient_and_inheritable_caps() -> Result<(), std::io::Error> {
    // PR_CAP_AMBIENT_CLEAR_ALL = 4 (subcommand of PR_CAP_AMBIENT = 47)
    let ret = unsafe { libc::prctl(47, 4, 0, 0, 0) };
    if ret != 0 {
        let err = std::io::Error::last_os_error();
        // EINVAL means kernel doesn't support ambient caps — that's fine
        if err.raw_os_error() != Some(libc::EINVAL) {
            return Err(err);
        }
    }
    Ok(())
}

/// Map a Linux capability name to its numeric value.
#[cfg(target_os = "linux")]
fn cap_name_to_number(name: &str) -> Option<i32> {
    // Standard Linux capability constants
    match name {
        "CHOWN" => Some(0),
        "DAC_OVERRIDE" => Some(1),
        "DAC_READ_SEARCH" => Some(2),
        "FOWNER" => Some(3),
        "FSETID" => Some(4),
        "KILL" => Some(5),
        "SETGID" => Some(6),
        "SETUID" => Some(7),
        "SETPCAP" => Some(8),
        "LINUX_IMMUTABLE" => Some(9),
        "NET_BIND_SERVICE" => Some(10),
        "NET_BROADCAST" => Some(11),
        "NET_ADMIN" => Some(12),
        "NET_RAW" => Some(13),
        "IPC_LOCK" => Some(14),
        "IPC_OWNER" => Some(15),
        "SYS_MODULE" => Some(16),
        "SYS_RAWIO" => Some(17),
        "SYS_CHROOT" => Some(18),
        "SYS_PTRACE" => Some(19),
        "SYS_PACCT" => Some(20),
        "SYS_ADMIN" => Some(21),
        "SYS_BOOT" => Some(22),
        "SYS_NICE" => Some(23),
        "SYS_RESOURCE" => Some(24),
        "SYS_TIME" => Some(25),
        "SYS_TTY_CONFIG" => Some(26),
        "MKNOD" => Some(27),
        "LEASE" => Some(28),
        "AUDIT_WRITE" => Some(29),
        "AUDIT_CONTROL" => Some(30),
        "SETFCAP" => Some(31),
        "MAC_OVERRIDE" => Some(32),
        "MAC_ADMIN" => Some(33),
        "SYSLOG" => Some(34),
        "WAKE_ALARM" => Some(35),
        "BLOCK_SUSPEND" => Some(36),
        "AUDIT_READ" => Some(37),
        "PERFMON" => Some(38),
        "BPF" => Some(39),
        "CHECKPOINT_RESTORE" => Some(40),
        _ => None,
    }
}

/// Apply the default seccomp filter that blocks dangerous syscalls.
///
/// Based on Docker's default seccomp profile — blocks syscalls that could
/// escape the sandbox or compromise the host.
#[cfg(target_os = "linux")]
fn apply_default_seccomp() -> Result<(), std::io::Error> {
    // Use SECCOMP_SET_MODE_FILTER via prctl
    // The default profile uses a BPF filter that blocks:
    // - kexec_load, kexec_file_load (kernel replacement)
    // - reboot (system reboot)
    // - mount, umount2 (filesystem manipulation — unless in mount namespace)
    // - pivot_root, chroot (filesystem escape)
    // - swapon, swapoff (swap manipulation)
    // - init_module, finit_module, delete_module (kernel modules)
    // - acct (process accounting)
    // - settimeofday, clock_settime (time manipulation)
    // - personality (execution domain change)
    // - keyctl (kernel keyring)
    // - ptrace (process tracing — unless CAP_SYS_PTRACE)
    // - userfaultfd (memory manipulation)
    // - perf_event_open (performance monitoring)
    // - bpf (eBPF programs)
    // - unshare (namespace creation — already in namespace)
    // - setns (namespace switching)

    // Build BPF filter program
    let filter = build_default_bpf_filter();

    // Install the filter via prctl + seccomp
    // First, ensure no-new-privs is set (required for unprivileged seccomp)
    let ret = unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) };
    if ret != 0 {
        return Err(std::io::Error::last_os_error());
    }

    // SECCOMP_SET_MODE_FILTER = 1, SECCOMP_FILTER_FLAG_TSYNC = 1
    let prog = libc::sock_fprog {
        len: filter.len() as u16,
        filter: filter.as_ptr() as *mut libc::sock_filter,
    };

    // seccomp(SECCOMP_SET_MODE_FILTER, 0, &prog)
    let ret = unsafe { libc::syscall(libc::SYS_seccomp, 1_i32, 0_i32, &prog) };
    if ret != 0 {
        return Err(std::io::Error::last_os_error());
    }

    Ok(())
}

/// Build the default BPF seccomp filter.
///
/// Returns SECCOMP_RET_ERRNO(EPERM) for blocked syscalls,
/// SECCOMP_RET_ALLOW for everything else.
#[cfg(target_os = "linux")]
fn build_default_bpf_filter() -> Vec<libc::sock_filter> {
    // BPF constants
    const BPF_LD: u16 = 0x00;
    const BPF_W: u16 = 0x00;
    const BPF_ABS: u16 = 0x20;
    const BPF_JMP: u16 = 0x05;
    const BPF_JEQ: u16 = 0x10;
    const BPF_K: u16 = 0x00;
    const BPF_RET: u16 = 0x06;

    // SECCOMP return values
    const SECCOMP_RET_ALLOW: u32 = 0x7fff_0000;
    const SECCOMP_RET_ERRNO_EPERM: u32 = 0x0005_0001; // SECCOMP_RET_ERRNO | EPERM
    const SECCOMP_RET_KILL_PROCESS: u32 = 0x8000_0000;

    // Architecture audit value for seccomp_data.arch
    #[cfg(target_arch = "x86_64")]
    const AUDIT_ARCH: u32 = 0xC000_003E; // AUDIT_ARCH_X86_64
    #[cfg(target_arch = "aarch64")]
    const AUDIT_ARCH: u32 = 0xC000_00B7; // AUDIT_ARCH_AARCH64

    // Blocked syscall numbers (x86_64)
    #[cfg(target_arch = "x86_64")]
    let blocked_syscalls: &[u32] = &[
        246, // kexec_load
        320, // kexec_file_load
        169, // reboot
        167, // swapon
        168, // swapoff
        175, // init_module
        313, // finit_module
        176, // delete_module
        163, // acct
        164, // settimeofday
        227, // clock_settime
        135, // personality
        250, // keyctl
        298, // perf_event_open
        321, // bpf
        323, // userfaultfd
    ];

    // Blocked syscall numbers (aarch64)
    #[cfg(target_arch = "aarch64")]
    let blocked_syscalls: &[u32] = &[
        104, // kexec_load
        294, // kexec_file_load
        142, // reboot
        224, // swapon
        225, // swapoff
        105, // init_module
        273, // finit_module
        106, // delete_module
        89,  // acct
        170, // settimeofday
        112, // clock_settime
        92,  // personality
        219, // keyctl
        241, // perf_event_open
        280, // bpf
        282, // userfaultfd
    ];

    let num_blocked = blocked_syscalls.len();
    // +5: arch_load, arch_check, syscall_load, allow, deny
    let mut filter = Vec::with_capacity(num_blocked + 5);

    // 1. Load architecture: LD [data[4]] (offset 4 = arch in seccomp_data)
    filter.push(libc::sock_filter {
        code: BPF_LD | BPF_W | BPF_ABS,
        jt: 0,
        jf: 0,
        k: 4, // offsetof(seccomp_data, arch)
    });

    // 2. Verify architecture matches — kill process if wrong arch
    //    JEQ AUDIT_ARCH, next(0), kill(num_blocked + 2)
    filter.push(libc::sock_filter {
        code: BPF_JMP | BPF_JEQ | BPF_K,
        jt: 0,                          // continue to syscall check
        jf: (num_blocked + 2) as u8,    // jump to kill (past load + all checks + allow)
        k: AUDIT_ARCH,
    });

    // 3. Load syscall number: LD [data[0]] (offset 0 = syscall nr in seccomp_data)
    filter.push(libc::sock_filter {
        code: BPF_LD | BPF_W | BPF_ABS,
        jt: 0,
        jf: 0,
        k: 0, // offsetof(seccomp_data, nr)
    });

    // 4. For each blocked syscall: JEQ #nr, goto_deny, next
    for (i, &nr) in blocked_syscalls.iter().enumerate() {
        let remaining = num_blocked - i;
        filter.push(libc::sock_filter {
            code: BPF_JMP | BPF_JEQ | BPF_K,
            jt: remaining as u8, // jump to deny (past all remaining checks + allow)
            jf: 0,               // continue to next check
            k: nr,
        });
    }

    // 5. Allow (default action)
    filter.push(libc::sock_filter {
        code: BPF_RET | BPF_K,
        jt: 0,
        jf: 0,
        k: SECCOMP_RET_ALLOW,
    });

    // 6. Deny with EPERM (blocked syscall)
    filter.push(libc::sock_filter {
        code: BPF_RET | BPF_K,
        jt: 0,
        jf: 0,
        k: SECCOMP_RET_ERRNO_EPERM,
    });

    // 7. Kill process (wrong architecture — potential bypass attempt)
    filter.push(libc::sock_filter {
        code: BPF_RET | BPF_K,
        jt: 0,
        jf: 0,
        k: SECCOMP_RET_KILL_PROCESS,
    });

    filter
}

/// Child process logic for non-Linux platforms (development stub).
#[cfg(not(target_os = "linux"))]
fn child_process(
    _config: &NamespaceConfig,
    command: &str,
    args: &[&str],
    env: &[(&str, &str)],
    workdir: &str,
) -> Result<(), NamespaceError> {
    // On non-Linux, just exec without namespace isolation or security
    tracing::warn!("Namespace isolation and security enforcement not available on this platform");

    let mut cmd = Command::new(command);
    cmd.args(args).current_dir(workdir);

    for (key, value) in env {
        cmd.env(key, value);
    }

    let err = cmd.exec();
    Err(NamespaceError::ExecFailed(err))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_namespace_config_default() {
        let config = NamespaceConfig::default();
        assert!(config.mount);
        assert!(config.pid);
        assert!(config.ipc);
        assert!(config.uts);
        assert!(!config.net);
        assert!(!config.user);
        assert!(!config.cgroup);
    }

    #[test]
    fn test_namespace_config_full_isolation() {
        let config = NamespaceConfig::full_isolation();
        assert!(config.mount);
        assert!(config.pid);
        assert!(config.ipc);
        assert!(config.uts);
        assert!(config.net);
        assert!(config.user);
        assert!(config.cgroup);
    }

    #[test]
    fn test_namespace_config_minimal() {
        let config = NamespaceConfig::minimal();
        assert!(config.mount);
        assert!(config.pid);
        assert!(!config.ipc);
        assert!(!config.uts);
        assert!(!config.net);
        assert!(!config.user);
        assert!(!config.cgroup);
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn test_namespace_config_to_clone_flags() {
        let config = NamespaceConfig {
            mount: true,
            pid: true,
            ipc: false,
            uts: false,
            net: false,
            user: false,
            cgroup: false,
        };

        let flags = config.to_clone_flags();
        assert!(flags.contains(CloneFlags::CLONE_NEWNS));
        assert!(flags.contains(CloneFlags::CLONE_NEWPID));
        assert!(!flags.contains(CloneFlags::CLONE_NEWIPC));
        assert!(!flags.contains(CloneFlags::CLONE_NEWUTS));
        assert!(!flags.contains(CloneFlags::CLONE_NEWNET));
        assert!(!flags.contains(CloneFlags::CLONE_NEWUSER));
        assert!(!flags.contains(CloneFlags::CLONE_NEWCGROUP));
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn test_namespace_config_to_clone_flags_full() {
        let config = NamespaceConfig::full_isolation();
        let flags = config.to_clone_flags();
        assert!(flags.contains(CloneFlags::CLONE_NEWNS));
        assert!(flags.contains(CloneFlags::CLONE_NEWPID));
        assert!(flags.contains(CloneFlags::CLONE_NEWIPC));
        assert!(flags.contains(CloneFlags::CLONE_NEWUTS));
        assert!(flags.contains(CloneFlags::CLONE_NEWNET));
        assert!(flags.contains(CloneFlags::CLONE_NEWUSER));
        assert!(flags.contains(CloneFlags::CLONE_NEWCGROUP));
    }

    // --- Capability mapping tests ---

    #[test]
    #[cfg(target_os = "linux")]
    fn test_cap_name_to_number_known() {
        assert_eq!(cap_name_to_number("NET_ADMIN"), Some(12));
        assert_eq!(cap_name_to_number("SYS_PTRACE"), Some(19));
        assert_eq!(cap_name_to_number("SYS_ADMIN"), Some(21));
        assert_eq!(cap_name_to_number("CHOWN"), Some(0));
        assert_eq!(cap_name_to_number("NET_RAW"), Some(13));
        assert_eq!(cap_name_to_number("CHECKPOINT_RESTORE"), Some(40));
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn test_cap_name_to_number_unknown() {
        assert_eq!(cap_name_to_number("NONEXISTENT"), None);
        assert_eq!(cap_name_to_number(""), None);
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn test_should_drop_caps_empty() {
        assert!(!should_drop_caps(&[]));
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn test_should_drop_caps_nonempty() {
        assert!(should_drop_caps(&["ALL".to_string()]));
        assert!(should_drop_caps(&["NET_RAW".to_string()]));
    }

    // --- BPF filter tests ---

    #[test]
    #[cfg(target_os = "linux")]
    fn test_bpf_filter_structure() {
        let filter = build_default_bpf_filter();
        // Should have: 1 arch_load + 1 arch_check + 1 syscall_load + N checks + 1 allow + 1 deny + 1 kill
        assert!(filter.len() >= 6);
        // First instruction should be BPF_LD (load arch)
        assert_eq!(filter[0].code, 0x20); // BPF_LD | BPF_W | BPF_ABS
        assert_eq!(filter[0].k, 4); // offset 4 = arch field
        // Second instruction should be JEQ (arch check)
        assert_eq!(filter[1].code, 0x15); // BPF_JMP | BPF_JEQ | BPF_K
        // Third instruction should be BPF_LD (load syscall nr)
        assert_eq!(filter[2].code, 0x20);
        assert_eq!(filter[2].k, 0); // offset 0 = syscall nr
        // Last instruction should be BPF_RET (kill — wrong arch)
        let last = filter.last().unwrap();
        assert_eq!(last.code, 0x06); // BPF_RET | BPF_K
        assert_eq!(last.k, 0x8000_0000); // SECCOMP_RET_KILL_PROCESS
        // Second to last should be BPF_RET (deny — blocked syscall)
        let second_last = &filter[filter.len() - 2];
        assert_eq!(second_last.code, 0x06);
        assert_eq!(second_last.k, 0x0005_0001); // SECCOMP_RET_ERRNO_EPERM
        // Third to last should be BPF_RET (allow)
        let third_last = &filter[filter.len() - 3];
        assert_eq!(third_last.code, 0x06);
        assert_eq!(third_last.k, 0x7fff_0000); // SECCOMP_RET_ALLOW
    }

    // --- Namespace error tests ---

    #[test]
    fn test_namespace_error_display() {
        let err = NamespaceError::InvalidCommand("bad cmd".to_string());
        assert_eq!(err.to_string(), "Invalid command: bad cmd");

        let err = NamespaceError::SecurityFailed("seccomp failed".to_string());
        assert_eq!(err.to_string(), "Security setup failed: seccomp failed");
    }
}
