//! Minimal SPDY/3.1 server for the Kubernetes `remotecommand` (exec/attach)
//! streaming protocol.
//!
//! `crictl` and the kubelet do not exec over a plain HTTP body or WebSocket —
//! they upgrade the connection to `SPDY/3.1` and multiplex the
//! error/stdin/stdout/stderr/resize channels as separate SPDY streams
//! (`X-Stream-Protocol-Version: v4.channel.k8s.io`). This module implements just
//! enough of SPDY/3.1 to serve that protocol.
//!
//! SPDY normally compresses `SYN_STREAM`/`SYN_REPLY` header blocks with a
//! stateful zlib stream seeded by the SPDY/3 dictionary. We sidestep that
//! entirely: the client opens streams in a fixed order (error, stdin, stdout,
//! stderr, resize), so we map each `SYN_STREAM` to a channel by open-order and
//! **skip** its compressed header block. We never send `SYN_REPLY` or any other
//! header-bearing control frame — the remotecommand client delivers and reads
//! stream data without waiting for a reply — so the server only ever emits
//! plaintext `DATA` frames. This keeps the implementation dependency-free (no
//! zlib dictionary support is available in `flate2`).
//!
//! Flow control (`WINDOW_UPDATE`) is intentionally ignored: exec output is
//! small relative to the 64 KiB default window, and the client grows the window
//! as it reads. This is revisited only if large-output conformance fails.

use std::collections::HashMap;
use std::path::Path;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use a3s_box_core::exec::{ExecEvent, StreamType as ExecStream};
use flate2::{Compress, Compression, FlushCompress, Status};

use crate::streaming::StreamingSession;

type DynError = Box<dyn std::error::Error + Send + Sync>;

const CTRL_SYN_STREAM: u16 = 1;
const CTRL_SYN_REPLY: u16 = 2;
const CTRL_PING: u16 = 6;
const FLAG_FIN: u8 = 0x01;

/// Continuous zlib compressor for SPDY control-frame header blocks.
///
/// SPDY normally seeds this with the SPDY/3 dictionary, but we deliberately use
/// plain zlib (no dictionary): the client decompresses our stream without ever
/// hitting `Z_NEED_DICT`, and we never have to compress the client's
/// dictionary-seeded headers because we map its streams by open-order instead
/// of reading them. This keeps everything within `flate2` (no zlib dictionary
/// API is available there).
struct HeaderCompressor {
    deflate: Compress,
}

impl HeaderCompressor {
    fn new() -> Self {
        Self {
            deflate: Compress::new(Compression::fast(), true),
        }
    }

    /// Compress one header block, flushing so the bytes are usable immediately
    /// while keeping the zlib stream continuous across frames.
    fn compress(&mut self, input: &[u8]) -> Vec<u8> {
        let mut output = Vec::with_capacity(input.len() + 64);
        let mut in_pos = 0usize;
        let mut buf = [0u8; 4096];
        loop {
            let in_before = self.deflate.total_in();
            let out_before = self.deflate.total_out();
            let status = self
                .deflate
                .compress(&input[in_pos..], &mut buf, FlushCompress::Sync)
                .expect("zlib deflate of SPDY header block");
            in_pos += (self.deflate.total_in() - in_before) as usize;
            let produced = (self.deflate.total_out() - out_before) as usize;
            output.extend_from_slice(&buf[..produced]);
            // `Sync` re-emits a sync marker on every call, so `produced` is never
            // zero — terminate once all input is consumed and the flush stopped
            // filling the whole buffer (nothing left pending). Guarding on
            // `produced == 0` here would busy-loop forever.
            if matches!(status, Status::StreamEnd)
                || (in_pos >= input.len() && produced < buf.len())
            {
                break;
            }
        }
        output
    }
}

