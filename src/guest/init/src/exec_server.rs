//! Guest exec server for executing commands inside the VM.
//!
//! Listens on vsock port 4089 and accepts Frame-based requests.
//! Each connection: read a Data frame (JSON ExecRequest), execute,
//! then send either a one-shot `ExecOutput` or streaming chunk/exit frames.

use std::io::Read;
use std::io::Write;
use std::sync::mpsc;
use std::time::Duration;

use a3s_box_core::exec::{
    ExecChunk, ExecExit, ExecOutput, StreamType, DEFAULT_EXEC_TIMEOUT_NS, EXEC_VSOCK_PORT,
    MAX_OUTPUT_BYTES,
};
use a3s_transport::FrameType;
use tracing::{info, warn};

use crate::user::{parse_process_user, ProcessUser};

/// Maximum payload bytes per streamed exec chunk.
const STREAM_CHUNK_BYTES: usize = 16 * 1024;
const EXEC_CONTROL_CANCEL: &[u8] = b"cancel";
const EXEC_CONTROL_STDIN_CLOSE: &[u8] = b"stdin-close";

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
                std::thread::spawn(move || {
                    if let Err(e) = handle_connection(client) {
                        warn!("Failed to handle exec connection: {}", e);
                    }
                });
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
/// 3. Send either a one-shot ExecOutput frame or streaming exec frames
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

    if exec_req.streaming {
        let input_rx = spawn_exec_input_monitor(&stream)?;
        execute_command_streaming(
            ExecCommandSpec {
                cmd: &exec_req.cmd,
                timeout_ns: exec_req.timeout_ns,
                env: &exec_req.env,
                working_dir: exec_req.working_dir.as_deref(),
                rootfs: exec_req.rootfs.as_deref(),
                stdin_data: exec_req.stdin.as_deref(),
                stdin_streaming: exec_req.stdin_streaming,
                user: exec_req.user.as_deref(),
            },
            Some(input_rx),
            &mut stream,
        )?;
    } else {
        // Execute the command
        let output = execute_command(
            &exec_req.cmd,
            exec_req.timeout_ns,
            &exec_req.env,
            exec_req.working_dir.as_deref(),
            exec_req.rootfs.as_deref(),
            exec_req.stdin.as_deref(),
            exec_req.user.as_deref(),
        );

        // Send response as Data frame with JSON payload
        let response_payload = serde_json::to_vec(&output)?;
        write_frame(&mut stream, FrameType::Data as u8, &response_payload)?;
    }

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

#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
enum ExecInputEvent {
    Stdin(Vec<u8>),
    StdinClose,
    Cancel,
}

#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn spawn_exec_input_monitor(
    stream: &std::fs::File,
) -> std::io::Result<mpsc::Receiver<ExecInputEvent>> {
    let mut reader = stream.try_clone()?;
    let (tx, rx) = mpsc::channel();

    std::thread::spawn(move || loop {
        match read_frame(&mut reader) {
            Ok(Some((frame_type, payload)))
                if frame_type == FrameType::Control as u8 && payload == EXEC_CONTROL_CANCEL =>
            {
                let _ = tx.send(ExecInputEvent::Cancel);
                break;
            }
            Ok(Some((frame_type, payload)))
                if frame_type == FrameType::Control as u8
                    && payload == EXEC_CONTROL_STDIN_CLOSE =>
            {
                if tx.send(ExecInputEvent::StdinClose).is_err() {
                    break;
                }
            }
            Ok(Some((frame_type, payload))) if frame_type == FrameType::Data as u8 => {
                if tx.send(ExecInputEvent::Stdin(payload)).is_err() {
                    break;
                }
            }
            Ok(Some((frame_type, _))) if frame_type == FrameType::Close as u8 => {
                let _ = tx.send(ExecInputEvent::Cancel);
                break;
            }
            Ok(Some(_)) => {}
            Ok(None) => break,
            Err(e) => {
                warn!("Failed to read exec control frame: {}", e);
                break;
            }
        }
    });

    Ok(rx)
}

/// Serialize a completed command output as streaming exec frames.
///
/// This keeps validation and spawn errors compatible with
/// `ExecClient::exec_stream()` even when no child process is running.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn write_exec_stream_response(
    w: &mut impl Write,
    output: &ExecOutput,
) -> Result<(), Box<dyn std::error::Error>> {
    write_exec_stream_chunks(w, StreamType::Stdout, &output.stdout)?;
    write_exec_stream_chunks(w, StreamType::Stderr, &output.stderr)?;
    write_exec_exit(w, output.exit_code)
}

#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn write_exec_stream_chunks(
    w: &mut impl Write,
    stream: StreamType,
    data: &[u8],
) -> Result<(), Box<dyn std::error::Error>> {
    for chunk in data.chunks(STREAM_CHUNK_BYTES) {
        write_exec_stream_chunk(w, stream, chunk)?;
    }

    Ok(())
}

#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn write_exec_stream_chunk(
    w: &mut impl Write,
    stream: StreamType,
    data: &[u8],
) -> Result<(), Box<dyn std::error::Error>> {
    if data.is_empty() {
        return Ok(());
    }

    let payload = serde_json::to_vec(&ExecChunk {
        stream,
        data: data.to_vec(),
    })?;
    write_frame(w, FrameType::Data as u8, &payload)?;
    Ok(())
}

