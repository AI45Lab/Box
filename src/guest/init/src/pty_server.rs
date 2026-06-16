//! Guest PTY server for interactive terminal sessions inside the VM.
//!
//! Listens on vsock port 4090 and allocates a PTY for each connection,
//! providing bidirectional streaming between the host CLI and a shell
//! process running inside the guest.

#[cfg(target_os = "linux")]
use std::time::Duration;

use a3s_box_core::pty::PTY_VSOCK_PORT;
use tracing::info;
#[cfg(target_os = "linux")]
use tracing::{error, warn};

#[cfg(target_os = "linux")]
use crate::user::parse_process_user;

/// A bound, listening PTY-server socket — produced by [`bind_pty_server`] and
/// consumed by [`serve_pty_server`]. Same early-bind rationale as
/// [`crate::exec_server::ExecListener`]: bind on the main thread before the
/// container fork (fork-safe, fills the listen backlog), accept later in a
/// thread. On non-Linux this is an inert placeholder.
#[cfg(target_os = "linux")]
pub struct PtyListener(std::os::fd::OwnedFd);
#[cfg(not(target_os = "linux"))]
pub struct PtyListener;

/// Bind + listen the PTY vsock socket (port 4090). Pure socket syscalls, safe to
/// call on the main thread before the container fork.
pub fn bind_pty_server() -> Result<PtyListener, Box<dyn std::error::Error>> {
    #[cfg(target_os = "linux")]
    {
        use nix::sys::socket::{
            bind, listen, socket, AddressFamily, Backlog, SockFlag, SockType, VsockAddr,
        };
        use std::os::fd::AsRawFd;

        let sock_fd = socket(
            AddressFamily::Vsock,
            SockType::Stream,
            SockFlag::empty(),
            None,
        )?;

        // Set CLOEXEC manually since SOCK_CLOEXEC isn't available in nix 0.29 on
        // macOS — and so the forked container never inherits the listening socket.
        unsafe {
            libc::fcntl(sock_fd.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC);
        }

        let addr = VsockAddr::new(libc::VMADDR_CID_ANY, PTY_VSOCK_PORT);
        bind(sock_fd.as_raw_fd(), &addr)?;
        listen(&sock_fd, Backlog::new(4)?)?;

        info!("PTY server listening on vsock port {}", PTY_VSOCK_PORT);
        Ok(PtyListener(sock_fd))
    }

    #[cfg(not(target_os = "linux"))]
    {
        info!("PTY server not available on non-Linux platform (development mode)");
        Ok(PtyListener)
    }
}

/// Run the PTY accept loop on an already-bound listener. Intended to run on its
/// own thread for the VM's lifetime; never returns under normal operation.
pub fn serve_pty_server(listener: PtyListener) -> Result<(), Box<dyn std::error::Error>> {
    #[cfg(target_os = "linux")]
    {
        run_pty_accept_loop(listener.0)
    }

    #[cfg(not(target_os = "linux"))]
    {
        let _ = listener;
        Ok(())
    }
}

/// Bind then serve in one call. Kept for callers that don't need the early-bind
/// split; guest-init's boot path uses `bind_*` + `serve_*` directly.
pub fn run_pty_server() -> Result<(), Box<dyn std::error::Error>> {
    info!("Starting PTY server on vsock port {}", PTY_VSOCK_PORT);
    serve_pty_server(bind_pty_server()?)
}

/// The PTY server accept loop.
#[cfg(target_os = "linux")]
fn run_pty_accept_loop(sock_fd: std::os::fd::OwnedFd) -> Result<(), Box<dyn std::error::Error>> {
    use nix::sys::socket::accept;
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

    loop {
        match accept(sock_fd.as_raw_fd()) {
            Ok(client_fd) => {
                let client = unsafe { OwnedFd::from_raw_fd(client_fd) };
                // Handle each PTY session in its own thread
                std::thread::spawn(move || {
                    if let Err(e) = handle_pty_connection(client) {
                        warn!("PTY session failed: {}", e);
                    }
                });
            }
            Err(e) => {
                error!("PTY accept failed: {}", e);
                std::thread::sleep(Duration::from_millis(100));
            }
        }
    }
}