/// Build a SPDY/3.1 `SYN_REPLY` that accepts a client-created stream. The
/// remotecommand client blocks (`stream.Wait()`) until each stream it opens is
/// replied to before opening the next, so we must answer every `SYN_STREAM`.
/// The header block carries zero name/value pairs.
fn syn_reply_frame(compressor: &mut HeaderCompressor, stream_id: u32) -> Vec<u8> {
    let header_block = compressor.compress(&0u32.to_be_bytes());
    let mut data = Vec::with_capacity(4 + header_block.len());
    data.extend_from_slice(&(stream_id & 0x7fff_ffff).to_be_bytes());
    data.extend_from_slice(&header_block);

    let mut frame = vec![0x80, 0x03];
    frame.extend_from_slice(&CTRL_SYN_REPLY.to_be_bytes());
    frame.push(0); // flags
    let len = data.len() as u32;
    frame.push((len >> 16) as u8);
    frame.push((len >> 8) as u8);
    frame.push(len as u8);
    frame.extend_from_slice(&data);
    frame
}

/// 101 response that upgrades the connection to SPDY and pins the v4
/// remotecommand subprotocol (exit codes are carried on the error stream).
const UPGRADE_RESPONSE: &str = "HTTP/1.1 101 Switching Protocols\r\n\
Connection: Upgrade\r\n\
Upgrade: SPDY/3.1\r\n\
X-Stream-Protocol-Version: v4.channel.k8s.io\r\n\r\n";

/// Logical remotecommand channels, in the order the client opens them.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
enum Channel {
    Error,
    Stdin,
    Stdout,
    Stderr,
    Resize,
}

/// A decoded SPDY frame (only the parts we act on).
enum Frame {
    /// Client opened a new stream; the compressed header block is skipped.
    SynStream { stream_id: u32 },
    /// Stream data (or a half-close when `data` is empty and `fin` is set).
    Data {
        stream_id: u32,
        fin: bool,
        data: Vec<u8>,
    },
    /// PING control frame; echoed back to keep the connection alive.
    Ping { id: u32 },
    /// SETTINGS / WINDOW_UPDATE / RST_STREAM / GOAWAY / HEADERS — ignored.
    Ignored,
}

/// Read one SPDY frame. Returns `Ok(None)` at clean end-of-stream.
async fn read_frame<R: AsyncReadExt + Unpin>(reader: &mut R) -> std::io::Result<Option<Frame>> {
    let mut hdr = [0u8; 8];
    if let Err(error) = reader.read_exact(&mut hdr).await {
        if error.kind() == std::io::ErrorKind::UnexpectedEof {
            return Ok(None);
        }
        return Err(error);
    }
    tracing::trace!(hdr = ?hdr, "spdy read_frame: header");
    let flags = hdr[4];
    let len = ((hdr[5] as usize) << 16) | ((hdr[6] as usize) << 8) | (hdr[7] as usize);
    let mut data = vec![0u8; len];
    if len > 0 {
        reader.read_exact(&mut data).await?;
    }

    if hdr[0] & 0x80 != 0 {
        // Control frame: hdr[2..4] is the type.
        let frame_type = u16::from_be_bytes([hdr[2], hdr[3]]);
        match frame_type {
            CTRL_SYN_STREAM if data.len() >= 4 => {
                let stream_id = u32::from_be_bytes([data[0] & 0x7f, data[1], data[2], data[3]]);
                Ok(Some(Frame::SynStream { stream_id }))
            }
            CTRL_PING if data.len() >= 4 => {
                let id = u32::from_be_bytes([data[0], data[1], data[2], data[3]]);
                Ok(Some(Frame::Ping { id }))
            }
            _ => Ok(Some(Frame::Ignored)),
        }
    } else {
        // Data frame: hdr[0..4] is the stream id (control bit already 0).
        let stream_id = u32::from_be_bytes([hdr[0] & 0x7f, hdr[1], hdr[2], hdr[3]]);
        Ok(Some(Frame::Data {
            stream_id,
            fin: flags & FLAG_FIN != 0,
            data,
        }))
    }
}

/// Serialize a SPDY DATA frame.
fn data_frame(stream_id: u32, fin: bool, payload: &[u8]) -> Vec<u8> {
    let mut frame = Vec::with_capacity(8 + payload.len());
    frame.extend_from_slice(&(stream_id & 0x7fff_ffff).to_be_bytes());
    frame.push(if fin { FLAG_FIN } else { 0 });
    let len = payload.len() as u32;
    frame.push((len >> 16) as u8);
    frame.push((len >> 8) as u8);
    frame.push(len as u8);
    frame.extend_from_slice(payload);
    frame
}

