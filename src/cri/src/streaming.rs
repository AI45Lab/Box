//! CRI streaming server for exec, attach, and port-forward.
//!
//! Kubernetes CRI uses a two-phase protocol for interactive operations:
//! 1. gRPC call returns a streaming URL
//! 2. Kubelet connects to the URL via HTTP/WebSocket for bidirectional I/O
//!
//! This module implements the HTTP streaming server that bridges kubelet
//! connections to A3S Box's existing exec/PTY infrastructure over vsock.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{TcpListener, UnixStream};
use tokio::sync::{broadcast, RwLock};

pub(crate) const PORT_FORWARD_STREAM_ID: u32 = 1;
pub(crate) const PORT_FORWARD_FRAME_OPEN: u8 = 1;
pub(crate) const PORT_FORWARD_FRAME_OPEN_ACK: u8 = 2;
pub(crate) const PORT_FORWARD_FRAME_DATA: u8 = 3;
pub(crate) const PORT_FORWARD_FRAME_CLOSE: u8 = 4;
const PORT_FORWARD_UNAVAILABLE_MESSAGE: &str =
    "PortForward is not available for this sandbox: no guest port-forward control channel is configured.";
const PORT_FORWARD_CONNECT_FAILED_MESSAGE: &str =
    "Failed to connect to the guest port-forward control channel.";
const PORT_FORWARD_OPEN_FAILED_MESSAGE: &str = "Failed to open the requested guest port.";
const PORT_FORWARD_MULTI_PORT_MESSAGE: &str =
    "PortForward currently supports exactly one port per streaming session.";
const PORT_FORWARD_INVALID_PORT_MESSAGE: &str = "PortForward requested an invalid guest port.";
const DEFAULT_STREAMING_SESSION_TTL: Duration = Duration::from_secs(60);

/// Cloneable stdin sink for a running workload exposed through CRI streaming.
#[derive(Debug, Clone)]
pub enum StreamingInput {
    Exec(a3s_box_runtime::StreamingExecInput),
    Pty(a3s_box_runtime::StreamingPtyInput),
}

impl StreamingInput {
    pub(crate) async fn write_stdin(
        &self,
        data: &[u8],
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        match self {
            Self::Exec(input) => input.write_stdin(data).await?,
            Self::Pty(input) => input.write_stdin(data).await?,
        }
        Ok(())
    }

    pub(crate) async fn close_stdin(&self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        match self {
            Self::Exec(input) => input.close_stdin().await?,
            Self::Pty(input) => input.close().await?,
        }
        Ok(())
    }
}

/// A pending streaming session registered by a CRI gRPC call.
#[derive(Debug, Clone)]
pub struct StreamingSession {
    /// Type of streaming operation.
    pub kind: SessionKind,
    /// Sandbox ID (for port-forward) or container's sandbox ID.
    pub sandbox_id: String,
    /// Command to execute (exec only).
    pub cmd: Vec<String>,
    /// Optional guest-visible rootfs path for container-scoped exec.
    pub rootfs: Option<String>,
    /// Whether to allocate a TTY.
    pub tty: bool,
    /// Whether stdin is requested.
    pub stdin: bool,
    /// Whether stdin should be closed after this attach session disconnects.
    pub stdin_once: bool,
    /// Whether stdout is requested.
    pub stdout: bool,
    /// Whether stderr is requested.
    pub stderr: bool,
    /// Ports to forward (port-forward only).
    pub ports: Vec<i32>,
    /// Running container output source for attach sessions.
    pub attach_stream: Option<broadcast::Sender<a3s_box_core::exec::ExecEvent>>,
    /// Running container stdin sink for attach sessions.
    pub attach_stdin: Option<StreamingInput>,
    /// Path to the exec Unix socket for this sandbox's VM.
    pub exec_socket_path: String,
    /// Path to the PTY Unix socket for this sandbox's VM.
    pub pty_socket_path: String,
    /// Path to the port-forward control Unix socket for this sandbox's VM.
    pub port_forward_socket_path: String,
}

#[derive(Debug, Clone)]
struct PendingStreamingSession {
    session: StreamingSession,
    expires_at: Instant,
}

impl PendingStreamingSession {
    fn new(session: StreamingSession, ttl: Duration) -> Self {
        Self {
            session,
            expires_at: Instant::now() + ttl,
        }
    }

    fn is_expired(&self, now: Instant) -> bool {
        self.expires_at <= now
    }
}

type StreamingSessionStore = Arc<RwLock<HashMap<String, PendingStreamingSession>>>;

fn prune_expired_sessions(sessions: &mut HashMap<String, PendingStreamingSession>, now: Instant) {
    let initial_len = sessions.len();
    sessions.retain(|_, session| !session.is_expired(now));
    let pruned = initial_len - sessions.len();
    if pruned > 0 {
        tracing::debug!(pruned, "Pruned expired CRI streaming sessions");
    }
}

/// Type of CRI streaming session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionKind {
    Exec,
    Attach,
    PortForward,
}

impl SessionKind {
    fn path_segment(self) -> &'static str {
        match self {
            SessionKind::Exec => "exec",
            SessionKind::Attach => "attach",
            SessionKind::PortForward => "portforward",
        }
    }
}

/// CRI streaming server that handles HTTP connections from kubelet.
pub struct StreamingServer {
    /// Address requested by configuration.
    bind_addr: SocketAddr,
    /// Address advertised to kubelet. Updated after binding when port 0 is used.
    advertised_addr: Arc<RwLock<SocketAddr>>,
    /// Pending sessions keyed by token.
    sessions: StreamingSessionStore,
}

