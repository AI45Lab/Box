//! Guest exec server for executing commands inside the VM.
//!
//! Listens on vsock port 4089 and accepts Frame-based requests.
//! Each connection: read a Data frame (JSON ExecRequest), execute,
//! send a Data frame (JSON ExecOutput), close.

use std::io::Read;
use std::io::Write;
use std::time::Duration;

use a3s_box_core::exec::{ExecOutput, DEFAULT_EXEC_TIMEOUT_NS, MAX_OUTPUT_BYTES};
#[cfg(any(target_os = "linux", test))]
use a3s_transport::frame::FrameType;
use tracing::{info, warn};

/// Vsock port for the exec server.
pub const EXEC_VSOCK_PORT: u32 = a3s_transport::ports::EXEC_SERVER;

/// Run the exec server, listening on vsock port 4089.
///
/// On Linux, binds to `AF_VSOCK` with `VMADDR_CID_ANY`.
/// On non-Linux platforms, this is a no-op (development stub).
pub fn run_exec_server() -> Result<(), Box<dyn std::error::Error>> {
    info!("Starting exec server on vsock port {}", EXEC_VSOCK_PORT);

    #[cfg(target_os = "linux")]
    {
        run_vsock_server()?;
    }

    #[cfg(not(target_os = "linux"))]
    {
        info!("Exec server not available on non-Linux platform (development mode)");
    }

    Ok(())
}

/// Linux vsock server implementation.
#[cfg(target_os = "linux")]
fn run_vsock_server() -> Result<(), Box<dyn std::error::Error>> {
    use nix::sys::socket::{
        accept, bind, listen, socket, AddressFamily, Backlog, SockFlag, SockType, VsockAddr,
    };
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    use tracing::error;

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

    let addr = VsockAddr::new(libc::VMADDR_CID_ANY, EXEC_VSOCK_PORT);
    bind(sock_fd.as_raw_fd(), &addr)?;
    listen(&sock_fd, Backlog::new(4)?)?;

    info!("Exec server listening on vsock port {}", EXEC_VSOCK_PORT);

    loop {
        match accept(sock_fd.as_raw_fd()) {
            Ok(client_fd) => {
                let client = unsafe { OwnedFd::from_raw_fd(client_fd) };
                if let Err(e) = handle_connection(client) {
                    warn!("Failed to handle exec connection: {}", e);
                }
            }
            Err(e) => {
                error!("Accept failed: {}", e);
                std::thread::sleep(Duration::from_millis(100));
            }
        }
    }
}

/// Handle a single connection using Frame protocol.
///
/// 1. Read a Data frame containing JSON ExecRequest
/// 2. Execute the command
/// 3. Send a Data frame containing JSON ExecOutput
#[cfg(target_os = "linux")]
fn handle_connection(fd: std::os::fd::OwnedFd) -> Result<(), Box<dyn std::error::Error>> {
    use a3s_box_core::exec::ExecRequest;
    use std::os::fd::{AsRawFd, FromRawFd};
    use tracing::debug;

    let raw_fd = fd.as_raw_fd();
    let mut stream = unsafe { std::fs::File::from_raw_fd(raw_fd) };

    // Read request frame
    let (frame_type, payload) = match read_frame(&mut stream)? {
        Some(f) => f,
        None => {
            std::mem::forget(fd);
            return Ok(());
        }
    };

    if frame_type != FrameType::Data as u8 {
        // Heartbeat: respond with Heartbeat frame (health check)
        if frame_type == FrameType::Heartbeat as u8 {
            write_frame(&mut stream, FrameType::Heartbeat as u8, &payload)?;
            std::mem::forget(fd);
            return Ok(());
        }
        send_error_frame(&mut stream, "Expected Data frame")?;
        std::mem::forget(fd);
        return Ok(());
    }

    debug!("Exec request received ({} bytes)", payload.len());

    // Parse ExecRequest from JSON payload
    let exec_req: ExecRequest = match serde_json::from_slice(&payload) {
        Ok(req) => req,
        Err(e) => {
            send_error_frame(&mut stream, &format!("Invalid JSON: {}", e))?;
            std::mem::forget(fd);
            return Ok(());
        }
    };

    // Execute the command
    let output = execute_command(
        &exec_req.cmd,
        exec_req.timeout_ns,
        &exec_req.env,
        exec_req.working_dir.as_deref(),
        exec_req.stdin.as_deref(),
        exec_req.user.as_deref(),
    );

    // Send response as Data frame with JSON payload
    let response_payload = serde_json::to_vec(&output)?;
    write_frame(&mut stream, FrameType::Data as u8, &response_payload)?;

    std::mem::forget(fd);
    Ok(())
}

/// Write a frame: [type:u8][length:u32 BE][payload].
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn write_frame(w: &mut impl Write, frame_type: u8, payload: &[u8]) -> std::io::Result<()> {
    let len = payload.len() as u32;
    w.write_all(&[frame_type])?;
    w.write_all(&len.to_be_bytes())?;
    w.write_all(payload)?;
    w.flush()
}

