//! `a3s-box exec` command — Execute a command in a running box.
//!
//! Connects to the exec server inside the guest VM via the exec Unix socket
//! and runs the specified command, printing stdout/stderr and exiting with
//! the command's exit code.
//!
//! When `-t` (tty) is specified, allocates a PTY in the guest for interactive
//! terminal sessions (e.g., `a3s-box exec -it mybox /bin/sh`).

use clap::Args;

#[cfg(not(windows))]
use super::common;
#[cfg(not(windows))]
use crate::resolve;
#[cfg(not(windows))]
use crate::state::StateFile;

#[cfg(not(windows))]
const NANOS_PER_SECOND: u64 = 1_000_000_000;

#[derive(Args)]
pub struct ExecArgs {
    /// Box name or ID
    pub r#box: String,

    /// Timeout in seconds (default: 5)
    #[arg(long, default_value = "5")]
    pub timeout: u64,

    /// Set environment variables (KEY=VALUE), can be repeated
    #[arg(short, long = "env")]
    pub envs: Vec<String>,

    /// Working directory inside the box
    #[arg(short, long)]
    pub workdir: Option<String>,

    /// Keep STDIN open (pipe stdin to the command)
    #[arg(short = 'i', long = "interactive")]
    pub interactive: bool,

    /// Allocate a pseudo-TTY
    #[arg(short = 't', long = "tty")]
    pub tty: bool,

    /// Run the command as a specific user (supported: root, UID, UID:GID)
    #[arg(short = 'u', long)]
    pub user: Option<String>,

    /// Command and arguments to execute
    #[arg(last = true, required = true)]
    pub cmd: Vec<String>,
}

