//! `a3s-box exec` command — Execute a command in a running box.
//!
//! Connects to the exec server inside the guest VM via the exec Unix socket
//! and runs the specified command, printing stdout/stderr and exiting with
//! the command's exit code.
//!
//! When `-t` (tty) is specified, allocates a PTY in the guest for interactive
//! terminal sessions (e.g., `a3s-box exec -it mybox /bin/sh`).

use a3s_box_core::exec::{ExecRequest, DEFAULT_EXEC_TIMEOUT_NS};
use a3s_box_runtime::ExecClient;
use clap::Args;

use crate::resolve;
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

    /// Command and arguments to execute
    #[arg(last = true, required = true)]
    pub cmd: Vec<String>,
}

pub async fn execute(args: ExecArgs) -> Result<(), Box<dyn std::error::Error>> {
    let state = StateFile::load_default()?;
    let record = resolve::resolve(&state, &args.r#box)?;

    if record.status != "running" {
        return Err(format!("Box {} is not running", record.name).into());
    }

    // If -t is specified, use interactive PTY mode
    if args.tty {
        return execute_pty(args, record).await;
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

/// Execute a command with an interactive PTY session.
async fn execute_pty(
    args: ExecArgs,
    record: &crate::state::BoxRecord,
) -> Result<(), Box<dyn std::error::Error>> {
    use a3s_box_core::pty::PtyRequest;
    use a3s_box_runtime::PtyClient;
    use crossterm::terminal;

    // Resolve PTY socket path
    let pty_socket_path = record.box_dir.join("sockets").join("pty.sock");
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

    // Put terminal into raw mode
    terminal::enable_raw_mode()?;

    // Split the PTY client stream for concurrent read/write
    let (read_half, write_half) = client.into_split();

    let exit_code = run_pty_session(read_half, write_half).await;

    // Restore terminal
    terminal::disable_raw_mode()?;

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
pub(crate) async fn run_pty_session(
    mut reader: a3s_transport::FrameReader<tokio::io::ReadHalf<tokio::net::UnixStream>>,
    mut writer: a3s_transport::FrameWriter<tokio::io::WriteHalf<tokio::net::UnixStream>>,
) -> i32 {
    use a3s_box_core::pty::{FRAME_PTY_DATA, FRAME_PTY_ERROR, FRAME_PTY_EXIT};
    use tokio::io::AsyncReadExt;

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

    // Task 2: Read from stdin + handle SIGWINCH → send frames to guest
    let writer_task = tokio::spawn(async move {
        let mut stdin = tokio::io::stdin();
        let mut buf = [0u8; 4096];

        let mut sigwinch =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::window_change()).ok();

        loop {
            tokio::select! {
                result = stdin.read(&mut buf) => {
                    match result {
                        Ok(0) => break,
                        Ok(n) => {
                            if writer.write_data(&buf[..n]).await.is_err() {
                                break;
                            }
                        }
                        Err(_) => break,
                    }
                },
                _ = async {
                    match sigwinch {
                        Some(ref mut sig) => { sig.recv().await; },
                        None => std::future::pending().await,
                    }
                } => {
                    if let Ok((cols, rows)) = crossterm::terminal::size() {
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