/// Read a frame: [type:u8][length:u32 BE][payload]. Returns None on EOF.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn read_frame(r: &mut impl Read) -> std::io::Result<Option<(u8, Vec<u8>)>> {
    let mut header = [0u8; 5];
    match r.read_exact(&mut header) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }

    let frame_type = header[0];
    let len = u32::from_be_bytes([header[1], header[2], header[3], header[4]]) as usize;

    if len > 16 * 1024 * 1024 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("Frame too large: {} bytes", len),
        ));
    }

    let mut payload = vec![0u8; len];
    if len > 0 {
        r.read_exact(&mut payload)?;
    }

    Ok(Some((frame_type, payload)))
}

/// Send an Error frame with a message.
#[cfg(target_os = "linux")]
fn send_error_frame(w: &mut impl Write, message: &str) -> std::io::Result<()> {
    write_frame(w, FrameType::Error as u8, message.as_bytes())
}

/// Execute a command with timeout, environment variables, working directory, optional stdin, and optional user.
///
/// When `user` is specified, the command is wrapped with `su -s /bin/sh <user> -c <cmd>`
/// to run as the given user inside the guest VM.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn execute_command(
    cmd: &[String],
    timeout_ns: u64,
    env: &[String],
    working_dir: Option<&str>,
    stdin_data: Option<&[u8]>,
    user: Option<&str>,
) -> ExecOutput {
    if cmd.is_empty() {
        return ExecOutput {
            stdout: vec![],
            stderr: b"Empty command".to_vec(),
            exit_code: 1,
        };
    }

    let timeout_ns = if timeout_ns == 0 {
        DEFAULT_EXEC_TIMEOUT_NS
    } else {
        timeout_ns
    };
    let timeout = Duration::from_nanos(timeout_ns);

    // If a user is specified, wrap the command with `su`
    let (program, args) = if let Some(user) = user {
        let shell_cmd = cmd
            .iter()
            .map(|a| shell_escape(a))
            .collect::<Vec<_>>()
            .join(" ");
        (
            "su".to_string(),
            vec![
                "-s".to_string(),
                "/bin/sh".to_string(),
                user.to_string(),
                "-c".to_string(),
                shell_cmd,
            ],
        )
    } else {
        (cmd[0].clone(), cmd[1..].to_vec())
    };

    let mut command = std::process::Command::new(&program);
    command
        .args(&args)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());

    if stdin_data.is_some() {
        command.stdin(std::process::Stdio::piped());
    }

    for entry in env {
        if let Some((key, value)) = entry.split_once('=') {
            command.env(key, value);
        }
    }

    if let Some(dir) = working_dir {
        command.current_dir(dir);
    }

    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(e) => {
            return ExecOutput {
                stdout: vec![],
                stderr: format!("Failed to spawn command '{}': {}", cmd[0], e).into_bytes(),
                exit_code: 127,
            };
        }
    };

    if let Some(data) = stdin_data {
        if let Some(mut stdin_pipe) = child.stdin.take() {
            use std::io::Write;
            let _ = stdin_pipe.write_all(data);
        }
    }

    // Wait with timeout using a polling loop
    let start = std::time::Instant::now();
    let poll_interval = Duration::from_millis(50);

    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let mut stdout = Vec::new();
                let mut stderr = Vec::new();
                if let Some(ref mut out) = child.stdout {
                    let _ = out.read_to_end(&mut stdout);
                }
                if let Some(ref mut err) = child.stderr {
                    let _ = err.read_to_end(&mut stderr);
                }

                return ExecOutput {
                    stdout: truncate_output(stdout),
                    stderr: truncate_output(stderr),
                    exit_code: status.code().unwrap_or(1),
                };
            }
            Ok(None) => {
                if start.elapsed() >= timeout {
                    warn!("Exec command timed out after {:?}, killing", timeout);
                    let _ = child.kill();
                    let _ = child.wait();

                    let mut stdout = Vec::new();
                    let mut stderr = Vec::new();
                    if let Some(ref mut out) = child.stdout {
                        let _ = out.read_to_end(&mut stdout);
                    }
                    if let Some(ref mut err) = child.stderr {
                        let _ = err.read_to_end(&mut stderr);
                    }

                    stderr.extend_from_slice(b"\nProcess killed: timeout exceeded");

                    return ExecOutput {
                        stdout: truncate_output(stdout),
                        stderr: truncate_output(stderr),
                        exit_code: 137,
                    };
                }
                std::thread::sleep(poll_interval);
            }
            Err(e) => {
                return ExecOutput {
                    stdout: vec![],
                    stderr: format!("Failed to wait for command: {}", e).into_bytes(),
                    exit_code: 1,
                };
            }
        }
    }
}

