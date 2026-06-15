//! Guest exec server for executing commands inside the VM.
//!
//! Listens on vsock port 4089 and accepts Frame-based requests.
//! Each connection: read a Data frame (JSON ExecRequest), execute,
//! then send either a one-shot `ExecOutput` or streaming chunk/exit frames.

use std::io::Read;
use std::io::Write;
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::mpsc;
use std::time::Duration;

use a3s_box_core::exec::{
    ExecChunk, ExecExit, ExecOutput, StreamType, DEFAULT_EXEC_TIMEOUT_NS, EXEC_VSOCK_PORT,
    MAX_OUTPUT_BYTES,
};
use a3s_transport::FrameType;
use tracing::{info, warn};

use crate::user::{parse_process_user, ProcessUser};

/// PID of the main container process (the entrypoint spawned by guest init).
/// Set by `main` after spawn so the exec server can deliver a graceful stop
/// signal to it on a host shutdown request. -1 until known.
static CONTAINER_PID: AtomicI32 = AtomicI32::new(-1);

/// Record the main container PID for graceful-shutdown signal delivery.
pub fn set_container_pid(pid: i32) {
    CONTAINER_PID.store(pid, Ordering::SeqCst);
}

/// The main container PID (-1 if not yet spawned, -2 while a deferred spawn is in
/// flight). The PID 1 supervision loop reads this each tick, so a deferred main
/// published here (after an IDLE boot) is recognized as the container and reaped
/// for its real exit code.
pub fn container_pid() -> i32 {
    CONTAINER_PID.load(Ordering::SeqCst)
}

/// Host→guest control to gracefully stop the container: deliver the given signal
/// number to the main container process. `signal-main:<N>` (e.g. `signal-main:15`
/// for SIGTERM, `signal-main:2` for the image STOPSIGNAL=SIGINT). The container
/// then runs its own shutdown; when it exits, guest init exits and the VM stops.
/// Must match the host's prefix in `runtime/src/grpc/exec.rs`.
#[cfg(target_os = "linux")]
const EXEC_CONTROL_SIGNAL_MAIN: &[u8] = b"signal-main:";
#[cfg(target_os = "linux")]
const EXEC_SIGNAL_MAIN_ACK: &[u8] = b"signal-main-ack";

/// Host→guest control to spawn the container MAIN process on demand — for VMs that
/// booted IDLE (`BOX_DEFERRED_MAIN=1`, e.g. a pre-warmed pool sandbox). Payload is
/// `spawn-main:<json {executable,args,env,workdir}>`. The spawned process becomes
/// the container main: it inherits PID 1's console fds (so its stdout/stderr reach
/// the json-file logs, unlike a piped exec) and the supervision loop reaps it for
/// the real exit code. Must match the host prefix in `runtime/src/grpc/exec.rs`.
#[cfg(target_os = "linux")]
const EXEC_CONTROL_SPAWN_MAIN: &[u8] = b"spawn-main:";
#[cfg(target_os = "linux")]
const EXEC_SPAWN_MAIN_ACK: &[u8] = b"spawn-main-ack";
#[cfg(target_os = "linux")]
const EXEC_SPAWN_MAIN_NACK: &[u8] = b"spawn-main-nack:";

/// Deliver `sig` to the main container process (best-effort).
#[cfg(target_os = "linux")]
fn signal_main_process(sig: i32) {
    let pid = CONTAINER_PID.load(Ordering::SeqCst);
    if pid > 0 {
        info!(
            pid,
            sig, "Delivering graceful stop signal to container main process"
        );
        unsafe {
            libc::kill(pid, sig);
        }
    } else {
        warn!(sig, "Graceful stop requested but container PID is unknown");
    }
}

/// The container command, stashed at boot (parsed from BOX_EXEC_*), so a later
/// `spawn-main` trigger can run it as the main without the host re-sending it.
#[cfg(target_os = "linux")]
#[derive(serde::Deserialize)]
struct DeferredMainSpec {
    executable: String,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    env: Vec<(String, String)>,
    #[serde(default)]
    workdir: Option<String>,
    #[serde(default)]
    user: Option<String>,
}

#[cfg(target_os = "linux")]
static DEFERRED_MAIN: std::sync::Mutex<Option<DeferredMainSpec>> = std::sync::Mutex::new(None);

/// Stash the container command for a deferred (IDLE) boot. The command already
/// reached the guest via BOX_EXEC_*, so the host only sends a bare spawn-main
/// trigger post-readiness; the guest runs the stashed command as its main.
#[cfg(target_os = "linux")]
pub fn set_deferred_main_spec(
    executable: String,
    args: Vec<String>,
    env: Vec<(String, String)>,
    workdir: Option<String>,
    user: Option<String>,
) {
    *DEFERRED_MAIN.lock().unwrap_or_else(|e| e.into_inner()) = Some(DeferredMainSpec {
        executable,
        args,
        env,
        workdir,
        user,
    });
}