/// Serialize a SPDY PING control frame (echo of the client's id).
fn ping_frame(id: u32) -> Vec<u8> {
    let mut frame = vec![0x80, 0x03, 0x00, 0x06, 0x00, 0x00, 0x00, 0x04];
    frame.extend_from_slice(&id.to_be_bytes());
    frame
}

/// The channels the client will open, in order, for the given session flags.
/// Mirrors client-go `remotecommand` stream creation (v2/v3/v4).
fn expected_channels(session: &StreamingSession) -> Vec<Channel> {
    let mut channels = vec![Channel::Error];
    if session.stdin {
        channels.push(Channel::Stdin);
    }
    if session.stdout {
        channels.push(Channel::Stdout);
    }
    if session.stderr && !session.tty {
        channels.push(Channel::Stderr);
    }
    if session.tty {
        channels.push(Channel::Resize);
    }
    channels
}

/// Read `SYN_STREAM` frames until every expected channel has a stream id,
/// echoing PINGs in the meantime. Returns the channel→stream-id map.
async fn collect_streams<R: AsyncReadExt + Unpin, W: AsyncWriteExt + Unpin>(
    reader: &mut R,
    writer: &mut W,
    expected: &[Channel],
) -> Result<HashMap<Channel, u32>, DynError> {
    tracing::debug!(expecting = expected.len(), "spdy collect_streams: start");
    let mut compressor = HeaderCompressor::new();
    let mut ids = HashMap::new();
    let mut opened = 0usize;
    while opened < expected.len() {
        match read_frame(reader).await? {
            Some(Frame::SynStream { stream_id }) => {
                let channel = expected[opened];
                // Accept the stream so the client proceeds to open the next one.
                let reply = syn_reply_frame(&mut compressor, stream_id);
                tracing::debug!(?channel, stream_id, reply_hex = %reply.iter().map(|b| format!("{b:02x}")).collect::<String>(), "spdy: SYN_STREAM -> SYN_REPLY");
                writer.write_all(&reply).await?;
                ids.insert(channel, stream_id);
                opened += 1;
            }
            Some(Frame::Ping { id }) => {
                tracing::debug!(id, "spdy: PING");
                writer.write_all(&ping_frame(id)).await?;
            }
            Some(Frame::Data { stream_id, .. }) => {
                tracing::debug!(stream_id, "spdy: early DATA before streams open (ignored)")
            }
            Some(Frame::Ignored) => tracing::debug!("spdy: ignored control frame"),
            None => {
                tracing::debug!("spdy: connection closed while collecting streams");
                break;
            }
        }
    }
    tracing::debug!(
        collected = opened,
        expected = expected.len(),
        "spdy: streams collected"
    );
    Ok(ids)
}

/// Build the v4 error-stream `metav1.Status` payload for a finished command.
/// Returns `None` for a successful (exit 0) command, where the client expects
/// the error stream to simply close.
fn exit_status_payload(exit_code: i32) -> Option<Vec<u8>> {
    if exit_code == 0 {
        return None;
    }
    let status = serde_json::json!({
        "metadata": {},
        "status": "Failure",
        "message": format!("command terminated with non-zero exit code: error executing command, exit code {exit_code}"),
        "reason": "NonZeroExitCode",
        "details": {
            "causes": [{ "reason": "ExitCode", "message": exit_code.to_string() }]
        }
    });
    Some(serde_json::to_vec(&status).unwrap_or_default())
}