/// Truncate output to MAX_OUTPUT_BYTES if it exceeds the limit.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn truncate_output(mut data: Vec<u8>) -> Vec<u8> {
    if data.len() > MAX_OUTPUT_BYTES {
        data.truncate(MAX_OUTPUT_BYTES);
    }
    data
}

/// Minimal shell escaping for a single argument.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
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
    fn test_truncate_output_within_limit() {
        let data = vec![0u8; 100];
        let result = truncate_output(data.clone());
        assert_eq!(result.len(), 100);
    }

    #[test]
    fn test_truncate_output_exceeds_limit() {
        let data = vec![0u8; MAX_OUTPUT_BYTES + 1000];
        let result = truncate_output(data);
        assert_eq!(result.len(), MAX_OUTPUT_BYTES);
    }

    #[test]
    fn test_truncate_output_at_limit() {
        let data = vec![0u8; MAX_OUTPUT_BYTES];
        let result = truncate_output(data);
        assert_eq!(result.len(), MAX_OUTPUT_BYTES);
    }

    #[test]
    fn test_truncate_output_empty() {
        let data = vec![];
        let result = truncate_output(data);
        assert!(result.is_empty());
    }

    #[test]
    fn test_execute_command_echo() {
        let output = execute_command(
            &["echo".to_string(), "hello".to_string()],
            0,
            &[],
            None,
            None,
            None,
        );
        assert_eq!(output.exit_code, 0);
        assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "hello");
        assert!(output.stderr.is_empty());
    }

    #[test]
    fn test_execute_command_nonexistent() {
        let output = execute_command(
            &["this_command_does_not_exist_a3s_test".to_string()],
            0,
            &[],
            None,
            None,
            None,
        );
        assert_ne!(output.exit_code, 0);
        assert!(!output.stderr.is_empty());
    }

    #[test]
    fn test_execute_command_empty() {
        let output = execute_command(&[], 0, &[], None, None, None);
        assert_eq!(output.exit_code, 1);
        assert_eq!(output.stderr, b"Empty command");
    }

    #[test]
    fn test_execute_command_non_zero_exit() {
        let output = execute_command(
            &["sh".to_string(), "-c".to_string(), "exit 42".to_string()],
            0,
            &[],
            None,
            None,
            None,
        );
        assert_eq!(output.exit_code, 42);
    }

    #[test]
    fn test_execute_command_with_env() {
        let output = execute_command(
            &[
                "sh".to_string(),
                "-c".to_string(),
                "echo $TEST_VAR".to_string(),
            ],
            0,
            &["TEST_VAR=hello_from_env".to_string()],
            None,
            None,
            None,
        );
        assert_eq!(output.exit_code, 0);
        assert_eq!(
            String::from_utf8_lossy(&output.stdout).trim(),
            "hello_from_env"
        );
    }

    #[test]
    fn test_execute_command_with_working_dir() {
        let output = execute_command(&["pwd".to_string()], 0, &[], Some("/tmp"), None, None);
        assert_eq!(output.exit_code, 0);
        let pwd = String::from_utf8_lossy(&output.stdout).trim().to_string();
        assert!(pwd == "/tmp" || pwd == "/private/tmp");
    }

    #[test]
    fn test_exec_vsock_port_constant() {
        assert_eq!(EXEC_VSOCK_PORT, 4089);
    }

    #[test]
    fn test_execute_command_with_stdin() {
        let output = execute_command(
            &["cat".to_string()],
            0,
            &[],
            None,
            Some(b"hello from stdin"),
            None,
        );
        assert_eq!(output.exit_code, 0);
        assert_eq!(String::from_utf8_lossy(&output.stdout), "hello from stdin");
    }

    #[test]
    fn test_shell_escape_simple() {
        assert_eq!(shell_escape("hello"), "hello");
        assert_eq!(shell_escape("/usr/bin/ls"), "/usr/bin/ls");
        assert_eq!(shell_escape("file.txt"), "file.txt");
    }

    #[test]
    fn test_shell_escape_special_chars() {
        assert_eq!(shell_escape("hello world"), "'hello world'");
        assert_eq!(shell_escape("it's"), "'it'\\''s'");
    }

    #[test]
    fn test_frame_roundtrip() {
        // Write a Data frame and read it back
        let mut buf = Vec::new();
        let payload = b"test payload";
        write_frame(&mut buf, FrameType::Data as u8, payload).unwrap();

        let mut cursor = std::io::Cursor::new(buf);
        let (ft, data) = read_frame(&mut cursor).unwrap().unwrap();
        assert_eq!(ft, FrameType::Data as u8);
        assert_eq!(data, payload);
    }

    #[test]
    fn test_frame_read_eof() {
        let mut cursor = std::io::Cursor::new(Vec::<u8>::new());
        let result = read_frame(&mut cursor).unwrap();
        assert!(result.is_none());
    }
}
