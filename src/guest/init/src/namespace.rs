//! Linux namespace isolation for agent and business code.
//!
//! Provides utilities to spawn processes in isolated namespaces
//! with seccomp filtering, capability dropping, and no-new-privileges.

#[cfg(target_os = "linux")]
use nix::sched::{unshare, CloneFlags};

use nix::unistd::{fork, ForkResult};
#[cfg(target_os = "linux")]
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
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
            net: false,    // Share network for communication
            user: false,   // Disabled by default (requires UID mapping setup)
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
    user: Option<&str>,
) -> Result<u32, NamespaceError> {
    tracing::info!(
        command = %command,
        args = ?args,
        workdir = %workdir,
        user = ?user,
        "Spawning process in isolated namespace"
    );

    // Fork to create child process
    match unsafe { fork() }.map_err(NamespaceError::ForkFailed)? {
        ForkResult::Child => {
            // Child process: create namespaces and exec
            if let Err(e) = child_process(config, command, args, env, workdir, user) {
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
    user: Option<&str>,
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

    // Execute the command. `Command::exec` uses PATH for bare command names, so
    // the preflight check needs to mirror that instead of statting "sleep".
    if let Some(command_path) = resolve_command_path(command, env) {
        let metadata = std::fs::metadata(&command_path).ok();
        tracing::debug!(
            path = %command_path.display(),
            size = metadata.as_ref().map(|m| m.len()).unwrap_or(0),
            executable = metadata
                .as_ref()
                .map(|m| m.permissions().mode() & 0o111 != 0)
                .unwrap_or(false),
            "Command file resolved"
        );
    } else {
        tracing::warn!(
            command,
            "Command could not be resolved before exec; exec will report the final error"
        );
    }

    let mut cmd = Command::new(command);
    cmd.args(args).current_dir(workdir);

    // Set environment variables
    for (key, value) in env {
        cmd.env(key, value);
    }

    // Resolve the container user (image USER / --user) against the container
    // rootfs (already pivoted to "/"), the same way the exec server does:
    // names -> uid:gid via /etc/passwd, default the primary gid from passwd,
    // and gather image supplemental groups. Done here (pre-fork, allocating) so
    // the pre_exec hook only performs async-signal-safe syscalls.
    let (process_user, supplemental_groups) = resolve_user_and_groups(user);

    // Apply security restrictions + user before exec
    apply_security_before_exec(&mut cmd, process_user, supplemental_groups)?;

    tracing::debug!("Executing command: {} {:?}", command, args);

    // Replace current process with the command
    let err = cmd.exec();

    // If exec returns, it failed
    Err(NamespaceError::ExecFailed(err))
}

fn resolve_command_path(command: &str, env: &[(&str, &str)]) -> Option<PathBuf> {
    if command.contains('/') {
        let path = PathBuf::from(command);
        return path.exists().then_some(path);
    }

    let path_env = env
        .iter()
        .rev()
        .find_map(|(key, value)| (*key == "PATH").then_some((*value).to_string()))
        .or_else(|| std::env::var("PATH").ok())?;

    path_env
        .split(':')
        .filter(|dir| !dir.is_empty())
        .map(|dir| PathBuf::from(dir).join(command))
        .find(|path| path.exists())
}

/// Resolve a container user string (`uid`, `uid:gid`, `root`, or a name) to a
/// numeric [`ProcessUser`] plus its image supplementary groups, looking names up
/// in the container rootfs (already pivoted to `/`). Returns `(None, [])` when no
/// user is requested or the name cannot be resolved/parsed (the process then runs
/// as root, the prior behaviour). Pure file reads — call pre-fork.
#[cfg(target_os = "linux")]
fn resolve_user_and_groups(user: Option<&str>) -> (Option<crate::user::ProcessUser>, Vec<u32>) {
    let Some(user) = user.map(str::trim).filter(|u| !u.is_empty()) else {
        return (None, Vec::new());
    };
    // Names -> "uid:gid" via the container /etc/passwd; numeric/root pass through.
    let resolved = crate::user::resolve_named_user(user, "/").unwrap_or_else(|| user.to_string());
    let mut process_user = match crate::user::parse_process_user(Some(&resolved)) {
        Ok(Some(pu)) => pu,
        _ => {
            tracing::warn!(user, "Could not resolve container user; running as root");
            return (None, Vec::new());
        }
    };
    // Default the primary gid from the user's passwd entry (RunAsUser semantics).
    if process_user.gid.is_none() {
        process_user.gid = crate::user::primary_gid_for_uid("/", process_user.uid);
    }
    let groups = crate::user::resolve_image_groups("/", process_user.uid, process_user.gid, user);
    (Some(process_user), groups)
}

/// Apply security restrictions (seccomp, no-new-privileges, capabilities) and
/// the container user before exec using the pre_exec hook.
///
/// Reads security configuration from `A3S_SEC_*` environment variables
/// set by the host runtime.
#[cfg(target_os = "linux")]
fn apply_security_before_exec(
    cmd: &mut Command,
    process_user: Option<crate::user::ProcessUser>,
    supplemental_groups: Vec<u32>,
) -> Result<(), NamespaceError> {
    use a3s_box_core::security::{SeccompMode, SecurityConfig};

    let config = SecurityConfig::from_env_vars();

    // Privileged mode skips seccomp/caps/no-new-privs — but the container USER
    // (image USER / --user) is still honored, so it is applied below regardless.
    let privileged = config.privileged;
    if privileged {
        tracing::info!("Privileged mode: skipping seccomp/caps/no-new-privs");
    }

    tracing::debug!(
        seccomp = ?config.seccomp,
        no_new_privs = config.no_new_privileges,
        cap_add = ?config.cap_add,
        cap_drop = ?config.cap_drop,
        user = ?process_user,
        "Applying security configuration"
    );

    let no_new_privs = config.no_new_privileges && !privileged;
    let seccomp_mode = if privileged {
        SeccompMode::Unconfined
    } else {
        config.seccomp.clone()
    };
    let cap_drop = if privileged {
        Vec::new()
    } else {
        config.cap_drop.clone()
    };

    // Build the seccomp BPF filter BEFORE fork. Building allocates, which is
    // not async-signal-safe in the post-fork child (malloc may deadlock on
    // musl); the child only installs the prebuilt filter.
    let seccomp_filter = if matches!(seccomp_mode, SeccompMode::Default) {
        Some(build_default_bpf_filter())
    } else {
        None
    };

    // Use pre_exec to apply security in the child process right before exec
    // SAFETY: pre_exec runs after fork, before exec. We only call
    // async-signal-safe operations (prctl, seccomp) — the seccomp filter is
    // built above, pre-fork.
    unsafe {
        cmd.pre_exec(move || {
            // 1. Set no-new-privileges (does not block the setuid syscall below).
            if no_new_privs {
                let ret = libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0);
                if ret != 0 {
                    return Err(std::io::Error::last_os_error());
                }
            }

            // 2. Apply the container user, in the proven order used by the exec
            //    server: supplemental groups -> capabilities -> setgid+setuid.
            //    Each step needs root/CAP_SET*; setuid is LAST because it clears
            //    the privileges needed by the earlier ones.
            if process_user.is_some() && !supplemental_groups.is_empty() {
                let ret = libc::setgroups(
                    supplemental_groups.len() as _,
                    supplemental_groups.as_ptr() as *const libc::gid_t,
                );
                if ret != 0 {
                    return Err(std::io::Error::last_os_error());
                }
            }

            // 3. Drop capabilities (while still root, before the uid switch).
            if should_drop_caps(&cap_drop) {
                drop_capabilities(&cap_drop)?;
            }

            // 4. Drop to the target uid/gid (image USER / --user).
            if let Some(user) = process_user {
                user.apply()?;
            }

            // 5. Apply seccomp filter (prebuilt before fork)
            match &seccomp_mode {
                SeccompMode::Default => {
                    if let Some(filter) = &seccomp_filter {
                        install_seccomp_filter(filter)?;
                    }
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

/// Set `PR_SET_NO_NEW_PRIVS` on the calling thread.
///
/// **Async-signal-safe**: a single `prctl` syscall, safe to call in the
/// post-`fork` child. Once set, no subsequent `execve` can gain privileges via
/// setuid/setgid bits or file capabilities, and the bit is preserved across the
/// exec.
#[cfg(target_os = "linux")]
pub(crate) fn set_no_new_privs() -> Result<(), std::io::Error> {
    let ret = unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) };
    if ret != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

/// Drop Linux capabilities using prctl.
///
/// Drops capabilities from the bounding set AND clears the effective,
/// permitted, and inheritable sets to prevent retention of already-held caps.
/// Supports "ALL" to drop all capabilities, or individual capability names.
#[cfg(target_os = "linux")]
pub(crate) fn drop_capabilities(cap_drop: &[String]) -> Result<(), std::io::Error> {
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

    // The bounding-set drop above only limits future execve(); a process that
    // already holds the capabilities keeps them. Clear them from the effective,
    // permitted, and inheritable sets so the drop actually takes effect, then
    // clear the ambient set so they cannot be re-acquired.
    clear_effective_caps(cap_drop)?;
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

/// Clear the dropped capabilities from the **effective, permitted, and
/// inheritable** sets via `capset(2)`.
///
/// `PR_CAPBSET_DROP` only limits what a future `execve` can gain — it does NOT
/// remove a capability the process currently holds. A privileged (root)
/// container therefore keeps e.g. `CAP_NET_ADMIN` in its effective set unless we
/// clear it here, so `capset` is required for `drop_capabilities` to actually
/// take effect. Async-signal-safe: only `capget`/`capset` syscalls over
/// stack-resident structs, no allocation, so it is safe in a post-fork child.
#[cfg(target_os = "linux")]
fn clear_effective_caps(cap_drop: &[String]) -> Result<(), std::io::Error> {
    const LINUX_CAPABILITY_VERSION_3: u32 = 0x2008_0522;
    // Defined locally — this libc version does not export the capability structs.
    // Stable kernel ABI: capget/capset(hdr {version,pid}, data[2] {eff,perm,inh}).
    #[repr(C)]
    struct CapHeader {
        version: u32,
        pid: i32,
    }
    #[repr(C)]
    #[derive(Clone, Copy)]
    struct CapData {
        effective: u32,
        permitted: u32,
        inheritable: u32,
    }

    let mut header = CapHeader {
        version: LINUX_CAPABILITY_VERSION_3,
        pid: 0,
    };
    let mut data = [CapData {
        effective: 0,
        permitted: 0,
        inheritable: 0,
    }; 2];

    if unsafe {
        libc::syscall(
            libc::SYS_capget,
            &mut header as *mut CapHeader,
            data.as_mut_ptr(),
        )
    } != 0
    {
        return Err(std::io::Error::last_os_error());
    }

    if cap_drop.iter().any(|c| c == "ALL") {
        for word in data.iter_mut() {
            word.effective = 0;
            word.permitted = 0;
            word.inheritable = 0;
        }
    } else {
        for name in cap_drop {
            if let Some(cap) = cap_name_to_number(name) {
                let word = (cap / 32) as usize;
                if word < data.len() {
                    let mask = !(1u32 << (cap % 32));
                    data[word].effective &= mask;
                    data[word].permitted &= mask;
                    data[word].inheritable &= mask;
                }
            }
        }
    }

    if unsafe {
        libc::syscall(
            libc::SYS_capset,
            &mut header as *mut CapHeader,
            data.as_ptr(),
        )
    } != 0
    {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

/// Restrict the process to exactly the capability set named in `keep`.
///
/// This is how a **non-privileged** CRI container gets the runtime default
/// capability set (e.g. without `CAP_NET_ADMIN`/`CAP_SYS_ADMIN`): the CRI
/// resolves `(default ∪ add) − drop` and passes it here. Every capability NOT
/// in `keep` is dropped from the bounding set, and the effective/permitted/
/// inheritable sets are reduced to exactly `keep` — a reduction from the full
/// root set, which is always permitted. An empty `keep` drops everything.
///
/// Async-signal-safe: only `prctl` + `capset` over stack-resident structs, no
/// allocation (names are resolved into a fixed-size bitmask), so it is safe in
/// the post-fork child.
#[cfg(target_os = "linux")]
pub(crate) fn restrict_capabilities_to_keep(keep: &[String]) -> Result<(), std::io::Error> {
    // Resolve the keep names into a 64-bit capability bitmask.
    let mut mask = [0u32; 2];
    for name in keep {
        if let Some(cap) = cap_name_to_number(name) {
            let word = (cap / 32) as usize;
            if word < mask.len() {
                mask[word] |= 1u32 << (cap % 32);
            }
        }
    }

    // Drop every capability not kept from the bounding set so a future execve
    // cannot regain it.
    for cap in 0..=40_i32 {
        let word = (cap / 32) as usize;
        let kept = word < mask.len() && (mask[word] & (1u32 << (cap % 32))) != 0;
        if !kept {
            let ret = unsafe { libc::prctl(24, cap, 0, 0, 0) }; // PR_CAPBSET_DROP
            if ret != 0 {
                let err = std::io::Error::last_os_error();
                if err.raw_os_error() != Some(libc::EINVAL) {
                    return Err(err);
                }
            }
        }
    }

    set_capability_sets(mask)?;
    clear_ambient_and_inheritable_caps()?;
    Ok(())
}

/// Set the effective/permitted/inheritable capability sets to exactly `mask`
/// via `capset(2)`. Reducing the sets from the inherited (full-root) set is
/// always permitted; this never tries to raise a capability the process lacks.
#[cfg(target_os = "linux")]
fn set_capability_sets(mask: [u32; 2]) -> Result<(), std::io::Error> {
    const LINUX_CAPABILITY_VERSION_3: u32 = 0x2008_0522;
    #[repr(C)]
    struct CapHeader {
        version: u32,
        pid: i32,
    }
    #[repr(C)]
    #[derive(Clone, Copy)]
    struct CapData {
        effective: u32,
        permitted: u32,
        inheritable: u32,
    }

    let mut header = CapHeader {
        version: LINUX_CAPABILITY_VERSION_3,
        pid: 0,
    };
    let data = [
        CapData {
            effective: mask[0],
            permitted: mask[0],
            inheritable: mask[0],
        },
        CapData {
            effective: mask[1],
            permitted: mask[1],
            inheritable: mask[1],
        },
    ];

    if unsafe {
        libc::syscall(
            libc::SYS_capset,
            &mut header as *mut CapHeader,
            data.as_ptr(),
        )
    } != 0
    {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

/// Map a Linux capability name to its numeric value.
#[cfg(target_os = "linux")]
fn cap_name_to_number(name: &str) -> Option<i32> {
    // Accept both "NET_ADMIN" and "CAP_NET_ADMIN" (CRI naming varies).
    let name = name.strip_prefix("CAP_").unwrap_or(name);
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
/// Install a prebuilt seccomp BPF filter on the current thread.
///
/// **Async-signal-safe**: performs only `prctl` and the `seccomp` syscall and
/// reads the caller-owned `filter` slice — it does NOT allocate. It is therefore
/// safe to call from a post-fork `pre_exec` hook PROVIDED the filter was built
/// *before* the fork (see [`build_default_bpf_filter`]); building the filter
/// allocates and must never run in the child of a multi-threaded process.
///
/// Sets `PR_SET_NO_NEW_PRIVS` (required for unprivileged seccomp) then loads the
/// filter via `SECCOMP_SET_MODE_FILTER`, putting the process in
/// `SECCOMP_MODE_FILTER` (`/proc/self/status` `Seccomp: 2`). The default filter
/// returns `EPERM` for the syscalls listed in [`build_default_bpf_filter`].
#[cfg(target_os = "linux")]
pub(crate) fn install_seccomp_filter(filter: &[libc::sock_filter]) -> Result<(), std::io::Error> {
    // First, ensure no-new-privs is set (required for unprivileged seccomp).
    let ret = unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) };
    if ret != 0 {
        return Err(std::io::Error::last_os_error());
    }

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

/// Build the default (RuntimeDefault) BPF seccomp filter: allow-default with
/// `ERRNO(EPERM)` for the dangerous syscalls listed below.
#[cfg(target_os = "linux")]
pub(crate) fn build_default_bpf_filter() -> Vec<libc::sock_filter> {
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

    build_seccomp_errno_filter(blocked_syscalls)
}

/// Build an allow-default seccomp BPF filter that returns `ERRNO(EPERM)` for the
/// given blocked syscall numbers. Shared by [`build_default_bpf_filter`] and CRI
/// localhost profiles, which are `defaultAction: SCMP_ACT_ALLOW` plus a list of
/// `SCMP_ACT_ERRNO` syscalls.
#[cfg(target_os = "linux")]
pub(crate) fn build_seccomp_errno_filter(blocked_syscalls: &[u32]) -> Vec<libc::sock_filter> {
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
        jt: 0,                       // continue to syscall check
        jf: (num_blocked + 2) as u8, // jump to kill (past load + all checks + allow)
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

/// Map an OCI seccomp syscall name to its number for the guest architecture.
///
/// Covers the syscalls used by the CRI localhost-profile conformance plus a few
/// common ones; unknown names return `None` and are skipped (best-effort —
/// a profile entry the runtime can't map simply isn't enforced). On aarch64 the
/// legacy `chmod` syscall does not exist (libc uses `fchmodat`), so it maps to
/// `None` there.
#[cfg(target_os = "linux")]
pub(crate) fn syscall_name_to_number(name: &str) -> Option<u32> {
    #[cfg(target_arch = "x86_64")]
    let n: u32 = match name {
        "chmod" => 90,
        "fchmod" => 91,
        "fchmodat" => 268,
        "sethostname" => 170,
        "setdomainname" => 171,
        "mount" => 165,
        "umount2" => 166,
        "chroot" => 161,
        "ptrace" => 101,
        "reboot" => 169,
        _ => return None,
    };
    #[cfg(target_arch = "aarch64")]
    let n: u32 = match name {
        "fchmod" => 52,
        "fchmodat" => 53,
        "sethostname" => 161,
        "setdomainname" => 162,
        "mount" => 40,
        "umount2" => 39,
        "chroot" => 51,
        "ptrace" => 117,
        "reboot" => 142,
        _ => return None,
    };
    Some(n)
}

/// Child process logic for non-Linux platforms (development stub).
#[cfg(not(target_os = "linux"))]
fn child_process(
    _config: &NamespaceConfig,
    command: &str,
    args: &[&str],
    env: &[(&str, &str)],
    workdir: &str,
    _user: Option<&str>,
) -> Result<(), NamespaceError> {
    // On non-Linux, just exec without namespace isolation or security
    tracing::warn!("Namespace isolation and security enforcement not available on this platform");

    let mut cmd = Command::new(command);
    cmd.args(args).current_dir(workdir);

    for (key, value) in env {
        cmd.env(key, value);
    }

    if let Some(command_path) = resolve_command_path(command, env) {
        tracing::debug!(path = %command_path.display(), "Command file resolved");
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
    fn test_resolve_command_path_absolute() {
        let path = resolve_command_path("/bin/sh", &[]);
        assert_eq!(path, Some(PathBuf::from("/bin/sh")));
    }

    #[test]
    fn test_resolve_command_path_from_env_path() {
        let path = resolve_command_path("sh", &[("PATH", "/bin:/usr/bin")]);
        assert_eq!(path, Some(PathBuf::from("/bin/sh")));
    }

    #[test]
    fn test_resolve_command_path_missing() {
        let path = resolve_command_path("definitely-not-an-a3s-command", &[("PATH", "/bin")]);
        assert!(path.is_none());
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