impl StreamingServer {
    /// Create a new streaming server.
    pub fn new(addr: SocketAddr) -> Self {
        Self {
            bind_addr: addr,
            advertised_addr: Arc::new(RwLock::new(addr)),
            sessions: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Get a handle for registering sessions.
    pub fn handle(&self) -> StreamingHandle {
        StreamingHandle {
            advertised_addr: self.advertised_addr.clone(),
            sessions: self.sessions.clone(),
        }
    }

    /// Bind the streaming server and update the handle's advertised address.
    pub async fn bind(
        self,
    ) -> Result<BoundStreamingServer, Box<dyn std::error::Error + Send + Sync>> {
        let listener = TcpListener::bind(self.bind_addr).await?;
        let actual_addr = listener.local_addr()?;
        *self.advertised_addr.write().await = actual_addr;

        Ok(BoundStreamingServer {
            listener,
            addr: actual_addr,
            advertised_addr: self.advertised_addr,
            sessions: self.sessions,
        })
    }

    /// Start the streaming HTTP server.
    pub async fn serve(self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        self.bind().await?.serve().await
    }
}

/// A streaming server after its TCP listener has been bound.
pub struct BoundStreamingServer {
    listener: TcpListener,
    addr: SocketAddr,
    advertised_addr: Arc<RwLock<SocketAddr>>,
    sessions: StreamingSessionStore,
}

impl BoundStreamingServer {
    /// Get a handle for registering sessions.
    pub fn handle(&self) -> StreamingHandle {
        StreamingHandle {
            advertised_addr: self.advertised_addr.clone(),
            sessions: self.sessions.clone(),
        }
    }

    /// Start accepting HTTP streaming connections.
    pub async fn serve(self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        tracing::info!(addr = %self.addr, "CRI streaming server listening");

        loop {
            let (stream, peer) = self.listener.accept().await?;
            let sessions = self.sessions.clone();
            tokio::spawn(async move {
                if let Err(e) = handle_connection(stream, peer, sessions).await {
                    tracing::warn!(peer = %peer, error = %e, "Streaming connection failed");
                }
            });
        }
    }
}

/// Handle for registering streaming sessions from the CRI gRPC service.
#[derive(Clone)]
pub struct StreamingHandle {
    advertised_addr: Arc<RwLock<SocketAddr>>,
    sessions: StreamingSessionStore,
}

impl StreamingHandle {
    /// Register a streaming session and return the URL for kubelet to connect to.
    pub async fn register(&self, session: StreamingSession) -> String {
        let token = uuid::Uuid::new_v4().to_string();
        let kind = session.kind.path_segment();
        let addr = *self.advertised_addr.read().await;
        let mut sessions = self.sessions.write().await;
        prune_expired_sessions(&mut sessions, Instant::now());
        sessions.insert(
            token.clone(),
            PendingStreamingSession::new(session, DEFAULT_STREAMING_SESSION_TTL),
        );
        format!("http://{}/{}/{}", addr, kind, token)
    }
}

/// Handle an incoming HTTP connection from kubelet.
async fn handle_connection(
    mut stream: tokio::net::TcpStream,
    peer: SocketAddr,
    sessions: StreamingSessionStore,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    // Read ONLY the HTTP request headers, up to and including the terminating
    // CRLFCRLF. A greedy read would race with the client's pipelined SPDY
    // frames (which it sends right after the upgrade request) and swallow part
    // of the stream, leaving the SPDY handler with a truncated/misaligned frame
    // sequence. The upgrade request carries no body (Content-Length: 0), so
    // everything after the header terminator belongs to the SPDY handler and is
    // left untouched in the socket.
    let mut raw = Vec::with_capacity(1024);
    let mut byte = [0u8; 1];
    loop {
        let n = stream.read(&mut byte).await?;
        if n == 0 {
            break;
        }
        raw.push(byte[0]);
        if raw.ends_with(b"\r\n\r\n") || raw.len() > 16 * 1024 {
            break;
        }
    }
    let request = String::from_utf8_lossy(&raw);
    tracing::debug!(peer = %peer, request = %request, "Streaming request received");
    // crictl/kubelet upgrade exec/attach to the SPDY/3.1 remotecommand protocol.
    let upgrade_spdy = request.to_ascii_lowercase().contains("upgrade: spdy");

    // Parse request line: GET /exec/<token> HTTP/1.1
    let first_line = request.lines().next().unwrap_or("");
    let parts: Vec<&str> = first_line.split_whitespace().collect();
    if parts.len() < 2 {
        send_response(&mut stream, 400, "Bad Request").await?;
        return Ok(());
    }

    let path = parts[1];
    let segments: Vec<&str> = path.trim_start_matches('/').split('/').collect();
    if segments.len() != 2 {
        send_response(&mut stream, 404, "Not Found").await?;
        return Ok(());
    }

    let (kind, token) = (segments[0], segments[1]);

    // Look up and consume the session only when the URL kind matches the
    // registered operation. A typo should not burn a still-valid token.
    let session = {
        let mut sessions = sessions.write().await;
        prune_expired_sessions(&mut sessions, Instant::now());
        match sessions
            .get(token)
            .map(|session| session.session.kind.path_segment() == kind)
        {
            Some(true) => Ok(sessions.remove(token).expect("session must exist").session),
            Some(false) => Err("Session kind mismatch"),
            None => Err("Session not found or expired"),
        }
    };
    let session = match session {
        Ok(s) => s,
        Err(message) => {
            send_response(&mut stream, 404, message).await?;
            return Ok(());
        }
    };

    tracing::info!(
        peer = %peer,
        kind = %kind,
        sandbox_id = %session.sandbox_id,
        "Streaming session started"
    );

    match session.kind {
        // Real clients (crictl, kubelet) always speak SPDY/3.1 remotecommand.
        // The legacy bespoke handler is kept as a fallback for non-SPDY callers.
        SessionKind::Exec if upgrade_spdy => crate::spdy::serve_exec(stream, &session).await,
        SessionKind::Exec => handle_exec_stream(&mut stream, &session).await,
        SessionKind::Attach if upgrade_spdy => crate::spdy::serve_attach(stream, &session).await,
        SessionKind::Attach => handle_attach_stream(&mut stream, &session).await,
        SessionKind::PortForward if upgrade_spdy => {
            crate::spdy::serve_port_forward(stream, &session).await
        }
        SessionKind::PortForward => handle_port_forward_stream(&mut stream, &session).await,
    }
}

/// Handle exec streaming: bridge HTTP connection to guest exec/PTY.
async fn handle_exec_stream(
    stream: &mut tokio::net::TcpStream,
    session: &StreamingSession,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    if session.tty {
        // Interactive TTY: connect to PTY server
        handle_pty_stream(stream, session).await
    } else {
        // Non-interactive: use exec client for one-shot execution
        handle_exec_oneshot(stream, session).await
    }
}

/// Handle non-interactive exec: run command and stream output back.
async fn handle_exec_oneshot(
    stream: &mut tokio::net::TcpStream,
    session: &StreamingSession,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    if session.stdin {
        return handle_exec_stdin_stream(stream, session).await;
    }

    let exec_req = a3s_box_core::exec::ExecRequest {
        cmd: session.cmd.clone(),
        timeout_ns: a3s_box_core::exec::DEFAULT_EXEC_TIMEOUT_NS,
        env: vec![],
        working_dir: None,
        rootfs: session.rootfs.clone(),
        stdin: None,
        stdin_streaming: false,
        user: None,
        streaming: false,
    };

    let client = a3s_box_runtime::ExecClient::connect(Path::new(&session.exec_socket_path)).await?;
    let output = client.exec_command(&exec_req).await?;

    // Send HTTP 200 with output
    let response_body = format!(
        "{{\"exitCode\":{},\"stdout\":\"{}\",\"stderr\":\"{}\"}}",
        output.exit_code,
        base64_encode(&output.stdout),
        base64_encode(&output.stderr),
    );

    let http_response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        response_body.len(),
        response_body,
    );
    stream.write_all(http_response.as_bytes()).await?;

    Ok(())
}

/// Handle non-TTY exec with stdin: bridge HTTP bytes to guest stdin and stream output back.
async fn handle_exec_stdin_stream(
    stream: &mut tokio::net::TcpStream,
    session: &StreamingSession,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let exec_req = a3s_box_core::exec::ExecRequest {
        cmd: session.cmd.clone(),
        timeout_ns: a3s_box_core::exec::DEFAULT_EXEC_TIMEOUT_NS,
        env: vec![],
        working_dir: None,
        rootfs: session.rootfs.clone(),
        stdin: None,
        stdin_streaming: true,
        user: None,
        streaming: false,
    };

    let client = a3s_box_runtime::ExecClient::connect(Path::new(&session.exec_socket_path)).await?;
    let mut exec = client.exec_stream(&exec_req).await?;
    let stdin = exec.input();

    let upgrade =
        "HTTP/1.1 101 Switching Protocols\r\nConnection: Upgrade\r\nUpgrade: SPDY/3.1\r\n\r\n";
    stream.write_all(upgrade.as_bytes()).await?;

    let mut stdin_closed = false;
    let mut input_buffer = vec![0u8; 16 * 1024];
    let (mut tcp_read, mut tcp_write) = tokio::io::split(stream);

    loop {
        tokio::select! {
            input = tcp_read.read(&mut input_buffer), if !stdin_closed => {
                match input {
                    Ok(0) => {
                        stdin_closed = true;
                        stdin.close_stdin().await?;
                    }
                    Ok(n) => {
                        stdin.write_stdin(&input_buffer[..n]).await?;
                    }
                    Err(e) => {
                        let _ = stdin.cancel().await;
                        return Err(Box::new(e));
                    }
                }
            }
            event = exec.next_event() => {
                let event = match event {
                    Ok(event) => event,
                    Err(error) => {
                        let _ = stdin.cancel().await;
                        return Err(Box::new(error));
                    }
                };
                match event {
                    Some(a3s_box_core::exec::ExecEvent::Chunk(chunk)) => match chunk.stream {
                        a3s_box_core::exec::StreamType::Stdout if session.stdout => {
                            if let Err(error) = tcp_write.write_all(&chunk.data).await {
                                let _ = stdin.cancel().await;
                                return Err(Box::new(error));
                            }
                        }
                        a3s_box_core::exec::StreamType::Stderr if session.stderr => {
                            if let Err(error) = tcp_write.write_all(&chunk.data).await {
                                let _ = stdin.cancel().await;
                                return Err(Box::new(error));
                            }
                        }
                        _ => {}
                    },
                    Some(a3s_box_core::exec::ExecEvent::FlushAck) => {}
                    Some(a3s_box_core::exec::ExecEvent::Exit(_)) | None => break,
                }
            }
        }
    }

    if !stdin_closed {
        let _ = stdin.close_stdin().await;
    }

    Ok(())
}

/// Handle interactive PTY exec: bidirectional stream between kubelet and guest PTY.
async fn handle_pty_stream(
    stream: &mut tokio::net::TcpStream,
    session: &StreamingSession,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    // Send HTTP 101 Switching Protocols
    let upgrade =
        "HTTP/1.1 101 Switching Protocols\r\nConnection: Upgrade\r\nUpgrade: SPDY/3.1\r\n\r\n";
    stream.write_all(upgrade.as_bytes()).await?;

    // Connect to guest PTY server
    let mut pty_stream = UnixStream::connect(&session.pty_socket_path).await?;

    // Send PTY request
    let pty_req = a3s_box_core::pty::PtyRequest {
        cmd: session.cmd.clone(),
        env: vec![],
        working_dir: None,
        rootfs: session.rootfs.clone(),
        user: None,
        cols: 80,
        rows: 24,
    };
    let payload = serde_json::to_vec(&pty_req)?;
    write_pty_frame(
        &mut pty_stream,
        a3s_box_core::pty::FRAME_PTY_REQUEST,
        &payload,
    )
    .await?;

    // Bidirectional copy between TCP stream and PTY Unix socket
    let (mut tcp_read, mut tcp_write) = tokio::io::split(stream);
    let (mut pty_read, mut pty_write) = tokio::io::split(pty_stream);

    let tcp_to_pty = async {
        let mut buf = vec![0u8; 4096];
        loop {
            let n = tcp_read.read(&mut buf).await?;
            if n == 0 {
                break;
            }
            // Wrap as PTY data frame
            let len = n as u32;
            pty_write
                .write_all(&[a3s_box_core::pty::FRAME_PTY_DATA])
                .await?;
            pty_write.write_all(&len.to_be_bytes()).await?;
            pty_write.write_all(&buf[..n]).await?;
        }
        Ok::<_, std::io::Error>(())
    };

    let pty_to_tcp = async {
        let mut header = [0u8; 5];
        loop {
            match pty_read.read_exact(&mut header).await {
                Ok(_) => {}
                Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => break,
                Err(e) => return Err(e),
            }
            let frame_type = header[0];
            let len = u32::from_be_bytes([header[1], header[2], header[3], header[4]]) as usize;
            if len > a3s_box_core::pty::MAX_FRAME_PAYLOAD {
                break;
            }
            let mut payload = vec![0u8; len];
            if len > 0 {
                pty_read.read_exact(&mut payload).await?;
            }
            // Forward PTY data to TCP
            if frame_type == a3s_box_core::pty::FRAME_PTY_DATA {
                tcp_write.write_all(&payload).await?;
            }
        }
        Ok(())
    };

    tokio::select! {
        r = tcp_to_pty => { let _ = r; }
        r = pty_to_tcp => { let _ = r; }
    }

    Ok(())
}

/// Handle attach streaming: forward the supervised container workload output.
async fn handle_attach_stream(
    stream: &mut tokio::net::TcpStream,
    session: &StreamingSession,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    if session.stdin && session.attach_stdin.is_none() {
        send_response(
            stream,
            501,
            "Attach stdin is not available for this container: no running workload stdin is registered.",
        )
        .await?;
        return Ok(());
    }

    let Some(attach_stream) = session.attach_stream.as_ref() else {
        send_response(
            stream,
            501,
            "Attach is not available for this container: no running workload stream is registered.",
        )
        .await?;
        return Ok(());
    };

    let upgrade =
        "HTTP/1.1 101 Switching Protocols\r\nConnection: Upgrade\r\nUpgrade: SPDY/3.1\r\n\r\n";
    stream.write_all(upgrade.as_bytes()).await?;

    let mut receiver = attach_stream.subscribe();
    let stdin = session.attach_stdin.clone();
    let mut stdin_closed = !session.stdin;
    let mut input_buffer = vec![0u8; 16 * 1024];
    let (mut tcp_read, mut tcp_write) = tokio::io::split(stream);

    loop {
        tokio::select! {
            input = tcp_read.read(&mut input_buffer), if !stdin_closed => {
                match input {
                    Ok(0) => {
                        stdin_closed = true;
                        if session.stdin_once {
                            if let Some(stdin) = stdin.as_ref() {
                                stdin.close_stdin().await?;
                            }
                        }
                        if !session.stdout && !session.stderr {
                            break;
                        }
                    }
                    Ok(n) => {
                        if let Some(stdin) = stdin.as_ref() {
                            stdin.write_stdin(&input_buffer[..n]).await?;
                        }
                    }
                    Err(e) => return Err(Box::new(e)),
                }
            }
            event = receiver.recv() => {
                match event {
                    Ok(a3s_box_core::exec::ExecEvent::Chunk(chunk)) => match chunk.stream {
                        a3s_box_core::exec::StreamType::Stdout if session.stdout => {
                            tcp_write.write_all(&chunk.data).await?;
                        }
                        a3s_box_core::exec::StreamType::Stderr if session.stderr => {
                            tcp_write.write_all(&chunk.data).await?;
                        }
                        _ => {}
                    },
                    Ok(a3s_box_core::exec::ExecEvent::FlushAck) => {}
                    Ok(a3s_box_core::exec::ExecEvent::Exit(_)) => break,
                    Err(broadcast::error::RecvError::Lagged(skipped)) => {
                        tracing::warn!(
                            sandbox_id = %session.sandbox_id,
                            skipped,
                            "CRI attach stream lagged behind container output"
                        );
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        }
    }

    if session.stdin && session.stdin_once && !stdin_closed {
        if let Some(stdin) = stdin.as_ref() {
            let _ = stdin.close_stdin().await;
        }
    }

    Ok(())
}

/// Handle port-forward streaming: TCP proxy to guest ports.
async fn handle_port_forward_stream(
    stream: &mut tokio::net::TcpStream,
    session: &StreamingSession,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    if session.ports.is_empty() {
        send_response(stream, 400, "No ports specified").await?;
        return Ok(());
    }
    if session.ports.len() != 1 {
        send_response(stream, 400, PORT_FORWARD_MULTI_PORT_MESSAGE).await?;
        return Ok(());
    }

    if session.port_forward_socket_path.is_empty() {
        send_response(stream, 501, PORT_FORWARD_UNAVAILABLE_MESSAGE).await?;
        return Ok(());
    }

    let port = match u16::try_from(session.ports[0]) {
        Ok(port) => port,
        Err(_) => {
            send_response(stream, 400, PORT_FORWARD_INVALID_PORT_MESSAGE).await?;
            return Ok(());
        }
    };
    let mut control = match UnixStream::connect(&session.port_forward_socket_path).await {
        Ok(control) => control,
        Err(error) => {
            tracing::warn!(
                sandbox_id = %session.sandbox_id,
                socket_path = %session.port_forward_socket_path,
                guest_port = port,
                error = %error,
                "Failed to connect to guest port-forward control socket"
            );
            send_response(stream, 502, PORT_FORWARD_CONNECT_FAILED_MESSAGE).await?;
            return Ok(());
        }
    };

    write_port_forward_frame(
        &mut control,
        PORT_FORWARD_FRAME_OPEN,
        PORT_FORWARD_STREAM_ID,
        &port.to_be_bytes(),
    )
    .await?;

    let open_ack = match read_port_forward_frame(&mut control).await? {
        Some(frame) => frame,
        None => {
            send_response(stream, 502, PORT_FORWARD_OPEN_FAILED_MESSAGE).await?;
            return Ok(());
        }
    };

    let open_ok = open_ack.kind == PORT_FORWARD_FRAME_OPEN_ACK
        && open_ack.stream_id == PORT_FORWARD_STREAM_ID
        && open_ack.payload.first().copied().unwrap_or(1) == 0;
    if !open_ok {
        send_response(stream, 502, PORT_FORWARD_OPEN_FAILED_MESSAGE).await?;
        return Ok(());
    }

    let upgrade =
        "HTTP/1.1 101 Switching Protocols\r\nConnection: Upgrade\r\nUpgrade: SPDY/3.1\r\n\r\n";
    stream.write_all(upgrade.as_bytes()).await?;

    let (mut tcp_read, mut tcp_write) = tokio::io::split(stream);
    let (mut control_read, mut control_write) = tokio::io::split(control);

    let tcp_to_guest = async {
        let mut buf = vec![0u8; 4096];
        loop {
            let n = tcp_read.read(&mut buf).await?;
            if n == 0 {
                write_port_forward_frame(
                    &mut control_write,
                    PORT_FORWARD_FRAME_CLOSE,
                    PORT_FORWARD_STREAM_ID,
                    &[],
                )
                .await?;
                break;
            }

            write_port_forward_frame(
                &mut control_write,
                PORT_FORWARD_FRAME_DATA,
                PORT_FORWARD_STREAM_ID,
                &buf[..n],
            )
            .await?;
        }
        Ok::<_, std::io::Error>(())
    };

    let guest_to_tcp = async {
        loop {
            let Some(frame) = read_port_forward_frame(&mut control_read).await? else {
                break;
            };
            if frame.stream_id != PORT_FORWARD_STREAM_ID {
                continue;
            }

            match frame.kind {
                PORT_FORWARD_FRAME_DATA => {
                    tcp_write.write_all(&frame.payload).await?;
                }
                PORT_FORWARD_FRAME_CLOSE => break,
                _ => {}
            }
        }
        Ok::<_, std::io::Error>(())
    };

    tokio::pin!(tcp_to_guest);
    tokio::pin!(guest_to_tcp);

    tokio::select! {
        r = &mut guest_to_tcp => {
            let _ = r;
        }
        r = &mut tcp_to_guest => {
            let _ = r;
            let _ = guest_to_tcp.await;
        }
    }

    Ok(())
}

/// Send a simple HTTP response.
async fn send_response(
    stream: &mut tokio::net::TcpStream,
    status: u16,
    body: &str,
) -> Result<(), std::io::Error> {
    let status_text = match status {
        200 => "OK",
        400 => "Bad Request",
        404 => "Not Found",
        501 => "Not Implemented",
        502 => "Bad Gateway",
        _ => "Error",
    };
    let response = format!(
        "HTTP/1.1 {} {}\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        status, status_text, body.len(), body,
    );
    stream.write_all(response.as_bytes()).await
}

/// Write a PTY frame to a writer.
async fn write_pty_frame(
    stream: &mut UnixStream,
    frame_type: u8,
    payload: &[u8],
) -> Result<(), std::io::Error> {
    let len = payload.len() as u32;
    stream.write_all(&[frame_type]).await?;
    stream.write_all(&len.to_be_bytes()).await?;
    if !payload.is_empty() {
        stream.write_all(payload).await?;
    }
    Ok(())
}

pub(crate) struct PortForwardFrame {
    pub(crate) kind: u8,
    pub(crate) stream_id: u32,
    pub(crate) payload: Vec<u8>,
}

pub(crate) async fn write_port_forward_frame<W>(
    stream: &mut W,
    frame_type: u8,
    stream_id: u32,
    payload: &[u8],
) -> Result<(), std::io::Error>
where
    W: AsyncWrite + Unpin,
{
    stream.write_all(&[frame_type]).await?;
    stream.write_all(&stream_id.to_be_bytes()).await?;
    stream
        .write_all(&(payload.len() as u32).to_be_bytes())
        .await?;
    if !payload.is_empty() {
        stream.write_all(payload).await?;
    }
    stream.flush().await
}

pub(crate) async fn read_port_forward_frame<R>(
    stream: &mut R,
) -> Result<Option<PortForwardFrame>, std::io::Error>
where
    R: AsyncRead + Unpin,
{
    let mut header = [0u8; 9];
    match stream.read_exact(&mut header).await {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }

    let payload_len = u32::from_be_bytes([header[5], header[6], header[7], header[8]]) as usize;
    let mut payload = vec![0u8; payload_len];
    if payload_len > 0 {
        stream.read_exact(&mut payload).await?;
    }

    Ok(Some(PortForwardFrame {
        kind: header[0],
        stream_id: u32::from_be_bytes([header[1], header[2], header[3], header[4]]),
        payload,
    }))
}

/// Base64 encoding for JSON output.
fn base64_encode(data: &[u8]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.encode(data)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::{TcpListener, TcpStream, UnixListener};

    async fn bind_test_tcp_listener(addr: &str) -> Option<TcpListener> {
        match TcpListener::bind(addr).await {
            Ok(listener) => Some(listener),
            Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
                eprintln!("skipping TCP listener test; sandbox denied bind at {addr}: {e}");
                None
            }
            Err(e) => panic!("failed to bind test TCP listener at {addr}: {e}"),
        }
    }

    fn bind_test_exec_listener(path: &Path) -> Option<UnixListener> {
        match UnixListener::bind(path) {
            Ok(listener) => Some(listener),
            Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
                eprintln!(
                    "skipping Unix socket test; sandbox denied bind at {}: {}",
                    path.display(),
                    e
                );
                None
            }
            Err(e) => panic!("failed to bind test socket {}: {}", path.display(), e),
        }
    }

    fn exec_test_session(sandbox_id: &str) -> StreamingSession {
        StreamingSession {
            kind: SessionKind::Exec,
            sandbox_id: sandbox_id.to_string(),
            cmd: vec!["true".to_string()],
            rootfs: None,
            tty: false,
            stdin: false,
            stdin_once: false,
            stdout: true,
            stderr: true,
            attach_stream: None,
            attach_stdin: None,
            ports: vec![],
            exec_socket_path: "/tmp/exec.sock".to_string(),
            pty_socket_path: "/tmp/pty.sock".to_string(),
            port_forward_socket_path: String::new(),
        }
    }

    fn pending_session(session: StreamingSession) -> PendingStreamingSession {
        PendingStreamingSession::new(session, DEFAULT_STREAMING_SESSION_TTL)
    }

    fn expired_pending_session(session: StreamingSession) -> PendingStreamingSession {
        PendingStreamingSession {
            session,
            expires_at: Instant::now()
                .checked_sub(Duration::from_secs(1))
                .unwrap_or_else(Instant::now),
        }
    }

    #[test]
    fn test_base64_encode() {
        assert_eq!(base64_encode(b"hello"), "aGVsbG8=");
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"a"), "YQ==");
        assert_eq!(base64_encode(b"ab"), "YWI=");
        assert_eq!(base64_encode(b"abc"), "YWJj");
    }