#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn write_exec_exit(w: &mut impl Write, exit_code: i32) -> Result<(), Box<dyn std::error::Error>> {
    let exit = ExecExit { exit_code };
    let payload = serde_json::to_vec(&exit)?;
    write_frame(w, FrameType::Control as u8, &payload)?;
    Ok(())
}

#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
#[derive(Clone, Copy)]
struct ExecCommandSpec<'a> {
    cmd: &'a [String],
    timeout_ns: u64,
    env: &'a [String],
    working_dir: Option<&'a str>,
    rootfs: Option<&'a str>,
    stdin_data: Option<&'a [u8]>,
    stdin_streaming: bool,
    user: Option<&'a str>,
}

/// Execute a command with timeout, environment variables, working directory, optional stdin, and optional user.
///
/// When `user` is specified, guest-init applies the numeric UID/GID in the
/// child process before exec. Named users are rejected until passwd lookup is
/// implemented.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn build_command(
    spec: ExecCommandSpec<'_>,
) -> Result<(std::process::Command, Duration), ExecOutput> {
    if spec.cmd.is_empty() {
        return Err(ExecOutput {
            stdout: vec![],
            stderr: b"Empty command".to_vec(),
            exit_code: 1,
        });
    }

    let timeout_ns = if spec.timeout_ns == 0 {
        DEFAULT_EXEC_TIMEOUT_NS
    } else {
        spec.timeout_ns
    };
    let timeout = Duration::from_nanos(timeout_ns);
    let workdir = spec.working_dir.unwrap_or("/");
    // Resolve a named user (CRI RunAsUserName) against the container's
    // /etc/passwd before numeric parsing; falls through to spec.user when the
    // value is already numeric/root or cannot be resolved.
    let resolved_user = spec
        .user
        .zip(spec.rootfs)
        .and_then(|(user, rootfs)| crate::user::resolve_named_user(user, rootfs));
    let process_user = match parse_process_user(resolved_user.as_deref().or(spec.user)) {
        Ok(process_user) => process_user,
        Err(error) => {
            return Err(ExecOutput {
                stdout: vec![],
                stderr: error.into_bytes(),
                exit_code: 1,
            });
        }
    };

    if let Some(rootfs) = spec.rootfs {
        if rootfs.is_empty()
            || !rootfs.starts_with('/')
            || rootfs.contains('\0')
            || workdir.contains('\0')
        {
            return Err(ExecOutput {
                stdout: vec![],
                stderr: format!("Invalid rootfs path: {rootfs}").into_bytes(),
                exit_code: 1,
            });
        }

        #[cfg(not(target_os = "linux"))]
        {
            return Err(ExecOutput {
                stdout: vec![],
                stderr: b"Rootfs execution requires a Linux guest".to_vec(),
                exit_code: 1,
            });
        }

        #[cfg(target_os = "linux")]
        match std::fs::metadata(rootfs) {
            Ok(metadata) if metadata.is_dir() => {
                // Containers chroot into this rootfs, so the guest-root /proc
                // and /sys are invisible to them. Mount fresh pseudo-filesystems
                // inside the rootfs (idempotent) so in-container reads of
                // /proc/self/* and /sys/class/* work like any container runtime.
                ensure_container_pseudo_filesystems(rootfs);
                // CRI MaskedPaths/ReadonlyPaths arrive as ':'-separated absolute
                // paths over A3S_SEC_MASKED_PATHS / A3S_SEC_READONLY_PATHS.
                let masked = parse_sec_path_list(spec.env, "A3S_SEC_MASKED_PATHS=");
                let readonly = parse_sec_path_list(spec.env, "A3S_SEC_READONLY_PATHS=");
                if !masked.is_empty() || !readonly.is_empty() {
                    apply_container_path_restrictions(rootfs, &masked, &readonly);
                }
            }
            Ok(_) => {
                return Err(ExecOutput {
                    stdout: vec![],
                    stderr: format!("Rootfs path is not a directory: {rootfs}").into_bytes(),
                    exit_code: 1,
                });
            }
            Err(e) => {
                return Err(ExecOutput {
                    stdout: vec![],
                    stderr: format!("Rootfs path is unavailable: {rootfs} ({e})").into_bytes(),
                    exit_code: 1,
                });
            }
        }
    }

    let program = spec.cmd[0].clone();
    let args = spec.cmd[1..].to_vec();

    let mut command = std::process::Command::new(&program);
    command
        .args(&args)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());

    if spec.stdin_data.is_some() || spec.stdin_streaming {
        command.stdin(std::process::Stdio::piped());
    }

    for entry in spec.env {
        if let Some((key, value)) = entry.split_once('=') {
            // A3S_SEC_* are runtime control vars (consumed below for setgroups /
            // masked paths / seccomp); don't leak them into the workload's env.
            if key.starts_with("A3S_SEC_") {
                continue;
            }
            command.env(key, value);
        }
    }

    // CRI SupplementalGroups arrive as A3S_SEC_SUPPLEMENTAL_GROUPS=gid,gid,...
    // and are applied (setgroups) before dropping to the target uid/gid.
    let supplemental_groups: Vec<u32> = spec
        .env
        .iter()
        .find_map(|entry| entry.strip_prefix("A3S_SEC_SUPPLEMENTAL_GROUPS="))
        .map(|csv| {
            csv.split(',')
                .filter_map(|gid| gid.trim().parse::<u32>().ok())
                .collect()
        })
        .unwrap_or_default();
    // CRI seccomp: A3S_SEC_SECCOMP=default applies the default BPF filter
    // (RuntimeDefault) in the child; unconfined/unset leave the process
    // unfiltered.
    let apply_seccomp = spec
        .env
        .iter()
        .any(|entry| entry == "A3S_SEC_SECCOMP=default");
    configure_child_process(
        &mut command,
        spec.rootfs,
        workdir,
        process_user,
        supplemental_groups,
        apply_seccomp,
    );
    if spec.rootfs.is_none() {
        if let Some(dir) = spec.working_dir {
            command.current_dir(dir);
        }
    }

    Ok((command, timeout))
}

