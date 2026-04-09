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

/// Run the PTY server, listening on vsock port 4090.
///
/// On Linux, binds to `AF_VSOCK` with `VMADDR_CID_ANY`.
/// On non-Linux platforms, this is a no-op (development stub).
pub fn run_pty_server() -> Result<(), Box<dyn std::error::Error>> {
    info!("Starting PTY server on vsock port {}", PTY_VSOCK_PORT);

    #[cfg(target_os = "linux")]
    {
        run_vsock_pty_server()?;
    }

    #[cfg(not(target_os = "linux"))]
    {
        info!("PTY server not available on non-Linux platform (development mode)");
    }

    Ok(())
}

/// Linux vsock PTY server implementation.
#[cfg(target_os = "linux")]
fn run_vsock_pty_server() -> Result<(), Box<dyn std::error::Error>> {
    use nix::sys::socket::{
        accept, bind, listen, socket, AddressFamily, Backlog, SockFlag, SockType, VsockAddr,
    };
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

    let sock_fd = socket(
        AddressFamily::Vsock,
        SockType::Stream,
        SockFlag::empty(),
        None,
    )?;

    // Set CLOEXEC manually since SOCK_CLOEXEC isn't available in nix 0.29 on macOS
    unsafe {
        libc::fcntl(sock_fd.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC);
    }

    let addr = VsockAddr::new(libc::VMADDR_CID_ANY, PTY_VSOCK_PORT);
    bind(sock_fd.as_raw_fd(), &addr)?;
    listen(&sock_fd, Backlog::new(4)?)?;

    info!("PTY server listening on vsock port {}", PTY_VSOCK_PORT);

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
    use nix::unistd::{close, dup2, execvp, fork, setsid, ForkResult};
    use std::ffi::CString;
    use std::os::fd::{AsRawFd, FromRawFd};

    let raw_fd = fd.as_raw_fd();
    let mut stream = unsafe { std::fs::File::from_raw_fd(raw_fd) };

    // Step 1: Read PtyRequest
    let (frame_type, payload) = match read_frame(&mut stream)? {
        Some(f) => f,
        None => {
            std::mem::forget(fd);
            return Ok(());
        }
    };

    let request = match parse_frame(frame_type, payload)? {
        PtyFrame::Request(req) => req,
        _ => {
            write_error(&mut stream, "Expected PtyRequest frame")?;
            std::mem::forget(fd);
            return Ok(());
        }
    };

    if request.cmd.is_empty() {
        write_error(&mut stream, "Empty command")?;
        std::mem::forget(fd);
        return Ok(());
    }

    info!(cmd = ?request.cmd, "PTY session starting");

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
                close(slave_fd.as_raw_fd()).ok();
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

            // Apply working directory
            if let Some(ref dir) = request.working_dir {
                let _ = std::env::set_current_dir(dir);
            }

            // Build command: if user is specified, wrap with su
            let (program, args) = if let Some(ref user) = request.user {
                let shell_cmd = request
                    .cmd
                    .iter()
                    .map(|a| shell_escape(a))
                    .collect::<Vec<_>>()
                    .join(" ");
                (
                    "su".to_string(),
                    vec![
                        "-s".to_string(),
                        "/bin/sh".to_string(),
                        user.clone(),
                        "-c".to_string(),
                        shell_cmd,
                    ],
                )
            } else {
                (request.cmd[0].clone(), request.cmd[1..].to_vec())
            };

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

            let exit_code = relay_pty_data(&mut stream, &master_fd, child);

            // Send exit frame
            write_exit(&mut stream, exit_code).ok();

            info!(exit_code, "PTY session ended");

            // Prevent double-close: stream owns the fd
            std::mem::forget(fd);
            Ok(())
        }
    }
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
        parse_frame, read_frame, write_data, PtyFrame, FRAME_PTY_DATA, FRAME_PTY_RESIZE,
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
        match waitpid(child, None) {
            Ok(WaitStatus::Exited(_, code)) => exit_code = code,
            Ok(WaitStatus::Signaled(_, sig, _)) => exit_code = 128 + sig as i32,
            _ => exit_code = 1,
        }
    }

    exit_code
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

/// Minimal shell escaping for a single argument.
#[cfg(target_os = "linux")]
fn shell_escape(s: &str) -> String {
    if s.chars()
        .all(|c| c.is_alphanumeric() || c == '_' || c == '-' || c == '/' || c == '.')
    {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', "'\\''"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_pty_vsock_port_constant() {
        assert_eq!(PTY_VSOCK_PORT, 4090);
    }
}