    #[test]
    fn test_session_kind_eq() {
        assert_eq!(SessionKind::Exec, SessionKind::Exec);
        assert_ne!(SessionKind::Exec, SessionKind::Attach);
        assert_ne!(SessionKind::Attach, SessionKind::PortForward);
    }

    #[tokio::test]
    async fn test_streaming_handle_register() {
        let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
        let server = StreamingServer::new(addr);
        let handle = server.handle();

        let session = StreamingSession {
            kind: SessionKind::Exec,
            sandbox_id: "sb-1".to_string(),
            cmd: vec!["ls".to_string()],
            rootfs: None,
            tty: false,
            stdin: false,
            stdin_once: false,
            stdout: true,
            stderr: true,
            attach_stream: None,
            attach_stdin: None,
            ports: vec![],
            exec_socket_path: "/tmp/exec.sock".to_string(),
            pty_socket_path: "/tmp/pty.sock".to_string(),
            port_forward_socket_path: String::new(),
        };

        let url = handle.register(session).await;
        assert!(url.contains("/exec/"));
        assert!(url.starts_with("http://"));

        // Session should be in the map
        let sessions = handle.sessions.read().await;
        assert_eq!(sessions.len(), 1);
    }

    #[tokio::test]
    async fn test_streaming_handle_register_attach() {
        let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
        let server = StreamingServer::new(addr);
        let handle = server.handle();

        let session = StreamingSession {
            kind: SessionKind::Attach,
            sandbox_id: "sb-2".to_string(),
            cmd: vec![],
            rootfs: None,
            tty: true,
            stdin: true,
            stdin_once: false,
            stdout: true,
            stderr: true,
            attach_stream: None,
            attach_stdin: None,
            ports: vec![],
            exec_socket_path: "/tmp/exec.sock".to_string(),
            pty_socket_path: "/tmp/pty.sock".to_string(),
            port_forward_socket_path: String::new(),
        };

        let url = handle.register(session).await;
        assert!(url.contains("/attach/"));
    }