#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn write_child_stdin(child: &mut std::process::Child, stdin_data: Option<&[u8]>, keep_open: bool) {
    if let Some(data) = stdin_data {
        if keep_open {
            if let Some(stdin_pipe) = child.stdin.as_mut() {
                let _ = stdin_pipe.write_all(data);
                let _ = stdin_pipe.flush();
            }
        } else if let Some(mut stdin_pipe) = child.stdin.take() {
            let _ = stdin_pipe.write_all(data);
        }
    }
}

#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn read_child_output(child: &mut std::process::Child) -> (Vec<u8>, Vec<u8>) {
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    if let Some(ref mut out) = child.stdout {
        let _ = out.read_to_end(&mut stdout);
    }
    if let Some(ref mut err) = child.stderr {
        let _ = err.read_to_end(&mut stderr);
    }
    (stdout, stderr)
}

#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn execute_command(
    cmd: &[String],
    timeout_ns: u64,
    env: &[String],
    working_dir: Option<&str>,
    rootfs: Option<&str>,
    stdin_data: Option<&[u8]>,
    user: Option<&str>,
) -> ExecOutput {
    let (mut command, timeout) = match build_command(ExecCommandSpec {
        cmd,
        timeout_ns,
        env,
        working_dir,
        rootfs,
        stdin_data,
        stdin_streaming: false,
        user,
    }) {
        Ok(command) => command,
        Err(output) => return output,
    };

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

    write_child_stdin(&mut child, stdin_data, false);

    // Wait with timeout using a polling loop
    let start = std::time::Instant::now();
    let poll_interval = Duration::from_millis(50);

    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let (stdout, stderr) = read_child_output(&mut child);

                return ExecOutput {
                    stdout: truncate_output(stdout),
                    stderr: truncate_output(stderr),
                    exit_code: status.code().unwrap_or(1),
                };
            }
            Ok(None) => {
                if start.elapsed() >= timeout {
                    warn!("Exec command timed out after {:?}, killing", timeout);
                    kill_child_process_group(&mut child);

                    let (stdout, mut stderr) = read_child_output(&mut child);

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

#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
enum StreamReaderEvent {
    Chunk(StreamType, Vec<u8>),
}

#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
enum StreamingStopReason {
    Timeout,
    Cancelled,
}

#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn spawn_stream_reader<R>(
    stream: StreamType,
    mut reader: R,
    sender: mpsc::Sender<StreamReaderEvent>,
) -> std::thread::JoinHandle<()>
where
    R: Read + Send + 'static,
{
    std::thread::spawn(move || {
        let mut buffer = vec![0u8; STREAM_CHUNK_BYTES];
        loop {
            match reader.read(&mut buffer) {
                Ok(0) => break,
                Ok(n) => {
                    let chunk = buffer[..n].to_vec();
                    if sender
                        .send(StreamReaderEvent::Chunk(stream, chunk))
                        .is_err()
                    {
                        break;
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(e) => {
                    warn!(stream = %stream, error = %e, "Failed to read exec output stream");
                    break;
                }
            }
        }
    })
}

#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn drain_stream_reader_events(
    receiver: &mpsc::Receiver<StreamReaderEvent>,
    writer: &mut impl Write,
) -> Result<(), Box<dyn std::error::Error>> {
    while let Ok(StreamReaderEvent::Chunk(stream, data)) = receiver.try_recv() {
        write_exec_stream_chunk(writer, stream, &data)?;
    }
    Ok(())
}

#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn wait_streaming_child(
    child: &mut std::process::Child,
    timeout: Duration,
    input_rx: Option<&mpsc::Receiver<ExecInputEvent>>,
    receiver: &mpsc::Receiver<StreamReaderEvent>,
    writer: &mut impl Write,
) -> Result<(i32, Option<StreamingStopReason>), Box<dyn std::error::Error>> {
    let start = std::time::Instant::now();
    let poll_interval = Duration::from_millis(20);

    loop {
        if drain_exec_input_events(child, input_rx) {
            warn!("Streaming exec command received stop request, killing");
            kill_child_process_group(child);
            return Ok((137, Some(StreamingStopReason::Cancelled)));
        }

        match receiver.recv_timeout(poll_interval) {
            Ok(StreamReaderEvent::Chunk(stream, data)) => {
                write_exec_stream_chunk(writer, stream, &data)?;
            }
            Err(mpsc::RecvTimeoutError::Timeout | mpsc::RecvTimeoutError::Disconnected) => {}
        }

        if drain_exec_input_events(child, input_rx) {
            warn!("Streaming exec command received stop request, killing");
            kill_child_process_group(child);
            return Ok((137, Some(StreamingStopReason::Cancelled)));
        }

        match child.try_wait() {
            Ok(Some(status)) => return Ok((status.code().unwrap_or(1), None)),
            Ok(None) => {
                if start.elapsed() >= timeout {
                    warn!(
                        "Streaming exec command timed out after {:?}, killing",
                        timeout
                    );
                    kill_child_process_group(child);
                    return Ok((137, Some(StreamingStopReason::Timeout)));
                }
            }
            Err(e) => {
                write_exec_stream_chunk(
                    writer,
                    StreamType::Stderr,
                    format!("Failed to wait for command: {e}").as_bytes(),
                )?;
                return Ok((1, None));
            }
        }
    }
}

#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn drain_exec_input_events(
    child: &mut std::process::Child,
    input_rx: Option<&mpsc::Receiver<ExecInputEvent>>,
) -> bool {
    let Some(input_rx) = input_rx else {
        return false;
    };

    loop {
        match input_rx.try_recv() {
            Ok(ExecInputEvent::Stdin(data)) => write_live_child_stdin(child, &data),
            Ok(ExecInputEvent::StdinClose) => {
                let _ = child.stdin.take();
            }
            Ok(ExecInputEvent::Cancel) => return true,
            Err(mpsc::TryRecvError::Empty | mpsc::TryRecvError::Disconnected) => return false,
        }
    }
}