/// Handle a single PTY connection.
///
/// 1. Read PtyRequest frame
/// 2. Allocate PTY via openpty()
/// 3. Fork + exec command on the slave side
/// 4. Bidirectional relay: vsock ↔ PTY master fd
/// 5. Handle PtyResize frames
/// 6. On process exit → send PtyExit frame
#[cfg(target_os = "linux")]
fn handle_pty_connection(fd: std::os::fd::OwnedFd) -> Result<(), Box<dyn std::error::Error>> {
    use a3s_box_core::pty::{parse_frame, read_frame, write_error, write_exit, PtyFrame};
    use nix::pty::openpty;
    use nix::unistd::{dup2, execvp, fork, setsid, ForkResult};
    use std::ffi::CString;
    use std::os::fd::AsRawFd;

    let mut stream = std::fs::File::from(fd);

    // Step 1: Read PtyRequest
    let (frame_type, payload) = match read_frame(&mut stream)? {
        Some(f) => f,
        None => {
            return Ok(());
        }
    };

    let request = match parse_frame(frame_type, payload)? {
        PtyFrame::Request(req) => req,
        _ => {
            write_error(&mut stream, "Expected PtyRequest frame")?;
            return Ok(());
        }
    };

    if request.cmd.is_empty() {
        write_error(&mut stream, "Empty command")?;
        return Ok(());
    }
    if let Err(error) =
        validate_rootfs_request(request.rootfs.as_deref(), request.working_dir.as_deref())
    {
        write_error(&mut stream, &error)?;
        return Ok(());
    }
    let process_user = match parse_process_user(request.user.as_deref()) {
        Ok(user) => user,
        Err(error) => {
            write_error(&mut stream, &error)?;
            return Ok(());
        }
    };

    info!(cmd = ?request.cmd, "PTY session starting");

    // Parse the A3S_SEC_* confinement controls from the request env (same keys
    // and KEY=VALUE format the exec path consumes) and build the seccomp filter
    // BEFORE the fork — building a filter allocates, and the post-fork child must
    // stay async-signal-safe. The TTY workload previously applied NONE of these,
    // running with full capabilities, no seccomp and no_new_privs unset despite
    // the pod's securityContext (#11). The same async-signal-safe namespace::
    // primitives the exec path uses are applied in the child below.
    let sec_supplemental_groups: Vec<u32> = request
        .env
        .iter()
        .find_map(|entry| entry.strip_prefix("A3S_SEC_SUPPLEMENTAL_GROUPS="))
        .map(|csv| {
            csv.split(',')
                .filter_map(|gid| gid.trim().parse::<u32>().ok())
                .collect()
        })
        .unwrap_or_default();
    let sec_cap_drop: Vec<String> = request
        .env
        .iter()
        .find_map(|entry| entry.strip_prefix("A3S_SEC_CAP_DROP="))
        .map(|csv| {
            csv.split(',')
                .map(|name| name.trim().to_string())
                .filter(|name| !name.is_empty())
                .collect()
        })
        .unwrap_or_default();
    let sec_cap_keep: Option<Vec<String>> = request
        .env
        .iter()
        .find_map(|entry| entry.strip_prefix("A3S_SEC_CAP_KEEP="))
        .map(|csv| {
            csv.split(',')
                .map(|name| name.trim().to_string())
                .filter(|name| !name.is_empty())
                .collect()
        });
    let sec_no_new_privs = request
        .env
        .iter()
        .any(|entry| entry == "A3S_SEC_NO_NEW_PRIVS=1");
    #[cfg(target_os = "linux")]
    let seccomp_filter: Option<Vec<libc::sock_filter>> = {
        let localhost: Vec<u32> = request
            .env
            .iter()
            .find_map(|entry| entry.strip_prefix("A3S_SEC_SECCOMP_LOCALHOST="))
            .map(|csv| {
                csv.split(',')
                    .filter_map(|name| crate::namespace::syscall_name_to_number(name.trim()))
                    .collect()
            })
            .unwrap_or_default();
        let apply_default = request
            .env
            .iter()
            .any(|entry| entry == "A3S_SEC_SECCOMP=default");
        if !localhost.is_empty() {
            Some(crate::namespace::build_seccomp_errno_filter(&localhost))
        } else if apply_default {
            Some(crate::namespace::build_default_bpf_filter())
        } else {
            None
        }
    };
    #[cfg(not(target_os = "linux"))]
    let _ = (&sec_cap_drop, &sec_cap_keep, sec_no_new_privs);

    // Create the per-container cgroup from the A3S_SEC_* limits so the TTY
    // workload is bounded by cpu.max / pids.max / memory.* like the exec path —
    // the PTY path previously created NO cgroup, so tty containers escaped every
    // resource limit. Created here in the parent (allocates); the child joins it
    // (writes its PID to cgroup.procs) before chroot. The handle lives for the
    // connection so the cgroup dir is removed once the child exits.
    #[cfg(target_os = "linux")]
    let parse_u64 = |prefix: &str| {
        request
            .env
            .iter()
            .find_map(|entry| entry.strip_prefix(prefix))
            .and_then(|value| value.trim().parse::<u64>().ok())
    };
    #[cfg(target_os = "linux")]
    let parse_i64 = |prefix: &str| {
        request
            .env
            .iter()
            .find_map(|entry| entry.strip_prefix(prefix))
            .and_then(|value| value.trim().parse::<i64>().ok())
    };
    #[cfg(target_os = "linux")]
    let _container_cgroup = crate::cgroup::ContainerCgroup::create(
        parse_u64("A3S_SEC_MEM_LIMIT="),
        parse_u64("A3S_SEC_MEM_LOW="),
        parse_i64("A3S_SEC_MEM_SWAP="),
        parse_i64("A3S_SEC_CPU_QUOTA="),
        parse_u64("A3S_SEC_CPU_PERIOD="),
        parse_u64("A3S_SEC_CPU_SHARES="),
        parse_u64("A3S_SEC_PIDS_LIMIT="),
    );
    #[cfg(target_os = "linux")]
    let cgroup_procs: Option<std::ffi::CString> = _container_cgroup
        .as_ref()
        .and_then(|cgroup| std::ffi::CString::new(cgroup.procs_path()).ok());

    // Set up the container rootfs before the fork — the child shares this mount
    // namespace and chroots into it. The exec path does all of this per spawn;
    // the PTY path did NONE of it, so a tty container was missing /proc + /dev and
    // its securityContext path restrictions went unenforced. Mirror the exec path
    // exactly (same critest-validated functions), in the same order.
    #[cfg(target_os = "linux")]
    if let Some(ref rootfs) = request.rootfs {
        // /proc + /sys, then the standard /dev nodes — many workloads read
        // /proc/self/* or /dev/urandom and won't start without them. Idempotent.
        crate::exec_server::ensure_container_pseudo_filesystems(rootfs);
        crate::exec_server::ensure_container_dev_nodes(rootfs);
        // CRI MaskedPaths / ReadonlyPaths (best-effort; a path that doesn't exist
        // is skipped) — these need /proc + /sys mounted above.
        let masked = crate::exec_server::parse_sec_path_list(&request.env, "A3S_SEC_MASKED_PATHS=");
        let readonly =
            crate::exec_server::parse_sec_path_list(&request.env, "A3S_SEC_READONLY_PATHS=");
        if !masked.is_empty() || !readonly.is_empty() {
            crate::exec_server::apply_container_path_restrictions(rootfs, &masked, &readonly);
        }
        // readOnlyRootFilesystem — last, after /proc, /sys and inner mounts are up.
        if request
            .env
            .iter()
            .any(|entry| entry == "A3S_SEC_READONLY_ROOTFS=1")
        {
            crate::exec_server::remount_rootfs_readonly(rootfs);
        }
    }

    // Step 2: Allocate PTY
    let pty = openpty(None, None)?;
    let master_fd = pty.master;
    let slave_fd = pty.slave;

    // Set initial terminal size
    set_winsize(master_fd.as_raw_fd(), request.cols, request.rows);

    // Step 3: Fork
    match unsafe { fork()? } {
        ForkResult::Child => {
            // Child: set up PTY slave as stdin/stdout/stderr, then exec
            drop(master_fd);

            // Join the per-container cgroup FIRST, before chroot makes
            // /sys/fs/cgroup unreachable, so this process and everything it forks
            // is bounded from birth. Async-signal-safe: open + getpid + a
            // stack-only itoa + write + close, no allocation.
            #[cfg(target_os = "linux")]
            if let Some(ref procs) = cgroup_procs {
                let fd = unsafe { libc::open(procs.as_ptr(), libc::O_WRONLY) };
                if fd >= 0 {
                    let mut buf = [0u8; 20];
                    let mut i = buf.len();
                    let mut n = unsafe { libc::getpid() } as u64;
                    if n == 0 {
                        i -= 1;
                        buf[i] = b'0';
                    }
                    while n > 0 {
                        i -= 1;
                        buf[i] = b'0' + (n % 10) as u8;
                        n /= 10;
                    }
                    unsafe {
                        libc::write(
                            fd,
                            buf[i..].as_ptr() as *const libc::c_void,
                            (buf.len() - i) as libc::size_t,
                        );
                        libc::close(fd);
                    }
                }
            }

            // Create new session (detach from controlling terminal)
            setsid().ok();

            // Set controlling terminal
            unsafe {
                libc::ioctl(slave_fd.as_raw_fd(), libc::TIOCSCTTY, 0);
            }

            // Redirect stdio to PTY slave
            dup2(slave_fd.as_raw_fd(), 0).ok(); // stdin
            dup2(slave_fd.as_raw_fd(), 1).ok(); // stdout
            dup2(slave_fd.as_raw_fd(), 2).ok(); // stderr
            if slave_fd.as_raw_fd() > 2 {
                drop(slave_fd);
            }

            // Apply environment variables
            for entry in &request.env {
                if let Some((key, value)) = entry.split_once('=') {
                    std::env::set_var(key, value);
                }
            }

            // Set TERM if not already set
            if std::env::var("TERM").is_err() {
                std::env::set_var("TERM", "xterm-256color");
            }

            let workdir = request.working_dir.as_deref().unwrap_or("/");
            if let Some(ref rootfs) = request.rootfs {
                if let Err(error) = apply_rootfs_chroot(rootfs, workdir) {
                    eprintln!(
                        "Failed to enter PTY rootfs {} with workdir {}: {}",
                        rootfs, workdir, error
                    );
                    std::process::exit(127);
                }
            } else if let Some(ref dir) = request.working_dir {
                let _ = std::env::set_current_dir(dir);
            }

            // Confinement, part 1 (while still root, BEFORE the uid switch):
            // supplemental groups need CAP_SETGID and capset needs CAP_SETPCAP,
            // both cleared once user.apply() drops to a non-root uid. The default
            // keep-set retains CAP_SETUID/CAP_SETGID so user.apply still works.
            if !sec_supplemental_groups.is_empty() {
                let ret = unsafe {
                    libc::setgroups(
                        sec_supplemental_groups.len() as _,
                        sec_supplemental_groups.as_ptr() as *const libc::gid_t,
                    )
                };
                if ret != 0 {
                    eprintln!("Failed to set PTY supplemental groups");
                    std::process::exit(127);
                }
            }
            #[cfg(target_os = "linux")]
            {
                if let Some(ref keep) = sec_cap_keep {
                    if let Err(error) = crate::namespace::restrict_capabilities_to_keep(keep) {
                        eprintln!("Failed to restrict PTY capabilities: {}", error);
                        std::process::exit(127);
                    }
                } else if !sec_cap_drop.is_empty() {
                    if let Err(error) = crate::namespace::drop_capabilities(&sec_cap_drop) {
                        eprintln!("Failed to drop PTY capabilities: {}", error);
                        std::process::exit(127);
                    }
                }
            }

            if let Some(user) = process_user {
                if let Err(error) = user.apply() {
                    eprintln!("Failed to apply PTY user: {}", error);
                    std::process::exit(127);
                }
            }

            // Confinement, part 2 (AFTER the uid switch): no_new_privs before
            // seccomp so a later execve cannot regain privileges, then install
            // the prebuilt seccomp filter last so it is active across execvp.
            #[cfg(target_os = "linux")]
            {
                if sec_no_new_privs {
                    if let Err(error) = crate::namespace::set_no_new_privs() {
                        eprintln!("Failed to set PTY no_new_privs: {}", error);
                        std::process::exit(127);
                    }
                }
                if let Some(ref filter) = seccomp_filter {
                    if let Err(error) = crate::namespace::install_seccomp_filter(filter) {
                        eprintln!("Failed to install PTY seccomp filter: {}", error);
                        std::process::exit(127);
                    }
                }
            }

            let program = request.cmd[0].clone();
            let args = request.cmd[1..].to_vec();

            let c_program =
                CString::new(program.as_str()).unwrap_or_else(|_| CString::new("/bin/sh").unwrap());
            let c_args: Vec<CString> = std::iter::once(c_program.clone())
                .chain(args.iter().map(|a| {
                    CString::new(a.as_str()).unwrap_or_else(|_| CString::new("").unwrap())
                }))
                .collect();

            // execvp replaces the process
            let _ = execvp(&c_program, &c_args);
            // If exec fails, exit
            std::process::exit(127);
        }
        ForkResult::Parent { child } => {
            // Parent: relay data between vsock and PTY master
            drop(slave_fd);

            // Register the PTY child with the reaper so PID 1 leaves it for us to
            // reap (relay_pty_data waitpid's it for the real exit code). The guard
            // unregisters when this branch returns.
            let _reap_guard = crate::reaper::manage_pid(child.as_raw());

            let exit_code = relay_pty_data(&mut stream, &master_fd, child);

            // Send exit frame
            write_exit(&mut stream, exit_code).ok();

            info!(exit_code, "PTY session ended");

            Ok(())
        }
    }
}