    #[tokio::test]
    async fn test_streaming_handle_register_port_forward() {
        let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
        let server = StreamingServer::new(addr);
        let handle = server.handle();

        let session = StreamingSession {
            kind: SessionKind::PortForward,
            sandbox_id: "sb-3".to_string(),
            cmd: vec![],
            rootfs: None,
            tty: false,
            stdin: false,
            stdin_once: false,
            stdout: true,
            stderr: true,
            attach_stream: None,
            attach_stdin: None,
            ports: vec![8080, 9090],
            exec_socket_path: "/tmp/exec.sock".to_string(),
            pty_socket_path: "/tmp/pty.sock".to_string(),
            port_forward_socket_path: "/tmp/portfwd.sock".to_string(),
        };

        let url = handle.register(session).await;
        assert!(url.contains("/portforward/"));
    }

    #[tokio::test]
    async fn test_streaming_handle_uses_bound_addr_for_ephemeral_port() {
        let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
        let server = match StreamingServer::new(addr).bind().await {
            Ok(server) => server,
            Err(error) => {
                eprintln!("skipping TCP bind test; sandbox denied bind: {error}");
                return;
            }
        };
        let bound_addr = server.addr;
        let handle = server.handle();

        let session = StreamingSession {
            kind: SessionKind::Exec,
            sandbox_id: "sb-1".to_string(),
            cmd: vec!["true".to_string()],
            rootfs: None,
            tty: false,
            stdin: false,
            stdin_once: false,
            stdout: true,
            stderr: true,
            attach_stream: None,
            attach_stdin: None,
            ports: vec![],
            exec_socket_path: "/tmp/exec.sock".to_string(),
            pty_socket_path: "/tmp/pty.sock".to_string(),
            port_forward_socket_path: String::new(),
        };

        let url = handle.register(session).await;
        assert!(bound_addr.port() > 0);
        assert!(url.starts_with(&format!("http://{bound_addr}/exec/")));
    }