#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn write_live_child_stdin(child: &mut std::process::Child, data: &[u8]) {
    let mut close_stdin = false;

    if let Some(stdin_pipe) = child.stdin.as_mut() {
        match stdin_pipe.write_all(data).and_then(|_| stdin_pipe.flush()) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => {
                close_stdin = true;
            }
            Err(e) => {
                warn!(error = %e, "Failed to write streaming exec stdin");
                close_stdin = true;
            }
        }
    }

    if close_stdin {
        let _ = child.stdin.take();
    }
}

/// Execute a command and emit stdout/stderr chunks while the process is running.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn execute_command_streaming(
    spec: ExecCommandSpec<'_>,
    input_rx: Option<mpsc::Receiver<ExecInputEvent>>,
    writer: &mut impl Write,
) -> Result<(), Box<dyn std::error::Error>> {
    let (mut command, timeout) = match build_command(spec) {
        Ok(command) => command,
        Err(output) => {
            write_exec_stream_response(writer, &output)?;
            return Ok(());
        }
    };

    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(e) => {
            let output = ExecOutput {
                stdout: vec![],
                stderr: format!("Failed to spawn command '{}': {}", spec.cmd[0], e).into_bytes(),
                exit_code: 127,
            };
            write_exec_stream_response(writer, &output)?;
            return Ok(());
        }
    };

    write_child_stdin(&mut child, spec.stdin_data, spec.stdin_streaming);

    let (sender, receiver) = mpsc::channel();
    let mut readers = Vec::new();
    if let Some(stdout) = child.stdout.take() {
        readers.push(spawn_stream_reader(
            StreamType::Stdout,
            stdout,
            sender.clone(),
        ));
    }
    if let Some(stderr) = child.stderr.take() {
        readers.push(spawn_stream_reader(
            StreamType::Stderr,
            stderr,
            sender.clone(),
        ));
    }
    drop(sender);

    let wait_result =
        wait_streaming_child(&mut child, timeout, input_rx.as_ref(), &receiver, writer);
    let (exit_code, stop_reason) = match wait_result {
        Ok(result) => result,
        Err(error) => {
            kill_child_process_group(&mut child);
            for reader in readers {
                let _ = reader.join();
            }
            return Err(error);
        }
    };

    for reader in readers {
        let _ = reader.join();
    }
    drain_stream_reader_events(&receiver, writer)?;

    match stop_reason {
        Some(StreamingStopReason::Timeout) => {
            write_exec_stream_chunk(
                writer,
                StreamType::Stderr,
                b"\nProcess killed: timeout exceeded",
            )?;
        }
        Some(StreamingStopReason::Cancelled) => {
            write_exec_stream_chunk(
                writer,
                StreamType::Stderr,
                b"\nProcess killed: stop requested",
            )?;
        }
        None => {}
    }

    write_exec_exit(writer, exit_code)
}

