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
use crate::resolve;
#[cfg(not(windows))]
use crate::state::StateFile;

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

    /// Run the command as a specific user (e.g., "root", "1000:1000")
    #[arg(short = 'u', long)]
    pub user: Option<String>,

    /// Run command in detached mode (background)
    #[arg(short = 'd', long)]
    pub detach: bool,

    /// Command and arguments to execute
    #[arg(last = true, required = true)]
    pub cmd: Vec<String>,
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
    use a3s_box_core::exec::{ExecRequest, DEFAULT_EXEC_TIMEOUT_NS};
    use a3s_box_runtime::ExecClient;

    let state = StateFile::load_default()?;
    let record = resolve::resolve(&state, &args.r#box)?;

    if record.status != "running" {
        return Err(format!("Box {} is not running", record.name).into());
    }

    // Validate flag combinations
    if args.tty && args.detach {
        return Err("Cannot use -t (tty) with -d (detach)".into());
    }
    if args.interactive && args.detach {
        return Err("Cannot use -i (interactive) with -d (detach)".into());
    }

    // If -t is specified, use interactive PTY mode
    if args.tty {
        return execute_pty(args, record).await;
    }

    // If -d is specified, use detached mode
    if args.detach {
        return execute_detached(args, record).await;
    }

    // Non-interactive mode (original behavior)
    let exec_socket_path = if !record.exec_socket_path.as_os_str().is_empty() {
        record.exec_socket_path.clone()
    } else {
        record.box_dir.join("sockets").join("exec.sock")
    };

    if !exec_socket_path.exists() {
        return Err(format!(
            "Exec socket not found for box {} at {}",
            record.name,
            exec_socket_path.display()
        )
        .into());
    }

    let client = ExecClient::connect(&exec_socket_path).await?;

    let timeout_ns = if args.timeout == 0 {
        DEFAULT_EXEC_TIMEOUT_NS
    } else {
        args.timeout * 1_000_000_000
    };

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
        stdin: stdin_data,
        user: args.user,
        streaming: false,
    };

    let output = client.exec_command(&request).await?;

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

/// Execute a command in detached mode (background).
///
/// The command runs in the background and the CLI returns immediately.
/// Output is not captured.
#[cfg(not(windows))]
async fn execute_detached(
    args: ExecArgs,
    record: &crate::state::BoxRecord,
) -> Result<(), Box<dyn std::error::Error>> {
    use a3s_box_core::exec::ExecRequest;
    use a3s_box_runtime::ExecClient;

    let exec_socket_path = if !record.exec_socket_path.as_os_str().is_empty() {
        record.exec_socket_path.clone()
    } else {
        record.box_dir.join("sockets").join("exec.sock")
    };

    if !exec_socket_path.exists() {
        return Err(format!(
            "Exec socket not found for box {} at {}",
            record.name,
            exec_socket_path.display()
        )
        .into());
    }

    let client = ExecClient::connect(&exec_socket_path).await?;

    // For detached mode, use a very long timeout (24 hours)
    let timeout_ns = 24 * 60 * 60 * 1_000_000_000u64;

    let request = ExecRequest {
        cmd: args.cmd.clone(),
        timeout_ns,
        env: args.envs,
        working_dir: args.workdir,
        stdin: None, // No stdin in detached mode
        user: args.user,
        streaming: false,
    };

    // Send the request but don't wait for response
    // We spawn a background task to handle the exec
    tokio::spawn(async move {
        let _ = client.exec_command(&request).await;
    });

    println!("Command started in background: {}", args.cmd.join(" "));
    Ok(())
}

/// Execute a command with an interactive PTY session.
#[cfg(not(windows))]
async fn execute_pty(
    args: ExecArgs,
    record: &crate::state::BoxRecord,
) -> Result<(), Box<dyn std::error::Error>> {
    use crate::terminal;
    use a3s_box_core::pty::PtyRequest;
    use a3s_box_runtime::PtyClient;

    let pty_socket_path = crate::socket_paths::pty(record);
    if !pty_socket_path.exists() {
        return Err(format!(
            "PTY socket not found for box {} at {} (guest may not support interactive mode)",
            record.name,
            pty_socket_path.display()
        )
        .into());
    }

    // Get terminal size
    let (cols, rows) = terminal::size().unwrap_or((80, 24));

    // Connect to PTY server
    let mut client = PtyClient::connect(&pty_socket_path).await?;

    // Send PTY request
    let request = PtyRequest {
        cmd: args.cmd,
        env: args.envs,
        working_dir: args.workdir,
        user: args.user,
        cols,
        rows,
    };
    client.send_request(&request).await?;

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
    // indefinitely. Use a dedicated blocking thread via spawn_blocking +
    // an mpsc channel to relay stdin bytes into the async writer task.
    let writer_task = tokio::spawn(async move {
        let (tx, mut rx) = tokio::sync::mpsc::channel::<Vec<u8>>(64);

        tokio::task::spawn_blocking(move || {
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