#[cfg(not(windows))]
pub(crate) async fn connect_pty_with_retry(
    socket_path: &std::path::Path,
    timeout: std::time::Duration,
) -> Result<a3s_box_runtime::PtyClient, Box<dyn std::error::Error>> {
    let deadline = std::time::Instant::now() + timeout;

    loop {
        match a3s_box_runtime::PtyClient::connect(socket_path).await {
            Ok(client) => return Ok(client),
            Err(error) => {
                let last_error = error.to_string();
                if std::time::Instant::now() >= deadline {
                    return Err(format!(
                        "Failed to connect to PTY server at {} after {:?}: {}",
                        socket_path.display(),
                        timeout,
                        last_error
                    )
                    .into());
                }
            }
        }

        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
}

#[cfg(windows)]
pub async fn execute(_args: ExecArgs) -> Result<(), Box<dyn std::error::Error>> {
    Err(crate::platform::unsupported_command(
        "exec",
        "guest exec channel support",
    ))
}

#[cfg(not(windows))]
pub async fn execute(args: ExecArgs) -> Result<(), Box<dyn std::error::Error>> {
    use a3s_box_core::exec::ExecRequest;
    use a3s_box_runtime::ExecClient;

    let user = common::normalize_user_option(args.user.as_deref())
        .map_err(|e| -> Box<dyn std::error::Error> { e.into() })?;
    common::validate_workdir_option(args.workdir.as_deref())
        .map_err(|e| -> Box<dyn std::error::Error> { e.into() })?;

    let state = StateFile::load_default()?;
    let record = resolve::resolve(&state, &args.r#box)?;
    crate::socket_paths::require_running(record, "exec")
        .map_err(|e| -> Box<dyn std::error::Error> { e.into() })?;

    // If -t is specified, use interactive PTY mode
    if args.tty {
        return execute_pty(args, record, user).await;
    }

    // Non-interactive mode (original behavior)
    let exec_socket_path = crate::socket_paths::require_runtime_socket(
        record,
        crate::socket_paths::RuntimeSocket::Exec,
    )
    .map_err(|e| -> Box<dyn std::error::Error> { e.into() })?;

    let client = ExecClient::connect(&exec_socket_path).await?;

    let timeout_ns = timeout_secs_to_ns(args.timeout);

    // Read stdin if interactive mode
    let stdin_data = if args.interactive {
        use std::io::Read;
        let mut buf = Vec::new();
        std::io::stdin().read_to_end(&mut buf)?;
        if buf.is_empty() {
            None
        } else {
            Some(buf)
        }
    } else {
        None
    };

    let request = ExecRequest {
        cmd: args.cmd,
        timeout_ns,
        env: args.envs,
        working_dir: args.workdir,
        rootfs: None,
        stdin: stdin_data,
        stdin_streaming: false,
        user,
        streaming: false,
    };

    let output = client.exec_command(&request).await?;
    // Record that an exec happened (best-effort) before the exit-code branch
    // below may std::process::exit. The container command's own exit code is
    // separate from whether the exec was delivered.
    crate::audit::record(
        a3s_box_core::audit::AuditAction::ExecCommand,
        a3s_box_core::audit::AuditOutcome::Success,
        &record.id,
        &format!("exec command in box {}", record.name),
    );

    if !output.stdout.is_empty() {
        let stdout = String::from_utf8_lossy(&output.stdout);
        print!("{}", stdout);
    }

    if !output.stderr.is_empty() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        eprint!("{}", stderr);
    }

    if output.exit_code != 0 {
        std::process::exit(output.exit_code);
    }

    Ok(())
}

#[cfg(not(windows))]
fn timeout_secs_to_ns(timeout_secs: u64) -> u64 {
    if timeout_secs == 0 {
        a3s_box_core::exec::DEFAULT_EXEC_TIMEOUT_NS
    } else {
        // Cap absurd values at u64::MAX ns instead of overflowing into a tiny
        // timeout in release builds.
        timeout_secs.saturating_mul(NANOS_PER_SECOND)
    }
}

/// Execute a command with an interactive PTY session.
#[cfg(not(windows))]
async fn execute_pty(
    args: ExecArgs,
    record: &crate::state::BoxRecord,
    user: Option<String>,
) -> Result<(), Box<dyn std::error::Error>> {
    use crate::terminal;
    use a3s_box_core::pty::PtyRequest;

    let pty_socket_path = crate::socket_paths::require_runtime_socket(
        record,
        crate::socket_paths::RuntimeSocket::Pty,
    )
    .map_err(|e| -> Box<dyn std::error::Error> { e.into() })?;

    // Get terminal size
    let (cols, rows) = terminal::size().unwrap_or((80, 24));

    // Connect to PTY server
    let mut client =
        connect_pty_with_retry(&pty_socket_path, std::time::Duration::from_secs(10)).await?;

    // Send PTY request
    let request = PtyRequest {
        cmd: args.cmd,
        env: args.envs,
        working_dir: args.workdir,
        rootfs: None,
        user,
        cols,
        rows,
    };
    client.send_request(&request).await?;
    // Record the interactive (pty) exec once the request is delivered — opening
    // a shell in a box is a key forensic event.
    crate::audit::record(
        a3s_box_core::audit::AuditAction::ExecCommand,
        a3s_box_core::audit::AuditOutcome::Success,
        &record.id,
        &format!("exec (pty) in box {}", record.name),
    );

    // Split the PTY client stream for concurrent read/write
    let (read_half, write_half) = client.into_split();

    let exit_code = {
        let _raw_mode = terminal::raw_mode()?;
        run_pty_session(read_half, write_half).await
    };

    if exit_code != 0 {
        std::process::exit(exit_code);
    }

    Ok(())
}

/// Run the bidirectional PTY relay:
/// - stdin → PtyData frames to guest
/// - PtyData frames from guest → stdout
/// - SIGWINCH → PtyResize frames
///
/// Returns the process exit code.
#[cfg(not(windows))]
pub(crate) async fn run_pty_session(
    mut reader: a3s_transport::FrameReader<tokio::io::ReadHalf<tokio::net::UnixStream>>,
    mut writer: a3s_transport::FrameWriter<tokio::io::WriteHalf<tokio::net::UnixStream>>,
) -> i32 {
    use a3s_box_core::pty::{FRAME_PTY_DATA, FRAME_PTY_ERROR, FRAME_PTY_EXIT};

    // Task 1: Read from guest PTY → write to stdout
    let reader_task = tokio::spawn(async move {
        let mut stdout = tokio::io::stdout();
        loop {
            match reader.read_frame().await {
                Ok(Some(frame)) => {
                    let frame_type = frame.frame_type as u8;
                    match frame_type {
                        FRAME_PTY_DATA => {
                            use tokio::io::AsyncWriteExt;
                            if stdout.write_all(&frame.payload).await.is_err() {
                                return -1i32;
                            }
                            let _ = stdout.flush().await;
                        }
                        FRAME_PTY_EXIT => {
                            if let Ok(exit) =
                                serde_json::from_slice::<a3s_box_core::pty::PtyExit>(&frame.payload)
                            {
                                return exit.exit_code;
                            }
                            return 1;
                        }
                        FRAME_PTY_ERROR => {
                            let msg = String::from_utf8_lossy(&frame.payload);
                            eprintln!("\r\nPTY error: {}", msg);
                            return 1;
                        }
                        _ => {} // Ignore unknown frames
                    }
                }
                Ok(None) => return -1, // EOF
                Err(_) => return -1,
            }
        }
    });

    // Task 2: Read from stdin + handle SIGWINCH → send frames to guest.
    //
    // tokio::io::stdin() uses kqueue on macOS which does not generate
    // readiness events for TTY fds in raw mode, causing reads to block
    // indefinitely. Use a detached OS thread instead of spawn_blocking so a
    // blocked stdin read cannot keep the Tokio runtime alive after PTY exit.
    let writer_task = tokio::spawn(async move {
        let (tx, mut rx) = tokio::sync::mpsc::channel::<Vec<u8>>(64);

        std::thread::spawn(move || {
            use std::io::Read;
            let mut stdin = std::io::stdin();
            let mut buf = [0u8; 4096];
            loop {
                match stdin.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        if tx.blocking_send(buf[..n].to_vec()).is_err() {
                            break;
                        }
                    }
                }
            }
        });

        let mut sigwinch =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::window_change()).ok();

        loop {
            tokio::select! {
                data = rx.recv() => {
                    match data {
                        Some(bytes) => {
                            // Send PTY_DATA frame (0x02), not generic Data frame (0x01)
                            let ft = a3s_transport::FrameType::try_from(a3s_box_core::pty::FRAME_PTY_DATA)
                                .unwrap_or(a3s_transport::FrameType::Data);
                            let frame = a3s_transport::Frame { frame_type: ft, payload: bytes };
                            if writer.write_frame(&frame).await.is_err() {
                                break;
                            }
                        }
                        None => break,
                    }
                },
                _ = async {
                    match sigwinch {
                        Some(ref mut sig) => { sig.recv().await; },
                        None => std::future::pending().await,
                    }
                } => {
                    if let Ok((cols, rows)) = crate::terminal::size() {
                        let resize = a3s_box_core::pty::PtyResize { cols, rows };
                        if let Ok(payload) = serde_json::to_vec(&resize) {
                            let ft = a3s_transport::FrameType::try_from(a3s_box_core::pty::FRAME_PTY_RESIZE)
                                .unwrap_or(a3s_transport::FrameType::Control);
                            let frame = a3s_transport::Frame { frame_type: ft, payload };
                            let _ = writer.write_frame(&frame).await;
                        }
                    }
                },
            }
        }
    });

    // Wait for the reader to finish (it returns the exit code)
    let exit_code = reader_task.await.unwrap_or(1);

    // Abort the writer task
    writer_task.abort();

    exit_code
}

#[cfg(all(test, not(windows)))]
mod tests {
    use super::*;

    #[test]
    fn timeout_secs_to_ns_uses_default_for_zero() {
        assert_eq!(
            timeout_secs_to_ns(0),
            a3s_box_core::exec::DEFAULT_EXEC_TIMEOUT_NS
        );
    }

    #[test]
    fn timeout_secs_to_ns_converts_seconds_to_nanoseconds() {
        assert_eq!(timeout_secs_to_ns(7), 7_000_000_000);
    }

    #[test]
    fn timeout_secs_to_ns_saturates_large_values() {
        assert_eq!(timeout_secs_to_ns(u64::MAX), u64::MAX);
    }
}