#[cfg(unix)]
fn configure_child_process(
    command: &mut std::process::Command,
    rootfs: Option<&str>,
    workdir: &str,
    user: Option<ProcessUser>,
    supplemental_groups: Vec<u32>,
    apply_seccomp: bool,
) {
    use std::ffi::CString;
    use std::os::unix::process::CommandExt;

    let rootfs = rootfs
        .map(|rootfs| CString::new(rootfs.as_bytes()).expect("rootfs path was pre-validated"));
    // workdir (for chdir) is only used when chrooting into a rootfs, where
    // build_command has already rejected an embedded NUL. Build the CString only
    // in that case so a workdir containing a NUL with no rootfs set cannot panic
    // this exec thread.
    let workdir = rootfs
        .as_ref()
        .map(|_| CString::new(workdir.as_bytes()).expect("working directory was pre-validated"));

    // Build the seccomp BPF filter BEFORE fork: building allocates, and
    // allocating in the post-fork child is not async-signal-safe (malloc may
    // deadlock on musl). The child only installs the prebuilt filter.
    #[cfg(target_os = "linux")]
    let seccomp_filter = apply_seccomp.then(crate::namespace::build_default_bpf_filter);
    #[cfg(not(target_os = "linux"))]
    let _ = apply_seccomp;

    unsafe {
        command.pre_exec(move || {
            if libc::setpgid(0, 0) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            if let Some(rootfs) = rootfs.as_ref() {
                if libc::chroot(rootfs.as_ptr()) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                if let Some(workdir) = workdir.as_ref() {
                    if libc::chdir(workdir.as_ptr()) != 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                }
            }
            // Apply supplemental groups while still privileged — setgroups
            // needs CAP_SETGID, which user.apply() drops via setuid below.
            if !supplemental_groups.is_empty() {
                let ret = libc::setgroups(
                    supplemental_groups.len() as _,
                    supplemental_groups.as_ptr() as *const libc::gid_t,
                );
                if ret != 0 {
                    return Err(std::io::Error::last_os_error());
                }
            }
            if let Some(user) = user {
                user.apply()?;
            }
            // Install the seccomp filter last — after chroot and the privilege
            // drop — so it is active across execve. Only the prebuilt filter is
            // installed here; no allocation happens in the post-fork child.
            #[cfg(target_os = "linux")]
            if let Some(filter) = &seccomp_filter {
                crate::namespace::install_seccomp_filter(filter)?;
            }
            Ok(())
        });
    }
}

#[cfg(not(unix))]
fn configure_child_process(
    _command: &mut std::process::Command,
    _rootfs: Option<&str>,
    _workdir: &str,
    _user: Option<ProcessUser>,
    _supplemental_groups: Vec<u32>,
    _apply_seccomp: bool,
) {
}

/// Mount fresh `proc` and `sysfs` instances inside a container `rootfs`.
///
/// Containers `chroot` into their overlay rootfs, where the guest-root `/proc`
/// and `/sys` are not visible. This mounts pseudo-filesystems at
/// `<rootfs>/proc` and `<rootfs>/sys` so in-container processes can read
/// `/proc/self/status`, `/sys/class/net`, etc. Best-effort and idempotent: an
/// existing mount (detected by a differing `st_dev`) is left untouched, and any
/// failure is logged without aborting the exec.
#[cfg(target_os = "linux")]
fn ensure_container_pseudo_filesystems(rootfs: &str) {
    use nix::mount::{mount, MsFlags};
    use std::os::unix::fs::MetadataExt;

    let Ok(root_dev) = std::fs::metadata(rootfs).map(|meta| meta.dev()) else {
        return;
    };

    for (subdir, fstype) in [("proc", "proc"), ("sys", "sysfs")] {
        let target = format!("{rootfs}/{subdir}");
        match std::fs::metadata(&target) {
            // Already a distinct mount (procfs/sysfs has its own device).
            Ok(meta) if meta.dev() != root_dev => continue,
            Ok(_) => {}
            Err(_) => {
                if let Err(e) = std::fs::create_dir_all(&target) {
                    warn!("Failed to create {target}: {e}");
                    continue;
                }
            }
        }
        if let Err(e) = mount(
            Some(fstype),
            target.as_str(),
            Some(fstype),
            MsFlags::empty(),
            None::<&str>,
        ) {
            warn!("Failed to mount {fstype} at {target}: {e}");
        }
    }
}

/// Parse a `':'`-separated absolute-path list from an `A3S_SEC_*` env entry.
#[cfg(target_os = "linux")]
fn parse_sec_path_list<'a>(env: &'a [String], prefix: &str) -> Vec<&'a str> {
    env.iter()
        .find_map(|entry| entry.strip_prefix(prefix))
        .map(|value| value.split(':').filter(|path| !path.is_empty()).collect())
        .unwrap_or_default()
}