    #[tokio::test]
    async fn test_streaming_session_consumed_on_use() {
        let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
        let server = StreamingServer::new(addr);
        let handle = server.handle();

        let session = StreamingSession {
            kind: SessionKind::Exec,
            sandbox_id: "sb-1".to_string(),
            cmd: vec!["ls".to_string()],
            rootfs: None,
            tty: false,
            stdin: false,
            stdin_once: false,
            stdout: true,
            stderr: true,
            attach_stream: None,
            attach_stdin: None,
            ports: vec![],
            exec_socket_path: "/tmp/exec.sock".to_string(),
            pty_socket_path: "/tmp/pty.sock".to_string(),
            port_forward_socket_path: String::new(),
        };

        let _url = handle.register(session).await;

        // Simulate consuming the session
        let token = {
            let sessions = handle.sessions.read().await;
            sessions.keys().next().unwrap().clone()
        };
        let consumed = handle.sessions.write().await.remove(&token);
        assert!(consumed.is_some());

        // Second access should return None
        let again = handle.sessions.write().await.remove(&token);
        assert!(again.is_none());
    }

    #[tokio::test]
    async fn test_streaming_handle_register_prunes_expired_sessions() {
        let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
        let server = StreamingServer::new(addr);
        let handle = server.handle();

        handle.sessions.write().await.insert(
            "old-token".to_string(),
            expired_pending_session(exec_test_session("sb-old")),
        );

        let url = handle.register(exec_test_session("sb-new")).await;
        assert!(url.contains("/exec/"));

        let sessions = handle.sessions.read().await;
        assert_eq!(sessions.len(), 1);
        assert!(!sessions.contains_key("old-token"));
    }

    #[tokio::test]
    async fn test_handle_connection_rejects_kind_mismatch_without_consuming_session() {
        let sessions = Arc::new(RwLock::new(HashMap::new()));
        sessions.write().await.insert(
            "tok".to_string(),
            pending_session(StreamingSession {
                kind: SessionKind::Exec,
                sandbox_id: "sb-1".to_string(),
                cmd: vec!["true".to_string()],
                rootfs: None,
                tty: false,
                stdin: false,
                stdin_once: false,
                stdout: true,
                stderr: true,
                attach_stream: None,
                attach_stdin: None,
                ports: vec![],
                exec_socket_path: "/tmp/exec.sock".to_string(),
                pty_socket_path: "/tmp/pty.sock".to_string(),
                port_forward_socket_path: String::new(),
            }),
        );

        let Some(tcp_listener) = bind_test_tcp_listener("127.0.0.1:0").await else {
            return;
        };
        let addr = tcp_listener.local_addr().unwrap();
        let client = tokio::spawn(async move {
            let mut stream = TcpStream::connect(addr).await.unwrap();
            stream
                .write_all(b"GET /portforward/tok HTTP/1.1\r\nHost: localhost\r\n\r\n")
                .await
                .unwrap();
            let mut response = Vec::new();
            stream.read_to_end(&mut response).await.unwrap();
            String::from_utf8(response).unwrap()
        });

        let (server_stream, peer) = tcp_listener.accept().await.unwrap();
        handle_connection(server_stream, peer, sessions.clone())
            .await
            .unwrap();

        let response = client.await.unwrap();
        assert!(response.starts_with("HTTP/1.1 404 Not Found"));
        assert!(response.contains("Session kind mismatch"));
        assert!(sessions.read().await.contains_key("tok"));
    }

    #[tokio::test]
    async fn test_handle_connection_rejects_expired_session_and_removes_it() {
        let sessions = Arc::new(RwLock::new(HashMap::new()));
        sessions.write().await.insert(
            "tok".to_string(),
            expired_pending_session(exec_test_session("sb-expired")),
        );

        let Some(tcp_listener) = bind_test_tcp_listener("127.0.0.1:0").await else {
            return;
        };
        let addr = tcp_listener.local_addr().unwrap();
        let client = tokio::spawn(async move {
            let mut stream = TcpStream::connect(addr).await.unwrap();
            stream
                .write_all(b"GET /exec/tok HTTP/1.1\r\nHost: localhost\r\n\r\n")
                .await
                .unwrap();
            let mut response = Vec::new();
            stream.read_to_end(&mut response).await.unwrap();
            String::from_utf8(response).unwrap()
        });

        let (server_stream, peer) = tcp_listener.accept().await.unwrap();
        handle_connection(server_stream, peer, sessions.clone())
            .await
            .unwrap();

        let response = client.await.unwrap();
        assert!(response.starts_with("HTTP/1.1 404 Not Found"));
        assert!(response.contains("Session not found or expired"));
        assert!(!sessions.read().await.contains_key("tok"));
    }