#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn validate_rootfs_request(rootfs: Option<&str>, working_dir: Option<&str>) -> Result<(), String> {
    let Some(rootfs) = rootfs else {
        return Ok(());
    };
    let workdir = working_dir.unwrap_or("/");

    if rootfs.is_empty()
        || !rootfs.starts_with('/')
        || rootfs.contains('\0')
        || workdir.contains('\0')
    {
        return Err(format!("Invalid rootfs path: {rootfs}"));
    }

    #[cfg(not(target_os = "linux"))]
    {
        Err("Rootfs PTY execution requires a Linux guest".to_string())
    }

    #[cfg(target_os = "linux")]
    match std::fs::metadata(rootfs) {
        Ok(metadata) if metadata.is_dir() => Ok(()),
        Ok(_) => Err(format!("Rootfs path is not a directory: {rootfs}")),
        Err(e) => Err(format!("Rootfs path is unavailable: {rootfs} ({e})")),
    }
}

#[cfg(target_os = "linux")]
fn apply_rootfs_chroot(rootfs: &str, workdir: &str) -> std::io::Result<()> {
    use std::ffi::CString;

    let rootfs = CString::new(rootfs.as_bytes()).map_err(|_| {
        std::io::Error::new(std::io::ErrorKind::InvalidInput, "rootfs contains NUL")
    })?;
    let workdir = CString::new(workdir.as_bytes()).map_err(|_| {
        std::io::Error::new(std::io::ErrorKind::InvalidInput, "workdir contains NUL")
    })?;

    unsafe {
        if libc::chroot(rootfs.as_ptr()) != 0 {
            return Err(std::io::Error::last_os_error());
        }
        if libc::chdir(workdir.as_ptr()) != 0 {
            return Err(std::io::Error::last_os_error());
        }
    }

    Ok(())
}