/// Serve a CRI exec over SPDY. Handles non-TTY exec (with optional stdin);
/// TTY exec falls back to the dedicated PTY bridge.
pub async fn serve_exec(mut stream: TcpStream, session: &StreamingSession) -> Result<(), DynError> {
    tracing::debug!(
        stdin = session.stdin,
        stdout = session.stdout,
        stderr = session.stderr,
        tty = session.tty,
        "spdy serve_exec: enter"
    );
    // Small control/data frames must not be delayed by Nagle's algorithm or the
    // client's per-stream reply wait can time out.
    let _ = stream.set_nodelay(true);
    stream.write_all(UPGRADE_RESPONSE.as_bytes()).await?;
    tracing::debug!("spdy serve_exec: 101 sent, awaiting SPDY frames");

    if session.tty {
        return serve_exec_tty(stream, session).await;
    }

    let expected = expected_channels(session);
    let (mut reader, writer) = tokio::io::split(stream);
    let writer = std::sync::Arc::new(tokio::sync::Mutex::new(writer));

    let ids = {
        let mut guard = writer.lock().await;
        collect_streams(&mut reader, &mut *guard, &expected).await?
    };
    let stdin_id = ids.get(&Channel::Stdin).copied();
    let stdout_id = ids.get(&Channel::Stdout).copied();
    let stderr_id = ids.get(&Channel::Stderr).copied();
    let error_id = ids.get(&Channel::Error).copied();

    let request = a3s_box_core::exec::ExecRequest {
        cmd: session.cmd.clone(),
        timeout_ns: a3s_box_core::exec::DEFAULT_EXEC_TIMEOUT_NS,
        env: vec![],
        working_dir: None,
        rootfs: session.rootfs.clone(),
        stdin: None,
        stdin_streaming: session.stdin,
        user: None,
        streaming: false,
    };
    tracing::debug!(socket = %session.exec_socket_path, "spdy exec: connecting to guest exec");
    let client = a3s_box_runtime::ExecClient::connect(Path::new(&session.exec_socket_path)).await?;
    let mut exec = client.exec_stream(&request).await?;
    let input = exec.input();
    tracing::debug!("spdy exec: streaming started");

    // Client → guest: forward stdin DATA frames; echo PINGs.
    let reader_task = {
        let writer = writer.clone();
        async move {
            let mut stdin_closed = false;
            loop {
                match read_frame(&mut reader).await {
                    Ok(Some(Frame::Data {
                        stream_id,
                        fin,
                        data,
                    })) if Some(stream_id) == stdin_id => {
                        if !data.is_empty() {
                            let _ = input.write_stdin(&data).await;
                        }
                        if fin && !stdin_closed {
                            stdin_closed = true;
                            let _ = input.close_stdin().await;
                        }
                    }
                    Ok(Some(Frame::Ping { id })) => {
                        let _ = writer.lock().await.write_all(&ping_frame(id)).await;
                    }
                    Ok(Some(_)) => {}
                    Ok(None) | Err(_) => {
                        // Full client disconnect (e.g. the user Ctrl-C'd
                        // `kubectl exec`): cancel the guest command so a
                        // long-running exec doesn't keep running orphaned in the
                        // guest. Closing stdin alone won't stop a command that
                        // ignores stdin EOF, and the guest treats a bare
                        // connection drop as "no more input", not "kill".
                        let _ = input.cancel().await;
                        break;
                    }
                }
            }
        }
    };

    // Guest → client: relay stdout/stderr DATA frames, then the exit status.
    let writer_task = {
        let writer = writer.clone();
        async move {
            let mut exit_code = 0;
            loop {
                match exec.next_event().await {
                    Ok(Some(ExecEvent::Chunk(chunk))) => {
                        let target = match chunk.stream {
                            ExecStream::Stdout => stdout_id,
                            ExecStream::Stderr => stderr_id,
                        };
                        if let Some(id) = target {
                            let _ = writer
                                .lock()
                                .await
                                .write_all(&data_frame(id, false, &chunk.data))
                                .await;
                        }
                    }
                    Ok(Some(ExecEvent::Exit(exit))) => {
                        exit_code = exit.exit_code;
                        tracing::debug!(exit_code, "spdy exec: guest command exited");
                        break;
                    }
                    // Flush-acks are only used by the log-rotation path; ignore.
                    Ok(Some(ExecEvent::FlushAck)) => {}
                    Ok(None) | Err(_) => break,
                }
            }

            tracing::debug!(exit_code, "spdy exec: writing exit status + FIN");
            let mut guard = writer.lock().await;
            if let Some(id) = error_id {
                if let Some(body) = exit_status_payload(exit_code) {
                    let _ = guard.write_all(&data_frame(id, false, &body)).await;
                }
                let _ = guard.write_all(&data_frame(id, true, &[])).await;
            }
            if let Some(id) = stdout_id {
                let _ = guard.write_all(&data_frame(id, true, &[])).await;
            }
            if let Some(id) = stderr_id {
                let _ = guard.write_all(&data_frame(id, true, &[])).await;
            }
        }
    };

    // The command drives completion; the reader task is dropped once it exits.
    tokio::select! {
        _ = writer_task => {}
        _ = reader_task => {}
    }
    Ok(())
}