    #[tokio::test]
    async fn test_handle_exec_oneshot_uses_exec_client_protocol() {
        let tmp = tempfile::TempDir::new().unwrap();
        let sock_path = tmp.path().join("exec.sock");
        let Some(listener) = bind_test_exec_listener(&sock_path) else {
            return;
        };

        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            drop(stream);

            let (stream, _) = listener.accept().await.unwrap();
            let (r, w) = tokio::io::split(stream);
            let mut reader = a3s_transport::FrameReader::new(r);
            let mut writer = a3s_transport::FrameWriter::new(w);

            let frame = reader.read_frame().await.unwrap().unwrap();
            let request: a3s_box_core::exec::ExecRequest =
                serde_json::from_slice(&frame.payload).unwrap();
            assert_eq!(request.cmd, vec!["echo".to_string(), "hello".to_string()]);
            assert_eq!(
                request.rootfs,
                Some("/run/a3s/cri/container-rootfs/sb-1/c-1/rootfs".to_string())
            );
            assert!(!request.streaming);

            let output = a3s_box_core::exec::ExecOutput {
                stdout: b"hello\n".to_vec(),
                stderr: b"warn\n".to_vec(),
                exit_code: 23,
            };
            writer
                .write_data(&serde_json::to_vec(&output).unwrap())
                .await
                .unwrap();
        });

        let tcp_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = tcp_listener.local_addr().unwrap();
        let client = tokio::spawn(async move {
            let mut stream = TcpStream::connect(addr).await.unwrap();
            let mut response = Vec::new();
            stream.read_to_end(&mut response).await.unwrap();
            response
        });

        let (mut server_stream, _) = tcp_listener.accept().await.unwrap();
        let session = StreamingSession {
            kind: SessionKind::Exec,
            sandbox_id: "sb-1".to_string(),
            cmd: vec!["echo".to_string(), "hello".to_string()],
            rootfs: Some("/run/a3s/cri/container-rootfs/sb-1/c-1/rootfs".to_string()),
            tty: false,
            stdin: false,
            stdin_once: false,
            stdout: true,
            stderr: true,
            attach_stream: None,
            attach_stdin: None,
            ports: vec![],
            exec_socket_path: sock_path.to_string_lossy().to_string(),
            pty_socket_path: tmp.path().join("pty.sock").to_string_lossy().to_string(),
            port_forward_socket_path: String::new(),
        };

        handle_exec_oneshot(&mut server_stream, &session)
            .await
            .unwrap();
        drop(server_stream);

        let response = String::from_utf8(client.await.unwrap()).unwrap();
        assert!(response.starts_with("HTTP/1.1 200 OK"));
        assert!(response.contains("\"exitCode\":23"));
        assert!(response.contains(&base64_encode(b"hello\n")));
        assert!(response.contains(&base64_encode(b"warn\n")));
    }

    #[tokio::test]
    async fn test_handle_exec_oneshot_forwards_stdin_when_requested() {
        let tmp = tempfile::TempDir::new().unwrap();
        let sock_path = tmp.path().join("exec_stdin.sock");
        let Some(listener) = bind_test_exec_listener(&sock_path) else {
            return;
        };

        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            drop(stream);

            let (stream, _) = listener.accept().await.unwrap();
            let (r, w) = tokio::io::split(stream);
            let mut reader = a3s_transport::FrameReader::new(r);
            let mut writer = a3s_transport::FrameWriter::new(w);

            let frame = reader.read_frame().await.unwrap().unwrap();
            let request: a3s_box_core::exec::ExecRequest =
                serde_json::from_slice(&frame.payload).unwrap();
            assert_eq!(request.cmd, vec!["cat".to_string()]);
            assert!(request.streaming);
            assert!(request.stdin_streaming);

            let stdin = reader.read_frame().await.unwrap().unwrap();
            assert_eq!(stdin.frame_type, a3s_transport::FrameType::Data);
            assert_eq!(stdin.payload, b"stdin from exec");

            let close = reader.read_frame().await.unwrap().unwrap();
            assert_eq!(close.frame_type, a3s_transport::FrameType::Control);
            assert_eq!(close.payload, b"stdin-close");

            let stdout = a3s_box_core::exec::ExecChunk {
                stream: a3s_box_core::exec::StreamType::Stdout,
                data: b"echoed stdin\n".to_vec(),
            };
            writer
                .write_data(&serde_json::to_vec(&stdout).unwrap())
                .await
                .unwrap();
            let exit = a3s_box_core::exec::ExecExit {
                exit_code: 0,
                oom_killed: false,
            };
            writer
                .write_control(&serde_json::to_vec(&exit).unwrap())
                .await
                .unwrap();
        });

        let tcp_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = tcp_listener.local_addr().unwrap();
        let client = tokio::spawn(async move {
            let mut stream = TcpStream::connect(addr).await.unwrap();
            let mut response = Vec::new();
            let mut buf = [0u8; 1024];
            while !response.windows(4).any(|window| window == b"\r\n\r\n") {
                let n = stream.read(&mut buf).await.unwrap();
                assert!(n > 0);
                response.extend_from_slice(&buf[..n]);
            }
            stream.write_all(b"stdin from exec").await.unwrap();
            stream.shutdown().await.unwrap();
            stream.read_to_end(&mut response).await.unwrap();
            String::from_utf8(response).unwrap()
        });

        let (mut server_stream, _) = tcp_listener.accept().await.unwrap();
        let session = StreamingSession {
            kind: SessionKind::Exec,
            sandbox_id: "sb-1".to_string(),
            cmd: vec!["cat".to_string()],
            rootfs: Some("/run/a3s/cri/container-rootfs/sb-1/c-1/rootfs".to_string()),
            tty: false,
            stdin: true,
            stdin_once: false,
            stdout: true,
            stderr: false,
            attach_stream: None,
            attach_stdin: None,
            ports: vec![],
            exec_socket_path: sock_path.to_string_lossy().to_string(),
            pty_socket_path: tmp.path().join("pty.sock").to_string_lossy().to_string(),
            port_forward_socket_path: String::new(),
        };

        handle_exec_oneshot(&mut server_stream, &session)
            .await
            .unwrap();
        drop(server_stream);

        let response = client.await.unwrap();
        assert!(response.starts_with("HTTP/1.1 101 Switching Protocols"));
        assert!(response.contains("echoed stdin"));
    }

    #[tokio::test]
    async fn test_handle_pty_stream_forwards_rootfs() {
        let tmp = tempfile::TempDir::new().unwrap();
        let sock_path = tmp.path().join("pty.sock");
        let Some(listener) = bind_test_exec_listener(&sock_path) else {
            return;
        };

        let guest = tokio::spawn(async move {
            let (mut pty_stream, _) = listener.accept().await.unwrap();
            let mut header = [0u8; 5];
            pty_stream.read_exact(&mut header).await.unwrap();
            assert_eq!(header[0], a3s_box_core::pty::FRAME_PTY_REQUEST);

            let len = u32::from_be_bytes([header[1], header[2], header[3], header[4]]) as usize;
            let mut payload = vec![0u8; len];
            pty_stream.read_exact(&mut payload).await.unwrap();
            let request: a3s_box_core::pty::PtyRequest = serde_json::from_slice(&payload).unwrap();

            assert_eq!(request.cmd, vec!["/bin/sh".to_string()]);
            assert_eq!(
                request.rootfs,
                Some("/run/a3s/cri/container-rootfs/sb-1/c-1/rootfs".to_string())
            );
        });

        let tcp_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = tcp_listener.local_addr().unwrap();
        let client = tokio::spawn(async move {
            let mut stream = TcpStream::connect(addr).await.unwrap();
            let mut response = [0u8; 64];
            let _ = stream.read(&mut response).await.unwrap();
            stream.shutdown().await.unwrap();
        });

        let (mut server_stream, _) = tcp_listener.accept().await.unwrap();
        let session = StreamingSession {
            kind: SessionKind::Exec,
            sandbox_id: "sb-1".to_string(),
            cmd: vec!["/bin/sh".to_string()],
            rootfs: Some("/run/a3s/cri/container-rootfs/sb-1/c-1/rootfs".to_string()),
            tty: true,
            stdin: true,
            stdin_once: false,
            stdout: true,
            stderr: true,
            attach_stream: None,
            attach_stdin: None,
            ports: vec![],
            exec_socket_path: String::new(),
            pty_socket_path: sock_path.to_string_lossy().to_string(),
            port_forward_socket_path: String::new(),
        };

        tokio::time::timeout(
            Duration::from_secs(1),
            handle_pty_stream(&mut server_stream, &session),
        )
        .await
        .unwrap()
        .unwrap();
        client.await.unwrap();
        guest.await.unwrap();
    }

    #[tokio::test]
    async fn test_handle_attach_stream_forwards_running_workload_output() {
        let Some(tcp_listener) = bind_test_tcp_listener("127.0.0.1:0").await else {
            return;
        };
        let addr = tcp_listener.local_addr().unwrap();
        let (attach_tx, _) = broadcast::channel(16);
        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();

        let client = tokio::spawn(async move {
            let mut stream = TcpStream::connect(addr).await.unwrap();
            let mut response = Vec::new();
            let mut buf = [0u8; 1024];
            while !response.windows(4).any(|window| window == b"\r\n\r\n") {
                let n = stream.read(&mut buf).await.unwrap();
                assert!(n > 0);
                response.extend_from_slice(&buf[..n]);
            }
            ready_tx.send(()).unwrap();
            stream.read_to_end(&mut response).await.unwrap();
            String::from_utf8(response).unwrap()
        });

        let (mut server_stream, _) = tcp_listener.accept().await.unwrap();
        let session = StreamingSession {
            kind: SessionKind::Attach,
            sandbox_id: "sb-attach".to_string(),
            cmd: vec![],
            rootfs: None,
            tty: false,
            stdin: false,
            stdin_once: false,
            stdout: true,
            stderr: false,
            attach_stream: Some(attach_tx.clone()),
            attach_stdin: None,
            ports: vec![],
            exec_socket_path: String::new(),
            pty_socket_path: String::new(),
            port_forward_socket_path: String::new(),
        };
        let server = tokio::spawn(async move {
            handle_attach_stream(&mut server_stream, &session)
                .await
                .unwrap();
        });

        ready_rx.await.unwrap();
        attach_tx
            .send(a3s_box_core::exec::ExecEvent::Chunk(
                a3s_box_core::exec::ExecChunk {
                    stream: a3s_box_core::exec::StreamType::Stdout,
                    data: b"hello attach\n".to_vec(),
                },
            ))
            .unwrap();
        attach_tx
            .send(a3s_box_core::exec::ExecEvent::Chunk(
                a3s_box_core::exec::ExecChunk {
                    stream: a3s_box_core::exec::StreamType::Stderr,
                    data: b"filtered stderr\n".to_vec(),
                },
            ))
            .unwrap();
        attach_tx
            .send(a3s_box_core::exec::ExecEvent::Exit(
                a3s_box_core::exec::ExecExit {
                    exit_code: 0,
                    oom_killed: false,
                },
            ))
            .unwrap();

        let response = client.await.unwrap();
        assert!(response.starts_with("HTTP/1.1 101 Switching Protocols"));
        assert!(response.contains("hello attach"));
        assert!(!response.contains("filtered stderr"));
        server.await.unwrap();
    }

    #[tokio::test]
    async fn test_handle_attach_stream_forwards_stdin_and_closes_once() {
        let tmp = tempfile::TempDir::new().unwrap();
        let sock_path = tmp.path().join("exec.sock");
        let Some(exec_listener) = bind_test_exec_listener(&sock_path) else {
            return;
        };
        let (stdin_seen_tx, stdin_seen_rx) = tokio::sync::oneshot::channel();

        tokio::spawn(async move {
            let (stream, _) = exec_listener.accept().await.unwrap();
            drop(stream);

            let (stream, _) = exec_listener.accept().await.unwrap();
            let (r, w) = tokio::io::split(stream);
            let mut reader = a3s_transport::FrameReader::new(r);
            let mut writer = a3s_transport::FrameWriter::new(w);

            let frame = reader.read_frame().await.unwrap().unwrap();
            let request: a3s_box_core::exec::ExecRequest =
                serde_json::from_slice(&frame.payload).unwrap();
            assert!(request.streaming);
            assert!(request.stdin_streaming);

            let stdin = reader.read_frame().await.unwrap().unwrap();
            assert_eq!(stdin.frame_type, a3s_transport::FrameType::Data);
            let close = reader.read_frame().await.unwrap().unwrap();
            assert_eq!(close.frame_type, a3s_transport::FrameType::Control);
            stdin_seen_tx.send((stdin.payload, close.payload)).unwrap();

            let exit = a3s_box_core::exec::ExecExit {
                exit_code: 0,
                oom_killed: false,
            };
            writer
                .write_control(&serde_json::to_vec(&exit).unwrap())
                .await
                .unwrap();
        });

        let exec_client = a3s_box_runtime::ExecClient::connect(&sock_path)
            .await
            .unwrap();
        let exec_req = a3s_box_core::exec::ExecRequest {
            cmd: vec!["cat".to_string()],
            timeout_ns: a3s_box_core::exec::DEFAULT_EXEC_TIMEOUT_NS,
            env: vec![],
            working_dir: None,
            rootfs: None,
            stdin: None,
            stdin_streaming: true,
            user: None,
            streaming: false,
        };
        let stream_guard = exec_client.exec_stream(&exec_req).await.unwrap();
        let stdin_handle = stream_guard.input();

        let Some(tcp_listener) = bind_test_tcp_listener("127.0.0.1:0").await else {
            return;
        };
        let addr = tcp_listener.local_addr().unwrap();
        let (attach_tx, _) = broadcast::channel(16);

        let client = tokio::spawn(async move {
            let mut stream = TcpStream::connect(addr).await.unwrap();
            let mut response = Vec::new();
            let mut buf = [0u8; 1024];
            while !response.windows(4).any(|window| window == b"\r\n\r\n") {
                let n = stream.read(&mut buf).await.unwrap();
                assert!(n > 0);
                response.extend_from_slice(&buf[..n]);
            }
            stream.write_all(b"stdin from attach").await.unwrap();
            stream.shutdown().await.unwrap();
            stream.read_to_end(&mut response).await.unwrap();
            String::from_utf8(response).unwrap()
        });

        let (mut server_stream, _) = tcp_listener.accept().await.unwrap();
        let session = StreamingSession {
            kind: SessionKind::Attach,
            sandbox_id: "sb-attach".to_string(),
            cmd: vec![],
            rootfs: None,
            tty: false,
            stdin: true,
            stdin_once: true,
            stdout: true,
            stderr: true,
            attach_stream: Some(attach_tx.clone()),
            attach_stdin: Some(StreamingInput::Exec(stdin_handle)),
            ports: vec![],
            exec_socket_path: String::new(),
            pty_socket_path: String::new(),
            port_forward_socket_path: String::new(),
        };
        let server = tokio::spawn(async move {
            handle_attach_stream(&mut server_stream, &session)
                .await
                .unwrap();
        });

        let (stdin_payload, close_payload) = stdin_seen_rx.await.unwrap();
        assert_eq!(stdin_payload, b"stdin from attach");
        assert_eq!(close_payload, b"stdin-close");
        attach_tx
            .send(a3s_box_core::exec::ExecEvent::Exit(
                a3s_box_core::exec::ExecExit {
                    exit_code: 0,
                    oom_killed: false,
                },
            ))
            .unwrap();

        let response = client.await.unwrap();
        assert!(response.starts_with("HTTP/1.1 101 Switching Protocols"));
        server.await.unwrap();
        drop(stream_guard);
    }

    #[tokio::test]
    async fn test_handle_attach_stream_without_workload_returns_501() {
        let Some(tcp_listener) = bind_test_tcp_listener("127.0.0.1:0").await else {
            return;
        };
        let addr = tcp_listener.local_addr().unwrap();
        let client = tokio::spawn(async move {
            let mut stream = TcpStream::connect(addr).await.unwrap();
            let mut response = Vec::new();
            stream.read_to_end(&mut response).await.unwrap();
            String::from_utf8(response).unwrap()
        });

        let (mut server_stream, _) = tcp_listener.accept().await.unwrap();
        let session = StreamingSession {
            kind: SessionKind::Attach,
            sandbox_id: "sb-attach".to_string(),
            cmd: vec![],
            rootfs: None,
            tty: false,
            stdin: false,
            stdin_once: false,
            stdout: true,
            stderr: true,
            attach_stream: None,
            attach_stdin: None,
            ports: vec![],
            exec_socket_path: String::new(),
            pty_socket_path: String::new(),
            port_forward_socket_path: String::new(),
        };

        handle_attach_stream(&mut server_stream, &session)
            .await
            .unwrap();
        drop(server_stream);

        let response = client.await.unwrap();
        assert!(response.starts_with("HTTP/1.1 501 Not Implemented"));
        assert!(response.contains("no running workload stream is registered"));
    }

    #[tokio::test]
    async fn test_handle_port_forward_stream_bridges_guest_control_socket() {
        let tmp = tempfile::TempDir::new().unwrap();
        let sock_path = tmp.path().join("portfwd.sock");
        let Some(listener) = bind_test_exec_listener(&sock_path) else {
            return;
        };

        let guest = tokio::spawn(async move {
            let (mut control, _) = listener.accept().await.unwrap();

            let open = read_port_forward_frame(&mut control)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(open.kind, PORT_FORWARD_FRAME_OPEN);
            assert_eq!(open.stream_id, PORT_FORWARD_STREAM_ID);
            assert_eq!(open.payload, 8080u16.to_be_bytes());

            write_port_forward_frame(
                &mut control,
                PORT_FORWARD_FRAME_OPEN_ACK,
                PORT_FORWARD_STREAM_ID,
                &[0],
            )
            .await
            .unwrap();
            write_port_forward_frame(
                &mut control,
                PORT_FORWARD_FRAME_DATA,
                PORT_FORWARD_STREAM_ID,
                b"guest->host",
            )
            .await
            .unwrap();

            let host_data = read_port_forward_frame(&mut control)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(host_data.kind, PORT_FORWARD_FRAME_DATA);
            assert_eq!(host_data.stream_id, PORT_FORWARD_STREAM_ID);
            assert_eq!(host_data.payload, b"host->guest");

            let close = read_port_forward_frame(&mut control)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(close.kind, PORT_FORWARD_FRAME_CLOSE);
        });

        let Some(tcp_listener) = bind_test_tcp_listener("127.0.0.1:0").await else {
            return;
        };
        let addr = tcp_listener.local_addr().unwrap();
        let client = tokio::spawn(async move {
            let mut stream = TcpStream::connect(addr).await.unwrap();
            stream.write_all(b"host->guest").await.unwrap();
            stream.shutdown().await.unwrap();
            let mut response = Vec::new();
            stream.read_to_end(&mut response).await.unwrap();
            String::from_utf8(response).unwrap()
        });

        let (mut server_stream, _) = tcp_listener.accept().await.unwrap();
        let session = StreamingSession {
            kind: SessionKind::PortForward,
            sandbox_id: "sb-3".to_string(),
            cmd: vec![],
            rootfs: None,
            tty: false,
            stdin: false,
            stdin_once: false,
            stdout: true,
            stderr: true,
            attach_stream: None,
            attach_stdin: None,
            ports: vec![8080],
            exec_socket_path: String::new(),
            pty_socket_path: String::new(),
            port_forward_socket_path: sock_path.to_string_lossy().to_string(),
        };

        handle_port_forward_stream(&mut server_stream, &session)
            .await
            .unwrap();
        drop(server_stream);

        let response = client.await.unwrap();
        assert!(response.starts_with("HTTP/1.1 101 Switching Protocols"));
        assert!(response.contains("guest->host"));
        guest.await.unwrap();
    }

    #[tokio::test]
    async fn test_handle_connection_routes_port_forward_stream() {
        let tmp = tempfile::TempDir::new().unwrap();
        let sock_path = tmp.path().join("portfwd.sock");
        let Some(listener) = bind_test_exec_listener(&sock_path) else {
            return;
        };

        let guest = tokio::spawn(async move {
            let (mut control, _) = listener.accept().await.unwrap();
            let open = read_port_forward_frame(&mut control)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(open.kind, PORT_FORWARD_FRAME_OPEN);
            assert_eq!(open.payload, 8080u16.to_be_bytes());

            write_port_forward_frame(
                &mut control,
                PORT_FORWARD_FRAME_OPEN_ACK,
                PORT_FORWARD_STREAM_ID,
                &[0],
            )
            .await
            .unwrap();

            let host_data = read_port_forward_frame(&mut control)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(host_data.kind, PORT_FORWARD_FRAME_DATA);
            assert_eq!(host_data.payload, b"host->guest");

            let close = read_port_forward_frame(&mut control)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(close.kind, PORT_FORWARD_FRAME_CLOSE);

            write_port_forward_frame(
                &mut control,
                PORT_FORWARD_FRAME_DATA,
                PORT_FORWARD_STREAM_ID,
                b"guest->host",
            )
            .await
            .unwrap();
            write_port_forward_frame(
                &mut control,
                PORT_FORWARD_FRAME_CLOSE,
                PORT_FORWARD_STREAM_ID,
                &[],
            )
            .await
            .unwrap();
        });

        let sessions = Arc::new(RwLock::new(HashMap::new()));
        sessions.write().await.insert(
            "tok".to_string(),
            pending_session(StreamingSession {
                kind: SessionKind::PortForward,
                sandbox_id: "sb-1".to_string(),
                cmd: vec![],
                rootfs: None,
                tty: false,
                stdin: false,
                stdin_once: false,
                stdout: true,
                stderr: true,
                attach_stream: None,
                attach_stdin: None,
                ports: vec![8080],
                exec_socket_path: String::new(),
                pty_socket_path: String::new(),
                port_forward_socket_path: sock_path.to_string_lossy().to_string(),
            }),
        );

        let Some(tcp_listener) = bind_test_tcp_listener("127.0.0.1:0").await else {
            return;
        };
        let addr = tcp_listener.local_addr().unwrap();
        let client = tokio::spawn(async move {
            let mut stream = TcpStream::connect(addr).await.unwrap();
            stream
                .write_all(b"GET /portforward/tok HTTP/1.1\r\nHost: localhost\r\n\r\n")
                .await
                .unwrap();

            let mut response = Vec::new();
            let mut buf = [0u8; 1024];
            while !response.windows(4).any(|window| window == b"\r\n\r\n") {
                let n = stream.read(&mut buf).await.unwrap();
                assert!(n > 0);
                response.extend_from_slice(&buf[..n]);
            }

            stream.write_all(b"host->guest").await.unwrap();
            stream.shutdown().await.unwrap();
            stream.read_to_end(&mut response).await.unwrap();
            String::from_utf8(response).unwrap()
        });

        let (server_stream, peer) = tcp_listener.accept().await.unwrap();
        handle_connection(server_stream, peer, sessions.clone())
            .await
            .unwrap();

        let response = client.await.unwrap();
        assert!(response.starts_with("HTTP/1.1 101 Switching Protocols"));
        assert!(response.contains("guest->host"));
        assert!(!sessions.read().await.contains_key("tok"));
        guest.await.unwrap();
    }

    #[tokio::test]
    async fn test_handle_port_forward_stream_without_control_socket_returns_501() {
        let Some(tcp_listener) = bind_test_tcp_listener("127.0.0.1:0").await else {
            return;
        };
        let addr = tcp_listener.local_addr().unwrap();
        let client = tokio::spawn(async move {
            let mut stream = TcpStream::connect(addr).await.unwrap();
            let mut response = Vec::new();
            stream.read_to_end(&mut response).await.unwrap();
            String::from_utf8(response).unwrap()
        });

        let (mut server_stream, _) = tcp_listener.accept().await.unwrap();
        let session = StreamingSession {
            kind: SessionKind::PortForward,
            sandbox_id: "sb-3".to_string(),
            cmd: vec![],
            rootfs: None,
            tty: false,
            stdin: false,
            stdin_once: false,
            stdout: true,
            stderr: true,
            attach_stream: None,
            attach_stdin: None,
            ports: vec![8080],
            exec_socket_path: String::new(),
            pty_socket_path: String::new(),
            port_forward_socket_path: String::new(),
        };

        handle_port_forward_stream(&mut server_stream, &session)
            .await
            .unwrap();
        drop(server_stream);

        let response = client.await.unwrap();
        assert!(response.starts_with("HTTP/1.1 501 Not Implemented"));
        assert!(response.contains(PORT_FORWARD_UNAVAILABLE_MESSAGE));
    }
}