/// Bidirectional relay between the vsock stream and the PTY master fd.
///
/// Uses poll() to multiplex between:
/// - Data from PTY master → send as PtyData frames to host
/// - Frames from host → write PtyData to PTY master, handle PtyResize
///
/// Returns the child process exit code.
#[cfg(target_os = "linux")]
fn relay_pty_data(
    stream: &mut std::fs::File,
    master: &std::os::fd::OwnedFd,
    child: nix::unistd::Pid,
) -> i32 {
    use a3s_box_core::pty::{
        parse_frame, read_frame, write_data, PtyFrame, FRAME_PTY_DATA, FRAME_PTY_ERROR,
        FRAME_PTY_RESIZE,
    };
    use nix::sys::wait::{waitpid, WaitPidFlag, WaitStatus};
    use std::os::fd::{AsFd, AsRawFd};

    let master_raw = master.as_raw_fd();
    let stream_fd = stream.as_raw_fd();

    // Set both fds to non-blocking
    set_nonblocking(master_raw);
    set_nonblocking(stream_fd);

    let mut pty_buf = [0u8; 4096];
    let mut exit_code = 0i32;
    let mut child_exited = false;

    loop {
        // Poll both fds
        let mut fds = [
            libc::pollfd {
                fd: master_raw,
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: stream_fd,
                events: libc::POLLIN,
                revents: 0,
            },
        ];

        let poll_result = unsafe { libc::poll(fds.as_mut_ptr(), 2, 100) };
        if poll_result < 0 {
            let err = std::io::Error::last_os_error();
            if err.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            break;
        }

        // Check for data from PTY master → send to host
        if fds[0].revents & libc::POLLIN != 0 {
            match nix::unistd::read(master_raw, &mut pty_buf) {
                Ok(0) => break,
                Ok(n) => {
                    if write_data(stream, &pty_buf[..n]).is_err() {
                        break;
                    }
                }
                Err(nix::errno::Errno::EAGAIN) => {}
                Err(nix::errno::Errno::EIO) => {
                    // EIO on PTY master means slave closed (child exited)
                    break;
                }
                Err(_) => break,
            }
        }

        // Check for PTY master hangup
        if fds[0].revents & libc::POLLHUP != 0 {
            // Drain remaining data
            loop {
                match nix::unistd::read(master_raw, &mut pty_buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        if write_data(stream, &pty_buf[..n]).is_err() {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
            break;
        }

        // Check for frames from host → handle
        if fds[1].revents & libc::POLLIN != 0 {
            // Temporarily set stream to blocking for frame read
            set_blocking(stream_fd);
            match read_frame(stream) {
                Ok(Some((ft, payload))) => {
                    match ft {
                        FRAME_PTY_DATA => {
                            // Write to PTY master
                            let _ = nix::unistd::write(master.as_fd(), &payload);
                        }
                        FRAME_PTY_RESIZE => {
                            if let Ok(PtyFrame::Resize(r)) = parse_frame(ft, payload) {
                                set_winsize(master_raw, r.cols, r.rows);
                            }
                        }
                        FRAME_PTY_ERROR if payload.is_empty() => break,
                        _ => {} // Ignore unknown frames
                    }
                }
                Ok(None) => break, // Host disconnected
                Err(_) => break,
            }
            set_nonblocking(stream_fd);
        }

        // Check for host disconnect
        if fds[1].revents & libc::POLLHUP != 0 {
            break;
        }

        // Check if child has exited (non-blocking)
        if !child_exited {
            match waitpid(child, Some(WaitPidFlag::WNOHANG)) {
                Ok(WaitStatus::Exited(_, code)) => {
                    exit_code = code;
                    child_exited = true;
                    // Don't break immediately — drain remaining PTY output
                }
                Ok(WaitStatus::Signaled(_, sig, _)) => {
                    exit_code = 128 + sig as i32;
                    child_exited = true;
                }
                _ => {}
            }
        }

        // If child exited and no more data, we're done
        if child_exited && fds[0].revents & libc::POLLIN == 0 {
            break;
        }
    }

    // Ensure child is reaped
    if !child_exited {
        terminate_pty_child(child);
        match waitpid(child, None) {
            Ok(WaitStatus::Exited(_, code)) => exit_code = code,
            Ok(WaitStatus::Signaled(_, sig, _)) => exit_code = 128 + sig as i32,
            _ => exit_code = 1,
        }
    }

    exit_code
}

#[cfg(target_os = "linux")]
fn terminate_pty_child(child: nix::unistd::Pid) {
    use nix::sys::signal::{kill, Signal};
    use nix::unistd::Pid;

    let pid = child.as_raw();
    if pid > 0 {
        let _ = kill(Pid::from_raw(-pid), Signal::SIGKILL);
        let _ = kill(child, Signal::SIGKILL);
    }
}

/// Set terminal window size on a PTY fd.
#[cfg(target_os = "linux")]
fn set_winsize(fd: std::os::fd::RawFd, cols: u16, rows: u16) {
    let ws = libc::winsize {
        ws_row: rows,
        ws_col: cols,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    unsafe {
        libc::ioctl(fd, libc::TIOCSWINSZ, &ws);
    }
}

/// Set a file descriptor to non-blocking mode.
#[cfg(target_os = "linux")]
fn set_nonblocking(fd: std::os::fd::RawFd) {
    unsafe {
        let flags = libc::fcntl(fd, libc::F_GETFL);
        libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK);
    }
}

/// Set a file descriptor to blocking mode.
#[cfg(target_os = "linux")]
fn set_blocking(fd: std::os::fd::RawFd) {
    unsafe {
        let flags = libc::fcntl(fd, libc::F_GETFL);
        libc::fcntl(fd, libc::F_SETFL, flags & !libc::O_NONBLOCK);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_pty_vsock_port_constant() {
        assert_eq!(PTY_VSOCK_PORT, 4090);
    }

    #[test]
    fn test_validate_rootfs_request_defaults() {
        assert!(validate_rootfs_request(None, Some("/tmp")).is_ok());
    }

    #[test]
    fn test_validate_rootfs_request_rejects_relative_rootfs() {
        let err = validate_rootfs_request(Some("relative/rootfs"), None).unwrap_err();
        assert!(err.contains("Invalid rootfs path"));
    }

    #[test]
    fn test_validate_rootfs_request_rejects_nul_workdir() {
        let err = validate_rootfs_request(Some("/rootfs"), Some("/bad\0dir")).unwrap_err();
        assert!(err.contains("Invalid rootfs path"));
    }
}