/// Apply CRI `MaskedPaths` and `ReadonlyPaths` inside a container `rootfs`.
///
/// Mirrors the standard OCI runtime behaviour, mounting under `<rootfs>` (in the
/// guest mount namespace) before the container `chroot`s in:
/// - a masked *file* is shadowed by a bind mount of `/dev/null` (so execing or
///   reading it yields EACCES — "Permission denied");
/// - a masked *directory* is shadowed by an empty read-only `tmpfs`;
/// - a read-only path is bind-mounted onto itself and remounted `MS_RDONLY`
///   (so writes yield EROFS — "Read-only file system").
///
/// Best-effort: an absent path is skipped and any mount failure is logged.
#[cfg(target_os = "linux")]
fn apply_container_path_restrictions(rootfs: &str, masked: &[&str], readonly: &[&str]) {
    use nix::mount::{mount, MsFlags};

    use std::os::unix::fs::MetadataExt;

    // Reject entries that are not safe absolute paths (must be rooted, no `..`
    // component) so a crafted MaskedPaths/ReadonlyPaths value cannot escape the
    // container rootfs through the join below.
    let is_safe = |path: &str| path.starts_with('/') && !path.split('/').any(|c| c == "..");

    // A target is already restricted if it is a distinct mount — its st_dev
    // differs from its parent directory's. This makes re-application on every
    // exec idempotent (build_command runs per-exec) instead of stacking mounts.
    let is_mountpoint = |target: &str| -> bool {
        let Ok(dev) = std::fs::metadata(target).map(|m| m.dev()) else {
            return false;
        };
        std::path::Path::new(target)
            .parent()
            .and_then(|p| std::fs::metadata(p).ok())
            .map(|pm| pm.dev() != dev)
            .unwrap_or(false)
    };

    for path in masked {
        if !is_safe(path) {
            warn!("Skipping unsafe masked path {path}");
            continue;
        }
        let target = format!("{rootfs}{path}");
        if is_mountpoint(&target) {
            continue; // already masked
        }
        match std::fs::metadata(&target) {
            Ok(meta) if meta.is_dir() => {
                if let Err(e) = mount(
                    Some("tmpfs"),
                    target.as_str(),
                    Some("tmpfs"),
                    MsFlags::MS_RDONLY,
                    Some("size=0k"),
                ) {
                    warn!("Failed to mask directory {target}: {e}");
                }
            }
            Ok(_) => {
                if let Err(e) = mount(
                    Some("/dev/null"),
                    target.as_str(),
                    None::<&str>,
                    MsFlags::MS_BIND,
                    None::<&str>,
                ) {
                    warn!("Failed to mask file {target}: {e}");
                }
            }
            Err(_) => {} // path absent in this rootfs; nothing to mask
        }
    }

    for path in readonly {
        if !is_safe(path) {
            warn!("Skipping unsafe readonly path {path}");
            continue;
        }
        let target = format!("{rootfs}{path}");
        if !std::path::Path::new(&target).exists() {
            continue;
        }
        // Idempotent: skip if already read-only — re-binding would stack mounts
        // on every exec into the container.
        if nix::sys::statvfs::statvfs(target.as_str())
            .map(|s| s.flags().contains(nix::sys::statvfs::FsFlags::ST_RDONLY))
            .unwrap_or(false)
        {
            continue;
        }
        // A fresh bind is read-write regardless of the source, so bind then
        // remount read-only — the order matters.
        if let Err(e) = mount(
            Some(target.as_str()),
            target.as_str(),
            None::<&str>,
            MsFlags::MS_BIND | MsFlags::MS_REC,
            None::<&str>,
        ) {
            warn!("Failed to bind {target} for read-only: {e}");
            continue;
        }
        if let Err(e) = mount(
            None::<&str>,
            target.as_str(),
            None::<&str>,
            MsFlags::MS_BIND | MsFlags::MS_REMOUNT | MsFlags::MS_RDONLY,
            None::<&str>,
        ) {
            warn!("Failed to remount {target} read-only: {e}");
        }
    }
}

#[cfg_attr(not(unix), allow(dead_code))]
fn kill_child_process_group(child: &mut std::process::Child) {
    #[cfg(unix)]
    unsafe {
        let pid = child.id() as i32;
        if pid > 0 {
            let _ = libc::kill(-pid, libc::SIGKILL);
        }
    }
    let _ = child.kill();
    let _ = child.wait();
}

