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
use std::sync::Arc;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, UnixStream};
use tokio::sync::RwLock;

/// A pending streaming session registered by a CRI gRPC call.
#[derive(Debug, Clone)]
pub struct StreamingSession {
    /// Type of streaming operation.
    pub kind: SessionKind,
    /// Sandbox ID (for port-forward) or container's sandbox ID.
    pub sandbox_id: String,
    /// Command to execute (exec only).
    pub cmd: Vec<String>,
    /// Whether to allocate a TTY.
    pub tty: bool,
    /// Whether stdin is requested.
    pub stdin: bool,
    /// Ports to forward (port-forward only).
    pub ports: Vec<i32>,
    /// Path to the exec Unix socket for this sandbox's VM.
    pub exec_socket_path: String,
    /// Path to the PTY Unix socket for this sandbox's VM.
    pub pty_socket_path: String,
}

/// Type of CRI streaming session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionKind {
    Exec,
    Attach,
    PortForward,
}

/// CRI streaming server that handles HTTP connections from kubelet.
pub struct StreamingServer {
    /// Listening address.
    addr: SocketAddr,
    /// Pending sessions keyed by token.
    sessions: Arc<RwLock<HashMap<String, StreamingSession>>>,
}

impl StreamingServer {
    /// Create a new streaming server.
    pub fn new(addr: SocketAddr) -> Self {
        Self {
            addr,
            sessions: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Get a handle for registering sessions.
    pub fn handle(&self) -> StreamingHandle {
        StreamingHandle {
            addr: self.addr,
            sessions: self.sessions.clone(),
        }
    }

    /// Start the streaming HTTP server.
    pub async fn serve(self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let listener = TcpListener::bind(self.addr).await?;
        tracing::info!(addr = %self.addr, "CRI streaming server listening");

        loop {
            let (stream, peer) = listener.accept().await?;
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
    addr: SocketAddr,
    sessions: Arc<RwLock<HashMap<String, StreamingSession>>>,
}

impl StreamingHandle {
    /// Register a streaming session and return the URL for kubelet to connect to.
    pub async fn register(&self, session: StreamingSession) -> String {
        let token = uuid::Uuid::new_v4().to_string();
        let kind = match session.kind {
            SessionKind::Exec => "exec",
            SessionKind::Attach => "attach",
            SessionKind::PortForward => "portforward",
        };
        self.sessions.write().await.insert(token.clone(), session);
        format!("http://{}/{}/{}", self.addr, kind, token)
    }
}

/// Handle an incoming HTTP connection from kubelet.
async fn handle_connection(
    mut stream: tokio::net::TcpStream,
    peer: SocketAddr,
    sessions: Arc<RwLock<HashMap<String, StreamingSession>>>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    // Read HTTP request
    let mut buf = vec![0u8; 8192];
    let n = stream.read(&mut buf).await?;
    let request = String::from_utf8_lossy(&buf[..n]);

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

    // Look up and consume the session
    let session = sessions.write().await.remove(token);
    let session = match session {
        Some(s) => s,
        None => {
            send_response(&mut stream, 404, "Session not found or expired").await?;
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
        SessionKind::Exec => handle_exec_stream(&mut stream, &session).await,
        SessionKind::Attach => handle_attach_stream(&mut stream, &session).await,
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
    let exec_req = a3s_box_core::exec::ExecRequest {
        cmd: session.cmd.clone(),
        timeout_ns: a3s_box_core::exec::DEFAULT_EXEC_TIMEOUT_NS,
        env: vec![],
        working_dir: None,
        stdin: None,
        user: None,
        streaming: false,
    };

    let body = serde_json::to_string(&exec_req)?;
    let http_request = format!(
        "POST /exec HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body,
    );

    let mut unix_stream = UnixStream::connect(&session.exec_socket_path).await?;
    unix_stream.write_all(http_request.as_bytes()).await?;

    // Read response from guest
    let mut response = Vec::with_capacity(4096);
    let mut buf = vec![0u8; 65536];
    loop {
        let n = unix_stream.read(&mut buf).await?;
        if n == 0 {
            break;
        }
        response.extend_from_slice(&buf[..n]);
        if response.len() > 33 * 1024 * 1024 {
            break;
        }
    }

    let response_str = String::from_utf8_lossy(&response);
    let body_str = response_str
        .find("\r\n\r\n")
        .map(|pos| &response_str[pos + 4..])
        .unwrap_or("");

    let output: a3s_box_core::exec::ExecOutput =
        serde_json::from_str(body_str).unwrap_or(a3s_box_core::exec::ExecOutput {
            stdout: vec![],
            stderr: b"Failed to parse exec response".to_vec(),
            exit_code: 1,
        });

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

/// Handle attach streaming: connect to the container's main process I/O.
/// For A3S Box, attach is equivalent to exec with the container's entrypoint shell.
async fn handle_attach_stream(
    stream: &mut tokio::net::TcpStream,
    session: &StreamingSession,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    // Use the command from the session if available, otherwise try common shells
    let shell = if !session.cmd.is_empty() {
        session.cmd.clone()
    } else {
        vec!["/bin/sh".to_string(), "-c".to_string(),
             "exec $(command -v bash || command -v sh || echo /bin/sh)".to_string()]
    };

    let attach_session = StreamingSession {
        kind: SessionKind::Attach,
        cmd: shell,
        tty: true,
        ..session.clone()
    };
    handle_pty_stream(stream, &attach_session).await
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

    let port = session.ports[0];

    // Send HTTP 101 Switching Protocols
    let upgrade =
        "HTTP/1.1 101 Switching Protocols\r\nConnection: Upgrade\r\nUpgrade: SPDY/3.1\r\n\r\n";
    stream.write_all(upgrade.as_bytes()).await?;

    // Connect to the guest port via the exec socket (HTTP CONNECT-style)
    // We use the exec server to establish a TCP connection inside the guest.
    // Prefers socat if available, falls back to /bin/sh + /dev/tcp (bash).
    let exec_req = a3s_box_core::exec::ExecRequest {
        cmd: vec![
            "/bin/sh".to_string(),
            "-c".to_string(),
            format!(
                "if command -v socat >/dev/null 2>&1; then \
                   socat STDIO TCP:127.0.0.1:{}; \
                 else \
                   echo 'WARNING: socat not found, using /dev/tcp fallback' >&2; \
                   exec 3<>/dev/tcp/127.0.0.1/{} && cat <&3 & cat >&3; \
                 fi",
                port, port
            ),
        ],
        timeout_ns: 0, // No timeout for port-forward
        env: vec![],
        working_dir: None,
        stdin: None,
        user: None,
        streaming: false,
    };

    let body = serde_json::to_string(&exec_req)?;
    let http_request = format!(
        "POST /exec HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body,
    );

    let mut unix_stream = UnixStream::connect(&session.exec_socket_path).await?;
    unix_stream.write_all(http_request.as_bytes()).await?;

    // Bidirectional copy
    let (mut tcp_read, mut tcp_write) = tokio::io::split(stream);
    let (mut unix_read, mut unix_write) = tokio::io::split(unix_stream);

    let tcp_to_unix = tokio::io::copy(&mut tcp_read, &mut unix_write);
    let unix_to_tcp = tokio::io::copy(&mut unix_read, &mut tcp_write);

    tokio::select! {
        r = tcp_to_unix => { let _ = r; }
        r = unix_to_tcp => { let _ = r; }
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

/// Simple base64 encoding for JSON output.
fn base64_encode(data: &[u8]) -> String {
    const CHARS: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut result = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = if chunk.len() > 1 { chunk[1] as u32 } else { 0 };
        let b2 = if chunk.len() > 2 { chunk[2] as u32 } else { 0 };
        let triple = (b0 << 16) | (b1 << 8) | b2;
        result.push(CHARS[((triple >> 18) & 0x3F) as usize] as char);
        result.push(CHARS[((triple >> 12) & 0x3F) as usize] as char);
        if chunk.len() > 1 {
            result.push(CHARS[((triple >> 6) & 0x3F) as usize] as char);
        } else {
            result.push('=');
        }
        if chunk.len() > 2 {
            result.push(CHARS[(triple & 0x3F) as usize] as char);
        } else {
            result.push('=');
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

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
            tty: false,
            stdin: false,
            ports: vec![],
            exec_socket_path: "/tmp/exec.sock".to_string(),
            pty_socket_path: "/tmp/pty.sock".to_string(),
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
            tty: true,
            stdin: true,
            ports: vec![],
            exec_socket_path: "/tmp/exec.sock".to_string(),
            pty_socket_path: "/tmp/pty.sock".to_string(),
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
            tty: false,
            stdin: false,
            ports: vec![8080, 9090],
            exec_socket_path: "/tmp/exec.sock".to_string(),
            pty_socket_path: "/tmp/pty.sock".to_string(),
        };

        let url = handle.register(session).await;
        assert!(url.contains("/portforward/"));
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
            tty: false,
            stdin: false,
            ports: vec![],
            exec_socket_path: "/tmp/exec.sock".to_string(),
            pty_socket_path: "/tmp/pty.sock".to_string(),
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
}