/// Serve an interactive TTY exec over SPDY by bridging the guest PTY socket.
/// The client opens error, [stdin], stdout and resize streams; PTY output is a
/// single stream relayed to stdout, and resize frames drive the guest PTY.
async fn serve_exec_tty(stream: TcpStream, session: &StreamingSession) -> Result<(), DynError> {
    use a3s_box_core::pty;
    use tokio::net::UnixStream;

    let expected = expected_channels(session);
    let (mut reader, writer) = tokio::io::split(stream);
    let writer = std::sync::Arc::new(tokio::sync::Mutex::new(writer));
    let ids = {
        let mut guard = writer.lock().await;
        collect_streams(&mut reader, &mut *guard, &expected).await?
    };
    let stdin_id = ids.get(&Channel::Stdin).copied();
    let stdout_id = ids.get(&Channel::Stdout).copied();
    let resize_id = ids.get(&Channel::Resize).copied();
    let error_id = ids.get(&Channel::Error).copied();

    let pty_stream = UnixStream::connect(&session.pty_socket_path).await?;
    let request = pty::PtyRequest {
        cmd: session.cmd.clone(),
        env: vec![],
        working_dir: None,
        rootfs: session.rootfs.clone(),
        user: None,
        cols: 80,
        rows: 24,
    };
    let (mut pty_read, mut pty_write) = tokio::io::split(pty_stream);
    write_pty_frame(
        &mut pty_write,
        pty::FRAME_PTY_REQUEST,
        &serde_json::to_vec(&request)?,
    )
    .await?;

    // Client → guest PTY: stdin data and resize requests.
    let client_to_pty = async move {
        loop {
            match read_frame(&mut reader).await {
                Ok(Some(Frame::Data {
                    stream_id, data, ..
                })) if Some(stream_id) == stdin_id && !data.is_empty() => {
                    if write_pty_frame(&mut pty_write, pty::FRAME_PTY_DATA, &data)
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
                Ok(Some(Frame::Data {
                    stream_id, data, ..
                })) if Some(stream_id) == resize_id && !data.is_empty() => {
                    if let Ok(size) = serde_json::from_slice::<TerminalSize>(&data) {
                        let resize = pty::PtyResize {
                            cols: size.width,
                            rows: size.height,
                        };
                        if let Ok(payload) = serde_json::to_vec(&resize) {
                            let _ =
                                write_pty_frame(&mut pty_write, pty::FRAME_PTY_RESIZE, &payload)
                                    .await;
                        }
                    }
                }
                Ok(Some(_)) => {}
                Ok(None) | Err(_) => break,
            }
        }
    };

    // Guest PTY → client: relay PTY data to the stdout stream; close on exit.
    let pty_to_client = {
        let writer = writer.clone();
        async move {
            let mut header = [0u8; 5];
            while pty_read.read_exact(&mut header).await.is_ok() {
                let frame_type = header[0];
                let len = u32::from_be_bytes([header[1], header[2], header[3], header[4]]) as usize;
                if len > pty::MAX_FRAME_PAYLOAD {
                    break;
                }
                let mut payload = vec![0u8; len];
                if len > 0 && pty_read.read_exact(&mut payload).await.is_err() {
                    break;
                }
                match frame_type {
                    t if t == pty::FRAME_PTY_DATA => {
                        if let Some(id) = stdout_id {
                            let _ = writer
                                .lock()
                                .await
                                .write_all(&data_frame(id, false, &payload))
                                .await;
                        }
                    }
                    t if t == pty::FRAME_PTY_EXIT => break,
                    _ => {}
                }
            }
            let mut guard = writer.lock().await;
            if let Some(id) = error_id {
                let _ = guard.write_all(&data_frame(id, true, &[])).await;
            }
            if let Some(id) = stdout_id {
                let _ = guard.write_all(&data_frame(id, true, &[])).await;
            }
        }
    };

    tokio::select! {
        _ = pty_to_client => {}
        _ = client_to_pty => {}
    }
    Ok(())
}

/// Serve a CRI attach over SPDY: bridge the running container's broadcast
/// output to the client's stdout/stderr streams and the client's stdin to the
/// workload's stdin sink. Unlike exec, there is no command — output comes from
/// the supervisor's broadcast channel registered when the container started.
pub async fn serve_attach(
    mut stream: TcpStream,
    session: &StreamingSession,
) -> Result<(), DynError> {
    use tokio::sync::broadcast::error::RecvError;

    let Some(attach_stream) = session.attach_stream.as_ref() else {
        let body = "Attach is not available: no running workload stream is registered.";
        let resp = format!(
            "HTTP/1.1 501 Not Implemented\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        stream.write_all(resp.as_bytes()).await?;
        return Ok(());
    };

    let _ = stream.set_nodelay(true);
    stream.write_all(UPGRADE_RESPONSE.as_bytes()).await?;
    tracing::debug!("spdy serve_attach: 101 sent, awaiting SPDY frames");

    let expected = expected_channels(session);
    let (mut reader, writer) = tokio::io::split(stream);
    let writer = std::sync::Arc::new(tokio::sync::Mutex::new(writer));
    let ids = {
        let mut guard = writer.lock().await;
        collect_streams(&mut reader, &mut *guard, &expected).await?
    };
    let stdin_id = ids.get(&Channel::Stdin).copied();
    let stdout_id = ids.get(&Channel::Stdout).copied();
    let stderr_id = ids.get(&Channel::Stderr).copied();
    let error_id = ids.get(&Channel::Error).copied();

    let mut receiver = attach_stream.subscribe();
    let stdin = session.attach_stdin.clone();
    let stdin_once = session.stdin_once;

    // Client → workload stdin.
    let reader_task = {
        let writer = writer.clone();
        async move {
            let mut closed = false;
            loop {
                match read_frame(&mut reader).await {
                    Ok(Some(Frame::Data {
                        stream_id,
                        fin,
                        data,
                    })) if Some(stream_id) == stdin_id => {
                        if !data.is_empty() {
                            if let Some(input) = stdin.as_ref() {
                                let _ = input.write_stdin(&data).await;
                            }
                        }
                        if fin && !closed {
                            closed = true;
                            if stdin_once {
                                if let Some(input) = stdin.as_ref() {
                                    let _ = input.close_stdin().await;
                                }
                            }
                        }
                    }
                    Ok(Some(Frame::Ping { id })) => {
                        let _ = writer.lock().await.write_all(&ping_frame(id)).await;
                    }
                    Ok(Some(_)) => {}
                    Ok(None) | Err(_) => break,
                }
            }
            // The loop also exits on a frame-read error (or EOF) before a `fin`
            // stdin frame; close stdin here so a stdin_once attach doesn't leak an
            // open stdin on an abnormal client disconnect.
            if stdin_once && !closed {
                if let Some(input) = stdin.as_ref() {
                    let _ = input.close_stdin().await;
                }
            }
        }
    };

    // Running container output → client.
    let writer_task = {
        let writer = writer.clone();
        async move {
            loop {
                match receiver.recv().await {
                    Ok(ExecEvent::Chunk(chunk)) => {
                        let target = match chunk.stream {
                            ExecStream::Stdout => stdout_id,
                            ExecStream::Stderr => stderr_id,
                        };
                        if let Some(id) = target {
                            let _ = writer
                                .lock()
                                .await
                                .write_all(&data_frame(id, false, &chunk.data))
                                .await;
                        }
                    }
                    Ok(ExecEvent::FlushAck) => {}
                    Ok(ExecEvent::Exit(_)) => break,
                    Err(RecvError::Lagged(_)) => continue,
                    Err(RecvError::Closed) => break,
                }
            }
            let mut guard = writer.lock().await;
            for id in [error_id, stdout_id, stderr_id].into_iter().flatten() {
                let _ = guard.write_all(&data_frame(id, true, &[])).await;
            }
        }
    };

    tokio::select! {
        _ = writer_task => {}
        _ = reader_task => {}
    }
    Ok(())
}

/// Terminal resize payload sent by the client on the resize stream.
#[derive(serde::Deserialize)]
struct TerminalSize {
    #[serde(rename = "Width", alias = "width")]
    width: u16,
    #[serde(rename = "Height", alias = "height")]
    height: u16,
}

/// Write a guest PTY frame: `[u8 type][u32 BE len][payload]`.
async fn write_pty_frame<W: AsyncWriteExt + Unpin>(
    writer: &mut W,
    frame_type: u8,
    payload: &[u8],
) -> std::io::Result<()> {
    writer.write_all(&[frame_type]).await?;
    writer
        .write_all(&(payload.len() as u32).to_be_bytes())
        .await?;
    writer.write_all(payload).await?;
    Ok(())
}

/// 101 response that upgrades the connection to SPDY for the `portforward.k8s.io`
/// subprotocol. crictl/kubelet stream raw TCP bytes over a single SPDY data
/// stream per forwarded port.
const UPGRADE_RESPONSE_PORTFORWARD: &str = "HTTP/1.1 101 Switching Protocols\r\n\
Connection: Upgrade\r\n\
Upgrade: SPDY/3.1\r\n\
X-Stream-Protocol-Version: portforward.k8s.io\r\n\r\n";

/// Serve a CRI port-forward over SPDY.
///
/// The portforward client (crictl/kubelet) opens, per forwarded port, an
/// **error** stream then a **data** stream (in that fixed order), immediately
/// half-closes the error stream, and tunnels the raw TCP byte stream over the
/// data stream. We accept both `SYN_STREAM`s (replying so the client proceeds),
/// then bridge the data stream to the guest's vsock port-forward control
/// channel, which dials `127.0.0.1:<port>` inside the sandbox VM.
pub async fn serve_port_forward(
    mut stream: TcpStream,
    session: &StreamingSession,
) -> Result<(), DynError> {
    use crate::streaming::{
        read_port_forward_frame, write_port_forward_frame, PORT_FORWARD_FRAME_CLOSE,
        PORT_FORWARD_FRAME_DATA, PORT_FORWARD_FRAME_OPEN, PORT_FORWARD_FRAME_OPEN_ACK,
        PORT_FORWARD_STREAM_ID,
    };
    use tokio::net::UnixStream;

    let _ = stream.set_nodelay(true);

    let port = match session.ports.first().and_then(|p| u16::try_from(*p).ok()) {
        Some(port) => port,
        None => {
            stream
                .write_all(b"HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\n\r\n")
                .await?;
            return Ok(());
        }
    };
    if session.port_forward_socket_path.is_empty() {
        stream
            .write_all(b"HTTP/1.1 501 Not Implemented\r\nContent-Length: 0\r\n\r\n")
            .await?;
        return Ok(());
    }

    stream
        .write_all(UPGRADE_RESPONSE_PORTFORWARD.as_bytes())
        .await?;
    tracing::debug!(
        port,
        "spdy serve_port_forward: 101 sent, awaiting SPDY frames"
    );

    let (mut reader, writer) = tokio::io::split(stream);
    let writer = std::sync::Arc::new(tokio::sync::Mutex::new(writer));

    // Collect the error + data streams (open order: error first, data second),
    // replying to each so the client proceeds. The error stream is half-closed
    // by the client straight away (empty DATA+FIN), which we simply ignore here.
    let mut compressor = HeaderCompressor::new();
    let mut stream_ids: Vec<u32> = Vec::new();
    {
        let mut guard = writer.lock().await;
        while stream_ids.len() < 2 {
            match read_frame(&mut reader).await? {
                Some(Frame::SynStream { stream_id }) => {
                    guard
                        .write_all(&syn_reply_frame(&mut compressor, stream_id))
                        .await?;
                    tracing::debug!(
                        stream_id,
                        idx = stream_ids.len(),
                        "spdy pf: SYN_STREAM -> SYN_REPLY"
                    );
                    stream_ids.push(stream_id);
                }
                Some(Frame::Ping { id }) => {
                    guard.write_all(&ping_frame(id)).await?;
                }
                Some(Frame::Data {
                    stream_id,
                    fin,
                    data,
                }) => {
                    tracing::debug!(
                        stream_id,
                        fin,
                        len = data.len(),
                        "spdy pf: DATA during stream collection (ignored)"
                    );
                }
                Some(Frame::Ignored) => {}
                None => {
                    tracing::debug!("spdy pf: connection closed during stream collection");
                    return Ok(());
                }
            }
        }
    }
    let data_id = stream_ids[1];
    tracing::debug!(
        error_id = stream_ids[0],
        data_id,
        port,
        "spdy pf: streams collected"
    );

    // Connect to the guest control channel and open the target guest port.
    let mut control = match UnixStream::connect(&session.port_forward_socket_path).await {
        Ok(control) => control,
        Err(error) => {
            tracing::warn!(error = %error, "spdy pf: guest control connect failed");
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
    let ack = read_port_forward_frame(&mut control).await?;
    let open_ok = ack.as_ref().is_some_and(|frame| {
        frame.kind == PORT_FORWARD_FRAME_OPEN_ACK
            && frame.stream_id == PORT_FORWARD_STREAM_ID
            && frame.payload.first().copied().unwrap_or(1) == 0
    });
    if !open_ok {
        tracing::warn!(got_ack = ack.is_some(), port, "spdy pf: guest OPEN failed");
        return Ok(());
    }
    tracing::debug!(port, "spdy pf: guest OPEN_ACK ok");

    let (mut control_read, mut control_write) = tokio::io::split(control);

    // Client data stream -> guest TCP.
    let client_to_guest = {
        let writer = writer.clone();
        async move {
            loop {
                match read_frame(&mut reader).await {
                    Ok(Some(Frame::Data {
                        stream_id,
                        fin,
                        data,
                    })) if stream_id == data_id => {
                        tracing::debug!(len = data.len(), fin, "spdy pf: client DATA -> guest");
                        if !data.is_empty()
                            && write_port_forward_frame(
                                &mut control_write,
                                PORT_FORWARD_FRAME_DATA,
                                PORT_FORWARD_STREAM_ID,
                                &data,
                            )
                            .await
                            .is_err()
                        {
                            break;
                        }
                        if fin {
                            let _ = write_port_forward_frame(
                                &mut control_write,
                                PORT_FORWARD_FRAME_CLOSE,
                                PORT_FORWARD_STREAM_ID,
                                &[],
                            )
                            .await;
                            break;
                        }
                    }
                    Ok(Some(Frame::Data { stream_id, fin, .. })) => {
                        tracing::debug!(
                            stream_id,
                            fin,
                            "spdy pf: client DATA on non-data stream (ignored)"
                        );
                    }
                    Ok(Some(Frame::Ping { id })) => {
                        let _ = writer.lock().await.write_all(&ping_frame(id)).await;
                    }
                    Ok(Some(_)) => {}
                    Ok(None) | Err(_) => {
                        let _ = write_port_forward_frame(
                            &mut control_write,
                            PORT_FORWARD_FRAME_CLOSE,
                            PORT_FORWARD_STREAM_ID,
                            &[],
                        )
                        .await;
                        break;
                    }
                }
            }
        }
    };

    // Guest TCP -> client data stream.
    let guest_to_client = {
        let writer = writer.clone();
        async move {
            loop {
                match read_port_forward_frame(&mut control_read).await {
                    Ok(Some(frame)) if frame.stream_id == PORT_FORWARD_STREAM_ID => {
                        match frame.kind {
                            PORT_FORWARD_FRAME_DATA => {
                                tracing::debug!(
                                    len = frame.payload.len(),
                                    "spdy pf: guest DATA -> client"
                                );
                                if writer
                                    .lock()
                                    .await
                                    .write_all(&data_frame(data_id, false, &frame.payload))
                                    .await
                                    .is_err()
                                {
                                    break;
                                }
                            }
                            PORT_FORWARD_FRAME_CLOSE => {
                                tracing::debug!("spdy pf: guest CLOSE -> client FIN");
                                let _ = writer
                                    .lock()
                                    .await
                                    .write_all(&data_frame(data_id, true, &[]))
                                    .await;
                                break;
                            }
                            _ => {}
                        }
                    }
                    Ok(Some(_)) => {}
                    Ok(None) | Err(_) => break,
                }
            }
        }
    };

    tokio::pin!(client_to_guest);
    tokio::pin!(guest_to_client);
    tokio::select! {
        _ = &mut guest_to_client => {
            tracing::debug!("spdy pf: guest_to_client finished");
        }
        _ = &mut client_to_guest => {
            tracing::debug!("spdy pf: client_to_guest finished, draining response");
            let _ = guest_to_client.await;
        }
    }
    tracing::debug!(port, "spdy serve_port_forward: done");
    Ok(())
}