/// Truncate output to MAX_OUTPUT_BYTES if it exceeds the limit.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn truncate_output(mut data: Vec<u8>) -> Vec<u8> {
    if data.len() > MAX_OUTPUT_BYTES {
        data.truncate(MAX_OUTPUT_BYTES);
    }
    data
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
            None,
        );
        assert_ne!(output.exit_code, 0);
        assert!(!output.stderr.is_empty());
    }

    #[test]
    fn test_execute_command_empty() {
        let output = execute_command(&[], 0, &[], None, None, None, None);
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
        let output = execute_command(&["pwd".to_string()], 0, &[], Some("/tmp"), None, None, None);
        assert_eq!(output.exit_code, 0);
        let pwd = String::from_utf8_lossy(&output.stdout).trim().to_string();
        assert!(pwd == "/tmp" || pwd == "/private/tmp");
    }

    #[test]
    fn test_build_command_rejects_named_user() {
        let output = build_command(ExecCommandSpec {
            cmd: &["id".to_string()],
            timeout_ns: 0,
            env: &[],
            working_dir: None,
            rootfs: None,
            stdin_data: None,
            stdin_streaming: false,
            user: Some("node"),
        })
        .unwrap_err();

        assert_eq!(output.exit_code, 1);
        assert!(String::from_utf8_lossy(&output.stderr).contains("named user"));
    }

    #[test]
    fn test_build_command_keeps_original_program_with_numeric_user() {
        let (command, _) = build_command(ExecCommandSpec {
            cmd: &["echo".to_string(), "hello".to_string()],
            timeout_ns: 0,
            env: &[],
            working_dir: None,
            rootfs: None,
            stdin_data: None,
            stdin_streaming: false,
            user: Some("1000:1000"),
        })
        .unwrap();

        assert_eq!(command.get_program(), "echo");
        assert_eq!(
            command
                .get_args()
                .map(|arg| arg.to_string_lossy().to_string())
                .collect::<Vec<_>>(),
            vec!["hello".to_string()]
        );
    }

    #[test]
    fn test_execute_command_rejects_relative_rootfs() {
        let output = execute_command(
            &["true".to_string()],
            0,
            &[],
            None,
            Some("relative/rootfs"),
            None,
            None,
        );
        assert_eq!(output.exit_code, 1);
        assert!(String::from_utf8_lossy(&output.stderr).contains("Invalid rootfs path"));
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
            None,
            Some(b"hello from stdin"),
            None,
        );
        assert_eq!(output.exit_code, 0);
        assert_eq!(String::from_utf8_lossy(&output.stdout), "hello from stdin");
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

    #[test]
    fn test_write_exec_stream_response() {
        let output = ExecOutput {
            stdout: b"hello".to_vec(),
            stderr: b"warn".to_vec(),
            exit_code: 42,
        };

        let mut buf = Vec::new();
        write_exec_stream_response(&mut buf, &output).unwrap();

        let mut cursor = std::io::Cursor::new(buf);

        let (ft, payload) = read_frame(&mut cursor).unwrap().unwrap();
        assert_eq!(ft, FrameType::Data as u8);
        let chunk: ExecChunk = serde_json::from_slice(&payload).unwrap();
        assert_eq!(chunk.stream, StreamType::Stdout);
        assert_eq!(chunk.data, b"hello");

        let (ft, payload) = read_frame(&mut cursor).unwrap().unwrap();
        assert_eq!(ft, FrameType::Data as u8);
        let chunk: ExecChunk = serde_json::from_slice(&payload).unwrap();
        assert_eq!(chunk.stream, StreamType::Stderr);
        assert_eq!(chunk.data, b"warn");

        let (ft, payload) = read_frame(&mut cursor).unwrap().unwrap();
        assert_eq!(ft, FrameType::Control as u8);
        let exit: ExecExit = serde_json::from_slice(&payload).unwrap();
        assert_eq!(exit.exit_code, 42);

        assert!(read_frame(&mut cursor).unwrap().is_none());
    }

    #[test]
    fn test_write_exec_stream_response_chunks_large_output() {
        let output = ExecOutput {
            stdout: vec![b'a'; STREAM_CHUNK_BYTES + 7],
            stderr: vec![],
            exit_code: 0,
        };

        let mut buf = Vec::new();
        write_exec_stream_response(&mut buf, &output).unwrap();

        let mut cursor = std::io::Cursor::new(buf);

        let (ft, payload) = read_frame(&mut cursor).unwrap().unwrap();
        assert_eq!(ft, FrameType::Data as u8);
        let chunk: ExecChunk = serde_json::from_slice(&payload).unwrap();
        assert_eq!(chunk.stream, StreamType::Stdout);
        assert_eq!(chunk.data.len(), STREAM_CHUNK_BYTES);

        let (ft, payload) = read_frame(&mut cursor).unwrap().unwrap();
        assert_eq!(ft, FrameType::Data as u8);
        let chunk: ExecChunk = serde_json::from_slice(&payload).unwrap();
        assert_eq!(chunk.stream, StreamType::Stdout);
        assert_eq!(chunk.data.len(), 7);

        let (ft, payload) = read_frame(&mut cursor).unwrap().unwrap();
        assert_eq!(ft, FrameType::Control as u8);
        let exit: ExecExit = serde_json::from_slice(&payload).unwrap();
        assert_eq!(exit.exit_code, 0);

        assert!(read_frame(&mut cursor).unwrap().is_none());
    }

    #[test]
    #[cfg(unix)]
    fn test_execute_command_streaming_writes_output_and_exit() {
        let mut buf = Vec::new();
        execute_command_streaming(
            ExecCommandSpec {
                cmd: &[
                    "sh".to_string(),
                    "-c".to_string(),
                    "printf out; printf err >&2; exit 7".to_string(),
                ],
                timeout_ns: 0,
                env: &[],
                working_dir: None,
                rootfs: None,
                stdin_data: None,
                stdin_streaming: false,
                user: None,
            },
            None,
            &mut buf,
        )
        .unwrap();

        let mut cursor = std::io::Cursor::new(buf);
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let mut exit_code = None;

        while let Some((ft, payload)) = read_frame(&mut cursor).unwrap() {
            match ft {
                ft if ft == FrameType::Data as u8 => {
                    let chunk: ExecChunk = serde_json::from_slice(&payload).unwrap();
                    match chunk.stream {
                        StreamType::Stdout => stdout.extend_from_slice(&chunk.data),
                        StreamType::Stderr => stderr.extend_from_slice(&chunk.data),
                    }
                }
                ft if ft == FrameType::Control as u8 => {
                    let exit: ExecExit = serde_json::from_slice(&payload).unwrap();
                    exit_code = Some(exit.exit_code);
                }
                other => panic!("unexpected frame type: {other}"),
            }
        }

        assert_eq!(stdout, b"out");
        assert_eq!(stderr, b"err");
        assert_eq!(exit_code, Some(7));
    }

    struct FlushChannelWriter {
        sender: std::sync::mpsc::Sender<Vec<u8>>,
        buffer: Vec<u8>,
    }

    impl Write for FlushChannelWriter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.buffer.extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            if !self.buffer.is_empty() {
                self.sender
                    .send(std::mem::take(&mut self.buffer))
                    .map_err(|_| {
                        std::io::Error::new(std::io::ErrorKind::BrokenPipe, "test receiver closed")
                    })?;
            }
            Ok(())
        }
    }

    #[test]
    #[cfg(unix)]
    fn test_execute_command_streaming_emits_chunk_before_process_exit() {
        let (sender, receiver) = std::sync::mpsc::channel();
        let handle = std::thread::spawn(move || {
            let mut writer = FlushChannelWriter {
                sender,
                buffer: Vec::new(),
            };
            execute_command_streaming(
                ExecCommandSpec {
                    cmd: &[
                        "sh".to_string(),
                        "-c".to_string(),
                        "printf ready; sleep 1; printf done".to_string(),
                    ],
                    timeout_ns: 5_000_000_000,
                    env: &[],
                    working_dir: None,
                    rootfs: None,
                    stdin_data: None,
                    stdin_streaming: false,
                    user: None,
                },
                None,
                &mut writer,
            )
            .unwrap();
        });

        let first_frame = receiver
            .recv_timeout(Duration::from_millis(500))
            .expect("streaming exec did not emit output before process exit");
        let mut cursor = std::io::Cursor::new(first_frame);
        let (ft, payload) = read_frame(&mut cursor).unwrap().unwrap();
        assert_eq!(ft, FrameType::Data as u8);
        let chunk: ExecChunk = serde_json::from_slice(&payload).unwrap();
        assert_eq!(chunk.stream, StreamType::Stdout);
        assert_eq!(chunk.data, b"ready");

        handle.join().unwrap();
    }

    #[test]
    #[cfg(unix)]
    fn test_execute_command_streaming_writes_live_stdin() {
        let (input_tx, input_rx) = std::sync::mpsc::channel();
        let handle = std::thread::spawn(move || {
            let mut buf = Vec::new();
            execute_command_streaming(
                ExecCommandSpec {
                    cmd: &["cat".to_string()],
                    timeout_ns: 5_000_000_000,
                    env: &[],
                    working_dir: None,
                    rootfs: None,
                    stdin_data: None,
                    stdin_streaming: true,
                    user: None,
                },
                Some(input_rx),
                &mut buf,
            )
            .unwrap();
            buf
        });

        input_tx
            .send(ExecInputEvent::Stdin(b"hello live stdin".to_vec()))
            .unwrap();
        input_tx.send(ExecInputEvent::StdinClose).unwrap();

        let mut cursor = std::io::Cursor::new(handle.join().unwrap());
        let mut stdout = Vec::new();
        let mut exit_code = None;

        while let Some((ft, payload)) = read_frame(&mut cursor).unwrap() {
            match ft {
                ft if ft == FrameType::Data as u8 => {
                    let chunk: ExecChunk = serde_json::from_slice(&payload).unwrap();
                    if chunk.stream == StreamType::Stdout {
                        stdout.extend_from_slice(&chunk.data);
                    }
                }
                ft if ft == FrameType::Control as u8 => {
                    let exit: ExecExit = serde_json::from_slice(&payload).unwrap();
                    exit_code = Some(exit.exit_code);
                }
                other => panic!("unexpected frame type: {other}"),
            }
        }

        assert_eq!(stdout, b"hello live stdin");
        assert_eq!(exit_code, Some(0));
    }

    #[test]
    #[cfg(unix)]
    fn test_execute_command_streaming_cancel_kills_child() {
        let (input_tx, input_rx) = std::sync::mpsc::channel();
        let mut buf = Vec::new();

        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(200));
            input_tx.send(ExecInputEvent::Cancel).unwrap();
        });

        execute_command_streaming(
            ExecCommandSpec {
                cmd: &[
                    "sh".to_string(),
                    "-c".to_string(),
                    "printf ready; sleep 5; printf done".to_string(),
                ],
                timeout_ns: 10_000_000_000,
                env: &[],
                working_dir: None,
                rootfs: None,
                stdin_data: None,
                stdin_streaming: false,
                user: None,
            },
            Some(input_rx),
            &mut buf,
        )
        .unwrap();

        let mut cursor = std::io::Cursor::new(buf);
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let mut exit_code = None;

        while let Some((ft, payload)) = read_frame(&mut cursor).unwrap() {
            match ft {
                ft if ft == FrameType::Data as u8 => {
                    let chunk: ExecChunk = serde_json::from_slice(&payload).unwrap();
                    match chunk.stream {
                        StreamType::Stdout => stdout.extend_from_slice(&chunk.data),
                        StreamType::Stderr => stderr.extend_from_slice(&chunk.data),
                    }
                }
                ft if ft == FrameType::Control as u8 => {
                    let exit: ExecExit = serde_json::from_slice(&payload).unwrap();
                    exit_code = Some(exit.exit_code);
                }
                other => panic!("unexpected frame type: {other}"),
            }
        }

        assert_eq!(stdout, b"ready");
        assert!(!String::from_utf8_lossy(&stdout).contains("done"));
        assert!(String::from_utf8_lossy(&stderr).contains("stop requested"));
        assert_eq!(exit_code, Some(137));
    }
}