/// Spawn the deferred container main (after an IDLE boot). The child inherits PID
/// 1's stdout/stderr — fds 1/2 = the virtio-console — so its output reaches
/// `console.log` → `container.json`, exactly like a boot-spawned main (and unlike a
/// `Stdio::piped` exec, whose output only flows over the exec stream). It is spawned
/// via `Command::spawn` (the same clone/exec the exec server already uses safely),
/// NOT `namespace::spawn_isolated`'s raw `fork()` — whose heavy allocating child
/// code could deadlock from this multi-threaded PID 1. The pid is published WHILE
/// still registered MANAGED (the reaper can't reap it as an orphan before the
/// hand-off), then released to the supervision loop, which reaps it for the real
/// exit code. A CAS makes only the first spawn-main win.
#[cfg(target_os = "linux")]
fn spawn_deferred_main(frame: Option<DeferredMainSpec>) -> Result<i32, String> {
    // Use the command carried in the frame (the pool path — a pre-warmed VM gets
    // its per-request command here), else the one stashed at boot from BOX_EXEC_*
    // (the `run` path, where the command is known at boot).
    let (executable, args, env, workdir, user) = match frame {
        Some(s) => (s.executable, s.args, s.env, s.workdir, s.user),
        None => {
            let guard = DEFERRED_MAIN.lock().unwrap_or_else(|e| e.into_inner());
            let spec = guard.as_ref().ok_or("no deferred-main command set")?;
            (
                spec.executable.clone(),
                spec.args.clone(),
                spec.env.clone(),
                spec.workdir.clone(),
                spec.user.clone(),
            )
        }
    };

    // cmd vector + env. Include the guest's own A3S_SEC_* control vars so
    // build_command applies the SAME seccomp/user/no-new-privs as a boot-spawned
    // main (the container env carries them on a normal exec; here we add them).
    let mut cmd_vec = Vec::with_capacity(1 + args.len());
    cmd_vec.push(executable);
    cmd_vec.extend(args);
    let mut env_entries: Vec<String> = env.iter().map(|(k, v)| format!("{k}={v}")).collect();
    for (k, v) in std::env::vars() {
        if k.starts_with("A3S_SEC_") {
            env_entries.push(format!("{k}={v}"));
        }
    }

    // Reuse the exec server's secured command builder (seccomp + user + no-new-privs
    // via async-signal-safe pre_exec — already safe to spawn from this multi-threaded
    // PID 1), then override stdio to INHERIT so the main's stdout/stderr reach PID
    // 1's console fds (→ json-file logs), unlike an exec's piped stdio.
    let (mut command, _timeout) = build_command(
        ExecCommandSpec {
            cmd: &cmd_vec,
            timeout_ns: 0,
            env: &env_entries,
            working_dir: workdir.as_deref(),
            rootfs: None,
            stdin_data: None,
            stdin_streaming: false,
            user: user.as_deref(),
        },
        None,
    )
    .map_err(|out| String::from_utf8_lossy(&out.stderr).into_owned())?;
    command
        .stdout(std::process::Stdio::inherit())
        .stderr(std::process::Stdio::inherit())
        .stdin(std::process::Stdio::null());

    // Idempotency: claim the sentinel (-1 → -2 pending); a second spawn-main loses.
    if CONTAINER_PID
        .compare_exchange(-1, -2, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
        return Err("container main already spawned".to_string());
    }

    match crate::reaper::spawn_managed(|| command.spawn()) {
        Ok((child, guard)) => {
            let pid = child.id() as i32;
            // Publish the real pid (over the -2 marker) while still MANAGED, then
            // release ownership: now the loop's `pid == container_pid` branch reaps.
            CONTAINER_PID.store(pid, Ordering::SeqCst);
            std::mem::forget(child); // PID 1's reaper owns it — do not double-reap
            drop(guard);
            Ok(pid)
        }
        Err(e) => {
            CONTAINER_PID.store(-1, Ordering::SeqCst); // reset so a retry is possible
            Err(format!("spawn failed: {e}"))
        }
    }
}

/// Maximum payload bytes per streamed exec chunk.
const STREAM_CHUNK_BYTES: usize = 16 * 1024;
const EXEC_CONTROL_CANCEL: &[u8] = b"cancel";
const EXEC_CONTROL_STDIN_CLOSE: &[u8] = b"stdin-close";
/// Host→guest control: flush all buffered output, then reply with a flush-ack.
const EXEC_CONTROL_FLUSH: &[u8] = b"flush";
/// Guest→host marker (in a Control frame) acknowledging a flush: every output
/// chunk buffered when the flush was received has been sent ahead of it. Must
/// match the host's `EXEC_FLUSH_ACK` in `runtime/src/grpc/exec.rs`.
const EXEC_FLUSH_ACK: &[u8] = b"flush-ack";

/// A bound, listening exec-server socket — produced by [`bind_exec_server`] and
/// consumed by [`serve_exec_server`].
///
/// Splitting bind from serve lets guest-init bind the exec vsock port EARLY on
/// the main thread (pure socket/bind/listen syscalls — no thread spawn, so the
/// later single-threaded container `fork()` stays fork-safe) while the accept
/// loop runs afterwards in its own thread. Binding early fills the listen
/// backlog from the start of boot, so a host connect QUEUES instead of being
/// refused while the slower boot steps (network, container spawn) finish — this
/// removes the "Connection refused" / heartbeat race of issue #3. On non-Linux
/// this is an inert placeholder so callers stay platform-agnostic.
#[cfg(target_os = "linux")]
pub struct ExecListener(std::os::fd::OwnedFd);
#[cfg(not(target_os = "linux"))]
pub struct ExecListener;

/// Bind + listen the exec vsock socket (port 4089). Pure socket syscalls, safe
/// to call on the main thread before the container fork.
pub fn bind_exec_server() -> Result<ExecListener, Box<dyn std::error::Error>> {
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

        let addr = VsockAddr::new(libc::VMADDR_CID_ANY, EXEC_VSOCK_PORT);
        bind(sock_fd.as_raw_fd(), &addr)?;
        listen(&sock_fd, Backlog::new(4)?)?;

        info!("Exec server listening on vsock port {}", EXEC_VSOCK_PORT);
        Ok(ExecListener(sock_fd))
    }

    #[cfg(not(target_os = "linux"))]
    {
        info!("Exec server not available on non-Linux platform (development mode)");
        Ok(ExecListener)
    }
}

/// Run the exec accept loop on an already-bound listener. Intended to run on its
/// own thread for the VM's lifetime; never returns under normal operation.
pub fn serve_exec_server(listener: ExecListener) -> Result<(), Box<dyn std::error::Error>> {
    #[cfg(target_os = "linux")]
    {
        run_accept_loop(listener.0)
    }

    #[cfg(not(target_os = "linux"))]
    {
        let _ = listener;
        Ok(())
    }
}

/// Bind then serve in one call. Kept for callers that don't need the early-bind
/// split (e.g. tests); guest-init's boot path uses `bind_*` + `serve_*` directly.
pub fn run_exec_server() -> Result<(), Box<dyn std::error::Error>> {
    info!("Starting exec server on vsock port {}", EXEC_VSOCK_PORT);
    serve_exec_server(bind_exec_server()?)
}

/// The exec server accept loop.
#[cfg(target_os = "linux")]
fn run_accept_loop(sock_fd: std::os::fd::OwnedFd) -> Result<(), Box<dyn std::error::Error>> {
    use nix::sys::socket::accept;
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    use tracing::error;

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
        // Graceful-stop control: deliver a signal to the container main process.
        if frame_type == FrameType::Control as u8 && payload.starts_with(EXEC_CONTROL_SIGNAL_MAIN) {
            let sig = std::str::from_utf8(&payload[EXEC_CONTROL_SIGNAL_MAIN.len()..])
                .ok()
                .and_then(|s| s.trim().parse::<i32>().ok())
                .filter(|n| *n > 0 && *n <= 64) // valid Linux signals are 1..=SIGRTMAX(64)
                .unwrap_or(libc::SIGTERM);
            signal_main_process(sig);
            write_frame(&mut stream, FrameType::Control as u8, EXEC_SIGNAL_MAIN_ACK)?;
            std::mem::forget(fd);
            return Ok(());
        }
        // Deferred-main control: spawn the container main on demand (IDLE boot).
        if frame_type == FrameType::Control as u8 && payload.starts_with(EXEC_CONTROL_SPAWN_MAIN) {
            // Optional JSON body carries the command (pool path); empty body uses
            // the command stashed at boot (run path).
            let body = &payload[EXEC_CONTROL_SPAWN_MAIN.len()..];
            let result = if body.is_empty() {
                spawn_deferred_main(None)
            } else {
                match serde_json::from_slice::<DeferredMainSpec>(body) {
                    Ok(spec) => spawn_deferred_main(Some(spec)),
                    Err(e) => Err(format!("invalid spawn-main spec: {e}")),
                }
            };
            match result {
                Ok(pid) => {
                    info!(pid, "Deferred container main spawned");
                    write_frame(&mut stream, FrameType::Control as u8, EXEC_SPAWN_MAIN_ACK)?;
                }
                Err(e) => {
                    warn!(error = %e, "spawn-main failed");
                    let mut nack = EXEC_SPAWN_MAIN_NACK.to_vec();
                    nack.extend_from_slice(e.as_bytes());
                    write_frame(&mut stream, FrameType::Control as u8, &nack)?;
                }
            }
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
    /// Flush buffered output and emit a flush-ack (log-rotation boundary).
    Flush,
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
            Ok(Some((frame_type, payload)))
                if frame_type == FrameType::Control as u8 && payload == EXEC_CONTROL_FLUSH =>
            {
                if tx.send(ExecInputEvent::Flush).is_err() {
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
    write_exec_exit(w, output.exit_code, false)
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
fn write_exec_exit(
    w: &mut impl Write,
    exit_code: i32,
    oom_killed: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let exit = ExecExit {
        exit_code,
        oom_killed,
    };
    let payload = serde_json::to_vec(&exit)?;
    write_frame(w, FrameType::Control as u8, &payload)?;
    Ok(())
}

/// Emit a flush-ack Control frame, marking that all output buffered when the
/// flush was received has now been sent.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn write_exec_flush_ack(w: &mut impl Write) -> Result<(), Box<dyn std::error::Error>> {
    write_frame(w, FrameType::Control as u8, EXEC_FLUSH_ACK)?;
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
    cgroup_procs: Option<&str>,
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
    // Resolve a named user (CRI RunAsUserName, or `exec -u <name>`) against the
    // container's /etc/passwd before numeric parsing; falls through to spec.user
    // when the value is already numeric/root or cannot be resolved. For an exec
    // (no rootfs override) the container root is the current `/`.
    let resolve_rootfs = spec.rootfs.unwrap_or("/");
    let resolved_user = spec
        .user
        .and_then(|user| crate::user::resolve_named_user(user, resolve_rootfs));
    let mut process_user = match parse_process_user(resolved_user.as_deref().or(spec.user)) {
        Ok(process_user) => process_user,
        Err(error) => {
            return Err(ExecOutput {
                stdout: vec![],
                stderr: error.into_bytes(),
                exit_code: 1,
            });
        }
    };
    // When a user is set without an explicit group (RunAsUser, no RunAsGroup),
    // default the primary gid to the user's /etc/passwd group — matching how a
    // normal login derives the primary group — instead of inheriting root's.
    if let Some(process_user) = process_user.as_mut() {
        if process_user.gid.is_none() {
            process_user.gid = crate::user::primary_gid_for_uid(resolve_rootfs, process_user.uid);
        }
    }

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
                // Populate the standard /dev device nodes (urandom, null, ...);
                // many workloads (e.g. Apache httpd seeding its RNG from
                // /dev/urandom) fail to start without them.
                ensure_container_dev_nodes(rootfs);
                // CRI MaskedPaths/ReadonlyPaths arrive as ':'-separated absolute
                // paths over A3S_SEC_MASKED_PATHS / A3S_SEC_READONLY_PATHS.
                let masked = parse_sec_path_list(spec.env, "A3S_SEC_MASKED_PATHS=");
                let readonly = parse_sec_path_list(spec.env, "A3S_SEC_READONLY_PATHS=");
                if !masked.is_empty() || !readonly.is_empty() {
                    apply_container_path_restrictions(rootfs, &masked, &readonly);
                }
                // CRI readonly_rootfs: remount the container root read-only
                // AFTER the pseudo-filesystems and path restrictions are set up,
                // so /proc, /sys, and any inner mounts stay writable.
                if spec
                    .env
                    .iter()
                    .any(|entry| entry == "A3S_SEC_READONLY_ROOTFS=1")
                {
                    remount_rootfs_readonly(rootfs);
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
    let mut supplemental_groups: Vec<u32> = spec
        .env
        .iter()
        .find_map(|entry| entry.strip_prefix("A3S_SEC_SUPPLEMENTAL_GROUPS="))
        .map(|csv| {
            csv.split(',')
                .filter_map(|gid| gid.trim().parse::<u32>().ok())
                .collect()
        })
        .unwrap_or_default();
    // runc-style initgroups: when running as a specific user, add the groups
    // that user belongs to per the image's /etc/group (resolved here, pre-fork).
    // CRI-supplied groups take precedence; image groups are appended + deduped.
    if let (Some(process_user), Some(rootfs)) = (process_user, spec.rootfs) {
        let image_groups = crate::user::resolve_image_groups(
            rootfs,
            process_user.uid,
            process_user.gid,
            spec.user.unwrap_or("root"),
        );
        supplemental_groups.extend(image_groups);
        let mut seen = std::collections::HashSet::new();
        supplemental_groups.retain(|gid| seen.insert(*gid));
    }
    // CRI seccomp: A3S_SEC_SECCOMP=default applies the default BPF filter
    // (RuntimeDefault) in the child; unconfined/unset leave the process
    // unfiltered.
    let apply_seccomp = spec
        .env
        .iter()
        .any(|entry| entry == "A3S_SEC_SECCOMP=default");
    // CRI capability drop: A3S_SEC_CAP_DROP=NAME,NAME,... (or ALL) — the guest
    // clears these from the child's effective/permitted/inheritable + bounding
    // sets before exec.
    let cap_drop: Vec<String> = spec
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
    // CRI seccomp Localhost: A3S_SEC_SECCOMP_LOCALHOST=name,name lists the
    // profile's SCMP_ACT_ERRNO syscalls (defaultAction=ALLOW). The guest builds
    // an ERRNO filter for them.
    let seccomp_localhost: Vec<String> = spec
        .env
        .iter()
        .find_map(|entry| entry.strip_prefix("A3S_SEC_SECCOMP_LOCALHOST="))
        .map(|csv| {
            csv.split(',')
                .map(|name| name.trim().to_string())
                .filter(|name| !name.is_empty())
                .collect()
        })
        .unwrap_or_default();
    // CRI capability keep-set: A3S_SEC_CAP_KEEP=NAME,NAME,... restricts a
    // non-privileged container to exactly these capabilities (the runtime
    // default adjusted by add/drop), dropping all others. Present-but-empty
    // means drop everything; absent means leave the full set (privileged).
    let cap_keep: Option<Vec<String>> = spec
        .env
        .iter()
        .find_map(|entry| entry.strip_prefix("A3S_SEC_CAP_KEEP="))
        .map(|csv| {
            csv.split(',')
                .map(|name| name.trim().to_string())
                .filter(|name| !name.is_empty())
                .collect()
        });
    // CRI no_new_privs: A3S_SEC_NO_NEW_PRIVS=1 sets PR_SET_NO_NEW_PRIVS in the
    // child before exec, so a setuid/file-capability binary cannot raise privs.
    let no_new_privs = spec
        .env
        .iter()
        .any(|entry| entry == "A3S_SEC_NO_NEW_PRIVS=1");
    configure_child_process(
        &mut command,
        spec.rootfs,
        workdir,
        process_user,
        supplemental_groups,
        apply_seccomp,
        cap_drop,
        cap_keep,
        seccomp_localhost,
        no_new_privs,
        cgroup_procs,
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
    let (mut command, timeout) = match build_command(
        ExecCommandSpec {
            cmd,
            timeout_ns,
            env,
            working_dir,
            rootfs,
            stdin_data,
            stdin_streaming: false,
            user,
        },
        None,
    ) {
        Ok(command) => command,
        Err(output) => return output,
    };

    // Spawn under the reaper registry: the pid is marked MANAGED before the PID 1
    // supervision loop can see it, so the loop leaves this child for us to reap
    // (and read its real exit code) instead of stealing it. The guard unregisters
    // the pid when this function returns (all paths).
    let (mut child, _reap_guard) = match crate::reaper::spawn_managed(|| command.spawn()) {
        Ok(pair) => pair,
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
            Err(ref e) if e.raw_os_error() == Some(libc::ECHILD) => {
                // Child already reaped (timing race in microVM PID 1 context).
                let (stdout, stderr) = read_child_output(&mut child);
                return ExecOutput {
                    stdout: truncate_output(stdout),
                    stderr: truncate_output(stderr),
                    exit_code: 0,
                };
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
        if process_streaming_input(child, input_rx, receiver, writer)? {
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

        if process_streaming_input(child, input_rx, receiver, writer)? {
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
            Err(ref e) if e.raw_os_error() == Some(libc::ECHILD) => {
                // The child exited and was reaped before this try_wait — a
                // timing race where the output pipes closed (confirming the
                // child finished) but the zombie was collected first (can happen
                // inside microVMs where PID 1's housekeeping overlaps). Treat
                // as a clean exit (code 0); the caller observes the output.
                drain_stream_reader_events(receiver, writer)?;
                return Ok((0, None));
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
#[derive(Default)]
struct InputDrainOutcome {
    /// A cancel/stop was requested.
    cancel: bool,
    /// A flush was requested (drain buffered output, then emit a flush-ack).
    flush: bool,
}

fn drain_exec_input_events(
    child: &mut std::process::Child,
    input_rx: Option<&mpsc::Receiver<ExecInputEvent>>,
) -> InputDrainOutcome {
    let mut outcome = InputDrainOutcome::default();
    let Some(input_rx) = input_rx else {
        return outcome;
    };

    loop {
        match input_rx.try_recv() {
            Ok(ExecInputEvent::Stdin(data)) => write_live_child_stdin(child, &data),
            Ok(ExecInputEvent::StdinClose) => {
                let _ = child.stdin.take();
            }
            Ok(ExecInputEvent::Flush) => outcome.flush = true,
            Ok(ExecInputEvent::Cancel) => {
                outcome.cancel = true;
                return outcome;
            }
            Err(mpsc::TryRecvError::Empty) => return outcome,
            // The host closed the exec connection without an explicit cancel
            // (e.g. a3s-box-cri died, or a client disconnect the host didn't
            // translate to a cancel). Treat it as a cancel so the command does
            // not keep running orphaned in the guest.
            Err(mpsc::TryRecvError::Disconnected) => {
                outcome.cancel = true;
                return outcome;
            }
        }
    }
}

/// Process pending input events: apply stdin/close, and on a flush request
/// drain all buffered output to the writer before emitting a flush-ack.
/// Returns `true` if a cancel/stop was requested.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn process_streaming_input(
    child: &mut std::process::Child,
    input_rx: Option<&mpsc::Receiver<ExecInputEvent>>,
    receiver: &mpsc::Receiver<StreamReaderEvent>,
    writer: &mut impl Write,
) -> Result<bool, Box<dyn std::error::Error>> {
    let outcome = drain_exec_input_events(child, input_rx);
    if outcome.flush {
        // Everything the reader threads have queued is written first, so the
        // ack marks a definitive boundary for log rotation.
        drain_stream_reader_events(receiver, writer)?;
        write_exec_flush_ack(writer)?;
    }
    Ok(outcome.cancel)
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
    // Per-container cgroup v2 (memory.max / cpu.max) for resource limits + OOM
    // accounting. Created BEFORE build_command so its cgroup.procs path can be
    // handed to the pre-exec hook: the container joins the cgroup itself (before
    // exec), so every worker it forks is bounded too — a parent-side join after
    // spawn races with workers the container forks immediately.
    #[cfg(target_os = "linux")]
    let container_cgroup = crate::cgroup::ContainerCgroup::create(
        parse_sec_mem_limit(spec.env),
        parse_sec_int(spec.env, "A3S_SEC_CPU_QUOTA="),
        parse_sec_int(spec.env, "A3S_SEC_CPU_PERIOD=").map(|value| value as u64),
        parse_sec_int(spec.env, "A3S_SEC_CPU_SHARES=").map(|value| value as u64),
        parse_sec_int(spec.env, "A3S_SEC_PIDS_LIMIT=").map(|value| value as u64),
    );
    #[cfg(target_os = "linux")]
    let cgroup_procs = container_cgroup.as_ref().map(|cgroup| cgroup.procs_path());
    #[cfg(not(target_os = "linux"))]
    let cgroup_procs: Option<String> = None;

    let (mut command, timeout) = match build_command(spec, cgroup_procs.as_deref()) {
        Ok(command) => command,
        Err(output) => {
            write_exec_stream_response(writer, &output)?;
            return Ok(());
        }
    };

    // Spawn under the reaper registry (see one-shot path) so PID 1 leaves this
    // streaming child for us to reap; the guard unregisters on return.
    let (mut child, _reap_guard) = match crate::reaper::spawn_managed(|| command.spawn()) {
        Ok(pair) => pair,
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

    // A non-zero cgroup `oom_kill` count means the kernel OOM-killer reaped the
    // container for exceeding its memory limit — report it as OOMKilled.
    #[cfg(target_os = "linux")]
    let oom_killed = container_cgroup
        .as_ref()
        .is_some_and(|cgroup| cgroup.oom_kills() > 0);
    #[cfg(not(target_os = "linux"))]
    let oom_killed = false;
    write_exec_exit(writer, exit_code, oom_killed)
}

#[cfg(unix)]
#[allow(clippy::too_many_arguments)] // cohesive child-process setup parameters
fn configure_child_process(
    command: &mut std::process::Command,
    rootfs: Option<&str>,
    workdir: &str,
    user: Option<ProcessUser>,
    supplemental_groups: Vec<u32>,
    apply_seccomp: bool,
    cap_drop: Vec<String>,
    cap_keep: Option<Vec<String>>,
    seccomp_localhost: Vec<String>,
    no_new_privs: bool,
    cgroup_procs: Option<&str>,
) {
    use std::ffi::CString;
    use std::os::unix::process::CommandExt;

    let rootfs = rootfs
        .map(|rootfs| CString::new(rootfs.as_bytes()).expect("rootfs path was pre-validated"));
    // The per-container cgroup's `cgroup.procs` path; the child writes its own
    // PID here (before chroot) so it — and everything it forks — is bounded by
    // the cgroup's memory.max / cpu.max from birth. Built pre-fork (allocates).
    let cgroup_procs = cgroup_procs.and_then(|path| CString::new(path.as_bytes()).ok());
    // workdir (for chdir) is only used when chrooting into a rootfs, where
    // build_command has already rejected an embedded NUL. Build the CString only
    // in that case so a workdir containing a NUL with no rootfs set cannot panic
    // this exec thread.
    let workdir = rootfs
        .as_ref()
        .map(|_| CString::new(workdir.as_bytes()).expect("working directory was pre-validated"));

    // Build the seccomp BPF filter BEFORE fork: building allocates, and
    // allocating in the post-fork child is not async-signal-safe (malloc may
    // deadlock on musl). The child only installs the prebuilt filter. A
    // Localhost profile (its SCMP_ACT_ERRNO syscalls) takes precedence over the
    // RuntimeDefault filter.
    #[cfg(target_os = "linux")]
    let seccomp_filter = if !seccomp_localhost.is_empty() {
        let deny: Vec<u32> = seccomp_localhost
            .iter()
            .filter_map(|name| crate::namespace::syscall_name_to_number(name))
            .collect();
        Some(crate::namespace::build_seccomp_errno_filter(&deny))
    } else if apply_seccomp {
        Some(crate::namespace::build_default_bpf_filter())
    } else {
        None
    };
    #[cfg(not(target_os = "linux"))]
    {
        let _ = apply_seccomp;
        let _ = &cap_drop;
        let _ = &cap_keep;
        let _ = &seccomp_localhost;
        let _ = no_new_privs;
    }

    unsafe {
        command.pre_exec(move || {
            // Join the per-container cgroup FIRST, before chroot (afterwards the
            // guest /sys/fs/cgroup is unreachable). Best-effort and entirely
            // async-signal-safe: open + getpid + a stack-only itoa + write +
            // close, no allocation. A failure here leaves the container
            // unbounded rather than failing its launch.
            if let Some(procs) = cgroup_procs.as_ref() {
                let fd = libc::open(procs.as_ptr(), libc::O_WRONLY);
                if fd >= 0 {
                    let mut buf = [0u8; 20];
                    let mut i = buf.len();
                    let mut n = libc::getpid() as u64;
                    if n == 0 {
                        i -= 1;
                        buf[i] = b'0';
                    }
                    while n > 0 {
                        i -= 1;
                        buf[i] = b'0' + (n % 10) as u8;
                        n /= 10;
                    }
                    let _ = libc::write(
                        fd,
                        buf[i..].as_ptr() as *const libc::c_void,
                        (buf.len() - i) as libc::size_t,
                    );
                    let _ = libc::close(fd);
                }
            }
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
            // Apply capabilities BEFORE the uid/gid switch, while the process is
            // still root and holds CAP_SETPCAP: capset / PR_CAPBSET_DROP require
            // it, and a setuid to a non-root user clears the effective set and
            // would make a later capset fail (breaking RunAsUser containers). A
            // keep-set (the CRI default for a non-privileged container) reduces
            // to exactly those caps; the legacy drop-list path remains for
            // explicit-drop-only callers. The default keep-set retains
            // CAP_SETUID/CAP_SETGID so the subsequent user.apply still works.
            #[cfg(target_os = "linux")]
            if let Some(cap_keep) = &cap_keep {
                crate::namespace::restrict_capabilities_to_keep(cap_keep)?;
            } else if !cap_drop.is_empty() {
                crate::namespace::drop_capabilities(&cap_drop)?;
            }
            if let Some(user) = user {
                user.apply()?;
            }
            // Set no_new_privs before installing seccomp: a single prctl
            // (async-signal-safe) that prevents any later execve from gaining
            // privileges via setuid/setgid bits or file capabilities.
            #[cfg(target_os = "linux")]
            if no_new_privs {
                crate::namespace::set_no_new_privs()?;
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
    _cap_drop: Vec<String>,
    _cap_keep: Option<Vec<String>>,
    _seccomp_localhost: Vec<String>,
    _no_new_privs: bool,
    _cgroup_procs: Option<&str>,
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

/// Create the standard character device nodes in a container `rootfs`.
///
/// A minimal image rootfs has an empty `/dev`, but many workloads need the
/// usual nodes — e.g. Apache httpd reads `/dev/urandom` to seed its RNG and
/// aborts with `AH00141` if it is absent. Created with `mknod` while guest-init
/// still holds `CAP_MKNOD` (before the privilege drop and chroot). Best-effort
/// and idempotent: an existing node is left as-is.
#[cfg(target_os = "linux")]
fn ensure_container_dev_nodes(rootfs: &str) {
    let dev = format!("{rootfs}/dev");
    if let Err(e) = std::fs::create_dir_all(&dev) {
        warn!("Failed to create container /dev {dev}: {e}");
        return;
    }
    // (name, major, minor) — the fixed Linux char-device numbers.
    for (name, major, minor) in [
        ("null", 1u32, 3u32),
        ("zero", 1, 5),
        ("full", 1, 7),
        ("random", 1, 8),
        ("urandom", 1, 9),
        ("tty", 5, 0),
    ] {
        let path = format!("{dev}/{name}");
        if std::path::Path::new(&path).exists() {
            continue;
        }
        let Ok(cpath) = std::ffi::CString::new(path.as_str()) else {
            continue;
        };
        let mode: libc::mode_t = libc::S_IFCHR | 0o666;
        // SAFETY: mknod with a valid CString path + fixed device numbers.
        let ret = unsafe { libc::mknod(cpath.as_ptr(), mode, libc::makedev(major, minor)) };
        if ret != 0 {
            warn!(
                "Failed to mknod {path}: {}",
                std::io::Error::last_os_error()
            );
        }
    }

    // Standard /dev/std{in,out,err} + /dev/fd symlinks into the process's own fds.
    // Apps that log to /dev/stdout or /dev/stderr (official nginx, and many others)
    // then resolve to their own stdio — which is re-openable now that the main
    // process's stdout/stderr are pipes (see setup_main_stdio_pipes in main.rs).
    for (link, target) in [
        ("stdin", "/proc/self/fd/0"),
        ("stdout", "/proc/self/fd/1"),
        ("stderr", "/proc/self/fd/2"),
        ("fd", "/proc/self/fd"),
    ] {
        let path = format!("{dev}/{link}");
        // symlink_metadata does not follow the link, so an existing symlink whose
        // target is not yet resolvable still counts as present (idempotent).
        if std::fs::symlink_metadata(&path).is_ok() {
            continue;
        }
        if let Err(e) = std::os::unix::fs::symlink(target, &path) {
            warn!("Failed to symlink /dev/{link} -> {target}: {e}");
        }
    }
}

/// Parse the container memory limit (bytes) from `A3S_SEC_MEM_LIMIT=<n>`.
/// Returns `None` when unset, zero, or unparseable (no cgroup enforcement).
#[cfg(target_os = "linux")]
fn parse_sec_mem_limit(env: &[String]) -> Option<u64> {
    env.iter()
        .find_map(|entry| entry.strip_prefix("A3S_SEC_MEM_LIMIT="))
        .and_then(|value| value.trim().parse::<u64>().ok())
        .filter(|limit| *limit > 0)
}

/// Parse a signed integer from an `A3S_SEC_*=<n>` env entry (the CPU
/// `cpu_quota`/`cpu_period` cgroup limits). `None` when unset/unparseable.
#[cfg(target_os = "linux")]
fn parse_sec_int(env: &[String], prefix: &str) -> Option<i64> {
    env.iter()
        .find_map(|entry| entry.strip_prefix(prefix))
        .and_then(|value| value.trim().parse::<i64>().ok())
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
        // lstat, not stat: a masked symlink (e.g. busybox's /bin/ls) must be
        // compared as the link entry itself. Following it would resolve to a
        // different filesystem (the guest-root busybox), making st_dev differ
        // from the parent and wrongly reporting "already a mount" — which would
        // skip masking the symlink on the very first exec.
        let Ok(dev) = std::fs::symlink_metadata(target).map(|m| m.dev()) else {
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
        // lstat (no symlink follow): a masked symlink must be treated as a file
        // so we mask the entry itself, not whatever it points at.
        match std::fs::symlink_metadata(&target) {
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
                // Bind /dev/null over the entry WITHOUT following it: open with
                // O_PATH|O_NOFOLLOW and mount onto /proc/self/fd/N so a symlinked
                // target (e.g. busybox's /bin/ls -> /bin/busybox) masks the
                // symlink itself — exec'ing it then yields EACCES — instead of
                // resolving the link against the guest root and masking the wrong
                // file. (Same technique container runtimes use against symlink
                // attacks.)
                use std::os::fd::AsRawFd;
                match nix::fcntl::open(
                    target.as_str(),
                    nix::fcntl::OFlag::O_PATH
                        | nix::fcntl::OFlag::O_NOFOLLOW
                        | nix::fcntl::OFlag::O_CLOEXEC,
                    nix::sys::stat::Mode::empty(),
                ) {
                    Ok(entry_fd) => {
                        let fd_path = format!("/proc/self/fd/{}", entry_fd.as_raw_fd());
                        if let Err(e) = mount(
                            Some("/dev/null"),
                            fd_path.as_str(),
                            None::<&str>,
                            MsFlags::MS_BIND,
                            None::<&str>,
                        ) {
                            warn!("Failed to mask file {target}: {e}");
                        }
                    }
                    Err(e) => warn!("Failed to open masked file {target}: {e}"),
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

/// Remount a container `rootfs` read-only (CRI `readonly_rootfs`).
///
/// Recursively bind-mounts the rootfs onto itself, then remounts the top
/// `MS_RDONLY`, so writes to the container root fail while pseudo-filesystems
/// and volumes mounted inside it (separate mounts) stay writable. Idempotent: a
/// rootfs that is already read-only is left untouched, so re-exec into the
/// container does not stack mounts.
#[cfg(target_os = "linux")]
fn remount_rootfs_readonly(rootfs: &str) {
    use nix::mount::{mount, MsFlags};

    if nix::sys::statvfs::statvfs(rootfs)
        .map(|s| s.flags().contains(nix::sys::statvfs::FsFlags::ST_RDONLY))
        .unwrap_or(false)
    {
        return; // already read-only
    }
    // A fresh bind is read-write regardless of the source, so bind (recursively,
    // to carry the inner pseudo-filesystems) then remount the top read-only.
    if let Err(e) = mount(
        Some(rootfs),
        rootfs,
        None::<&str>,
        MsFlags::MS_BIND | MsFlags::MS_REC,
        None::<&str>,
    ) {
        warn!("Failed to bind rootfs {rootfs} for read-only: {e}");
        return;
    }
    if let Err(e) = mount(
        None::<&str>,
        rootfs,
        None::<&str>,
        MsFlags::MS_BIND | MsFlags::MS_REMOUNT | MsFlags::MS_RDONLY,
        None::<&str>,
    ) {
        warn!("Failed to remount rootfs {rootfs} read-only: {e}");
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
    fn test_drain_exec_input_disconnected_requests_cancel() {
        use std::sync::mpsc;
        // A disconnected host input channel (a3s-box-cri died, or a client
        // disconnect) must be treated as a cancel so the command is killed
        // rather than left running orphaned.
        let mut child = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .unwrap();
        let (tx, rx) = mpsc::channel::<ExecInputEvent>();
        drop(tx); // host gone
        let outcome = drain_exec_input_events(&mut child, Some(&rx));
        assert!(
            outcome.cancel,
            "a disconnected host input channel must request cancel"
        );
        let _ = child.kill();
        let _ = child.wait();
    }

    #[test]
    fn test_drain_exec_input_empty_does_not_cancel() {
        use std::sync::mpsc;
        let mut child = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .unwrap();
        let (_tx, rx) = mpsc::channel::<ExecInputEvent>(); // sender alive, no events
        let outcome = drain_exec_input_events(&mut child, Some(&rx));
        assert!(
            !outcome.cancel,
            "an idle (still-connected) input channel must not cancel"
        );
        let _ = child.kill();
        let _ = child.wait();
    }

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
        let output = build_command(
            ExecCommandSpec {
                cmd: &["id".to_string()],
                timeout_ns: 0,
                env: &[],
                working_dir: None,
                rootfs: None,
                stdin_data: None,
                stdin_streaming: false,
                user: Some("node"),
            },
            None,
        )
        .unwrap_err();

        assert_eq!(output.exit_code, 1);
        assert!(String::from_utf8_lossy(&output.stderr).contains("named user"));
    }

    #[test]
    fn test_build_command_keeps_original_program_with_numeric_user() {
        let (command, _) = build_command(
            ExecCommandSpec {
                cmd: &["echo".to_string(), "hello".to_string()],
                timeout_ns: 0,
                env: &[],
                working_dir: None,
                rootfs: None,
                stdin_data: None,
                stdin_streaming: false,
                user: Some("1000:1000"),
            },
            None,
        )
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
    fn test_execute_command_streaming_flush_emits_ack() {
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
            .send(ExecInputEvent::Stdin(b"echoed".to_vec()))
            .unwrap();
        input_tx.send(ExecInputEvent::Flush).unwrap();
        input_tx.send(ExecInputEvent::StdinClose).unwrap();

        let mut cursor = std::io::Cursor::new(handle.join().unwrap());
        let mut stdout = Vec::new();
        let mut flush_acks = 0;
        let mut exit_code = None;

        while let Some((ft, payload)) = read_frame(&mut cursor).unwrap() {
            match ft {
                ft if ft == FrameType::Data as u8 => {
                    let chunk: ExecChunk = serde_json::from_slice(&payload).unwrap();
                    if chunk.stream == StreamType::Stdout {
                        stdout.extend_from_slice(&chunk.data);
                    }
                }
                ft if ft == FrameType::Control as u8 && payload == EXEC_FLUSH_ACK => {
                    flush_acks += 1;
                }
                ft if ft == FrameType::Control as u8 => {
                    let exit: ExecExit = serde_json::from_slice(&payload).unwrap();
                    exit_code = Some(exit.exit_code);
                }
                other => panic!("unexpected frame type: {other}"),
            }
        }

        // A flush-ack was emitted in response to the Flush event, the echoed
        // output was delivered, and the command still exited cleanly.
        assert!(flush_acks >= 1, "expected at least one flush-ack frame");
        assert_eq!(stdout, b"echoed");
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
