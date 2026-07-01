//! PTY terminal client for interactive sessions.

use std::path::Path;
use std::sync::Arc;

use a3s_box_core::error::{BoxError, Result};
use tokio::sync::Mutex;

type PtyFrameReader = a3s_transport::FrameReader<tokio::io::ReadHalf<tokio::net::UnixStream>>;
type PtyFrameWriter = a3s_transport::FrameWriter<tokio::io::WriteHalf<tokio::net::UnixStream>>;

/// Client for interactive PTY sessions in the guest over Unix socket.
///
/// Connects to the PTY server (vsock port 4090) and provides async
/// frame-based communication for bidirectional terminal I/O.
/// Uses `a3s_transport::FrameReader`/`FrameWriter` for wire I/O.
#[derive(Debug)]
pub struct PtyClient {
    reader: PtyFrameReader,
    writer: PtyFrameWriter,
}

impl PtyClient {
    /// Connect to the PTY server via Unix socket.
    pub async fn connect(socket_path: &Path) -> Result<Self> {
        let stream = tokio::net::UnixStream::connect(socket_path)
            .await
            .map_err(|e| {
                BoxError::ExecError(format!(
                    "Failed to connect to PTY server at {}: {}",
                    socket_path.display(),
                    e,
                ))
            })?;

        let (r, w) = tokio::io::split(stream);
        Ok(Self {
            reader: a3s_transport::FrameReader::new(r),
            writer: a3s_transport::FrameWriter::new(w),
        })
    }

    /// Send a PtyRequest to start an interactive session.
    pub async fn send_request(&mut self, req: &a3s_box_core::pty::PtyRequest) -> Result<()> {
        let payload = serde_json::to_vec(req)
            .map_err(|e| BoxError::ExecError(format!("Failed to serialize PtyRequest: {}", e)))?;
        self.write_raw_frame(a3s_box_core::pty::FRAME_PTY_REQUEST, &payload)
            .await
    }

    /// Start a streaming PTY session and return a handle for supervision.
    pub async fn start_stream(
        mut self,
        req: &a3s_box_core::pty::PtyRequest,
    ) -> Result<StreamingPty> {
        self.send_request(req).await?;
        Ok(StreamingPty {
            reader: self.reader,
            writer: Arc::new(Mutex::new(self.writer)),
            started: std::time::Instant::now(),
            stdout_bytes: 0,
            done: false,
        })
    }

    /// Send terminal data to the guest.
    pub async fn send_data(&mut self, data: &[u8]) -> Result<()> {
        self.write_raw_frame(a3s_box_core::pty::FRAME_PTY_DATA, data)
            .await
    }

    /// Send a terminal resize notification.
    pub async fn send_resize(&mut self, cols: u16, rows: u16) -> Result<()> {
        let resize = a3s_box_core::pty::PtyResize { cols, rows };
        let payload = serde_json::to_vec(&resize)
            .map_err(|e| BoxError::ExecError(format!("Failed to serialize PtyResize: {}", e)))?;
        self.write_raw_frame(a3s_box_core::pty::FRAME_PTY_RESIZE, &payload)
            .await
    }

    /// Read the next frame from the guest.
    ///
    /// Returns `Ok(None)` on EOF (guest disconnected).
    pub async fn read_frame(&mut self) -> Result<Option<(u8, Vec<u8>)>> {
        match self.reader.read_frame().await {
            Ok(Some(frame)) => Ok(Some((frame.frame_type as u8, frame.payload))),
            Ok(None) => Ok(None),
            Err(e) => Err(BoxError::ExecError(format!("PTY frame read failed: {}", e))),
        }
    }

    /// Split the client into read and write halves for concurrent I/O.
    pub fn into_split(
        self,
    ) -> (
        a3s_transport::FrameReader<tokio::io::ReadHalf<tokio::net::UnixStream>>,
        a3s_transport::FrameWriter<tokio::io::WriteHalf<tokio::net::UnixStream>>,
    ) {
        (self.reader, self.writer)
    }

    /// Write a raw PTY frame using the transport writer.
    async fn write_raw_frame(&mut self, frame_type: u8, payload: &[u8]) -> Result<()> {
        // PTY uses custom frame type bytes (0x01-0x05) that map to transport FrameType
        let ft = a3s_transport::FrameType::try_from(frame_type)
            .unwrap_or(a3s_transport::FrameType::Data);
        let frame = a3s_transport::Frame {
            frame_type: ft,
            payload: payload.to_vec(),
        };
        self.writer
            .write_frame(&frame)
            .await
            .map_err(|e| BoxError::ExecError(format!("PTY frame write failed: {}", e)))
    }
}

/// Cloneable input side for a running PTY session.
#[derive(Clone, Debug)]
pub struct StreamingPtyInput {
    writer: Arc<Mutex<PtyFrameWriter>>,
}

impl StreamingPtyInput {
    /// Write terminal data to the PTY.
    pub async fn write_stdin(&self, data: &[u8]) -> Result<()> {
        let frame = a3s_transport::Frame {
            frame_type: a3s_transport::FrameType::Control,
            payload: data.to_vec(),
        };
        self.writer
            .lock()
            .await
            .write_frame(&frame)
            .await
            .map_err(|e| BoxError::ExecError(format!("PTY stdin write failed: {}", e)))
    }

    /// Request terminal resize.
    pub async fn resize(&self, cols: u16, rows: u16) -> Result<()> {
        let resize = a3s_box_core::pty::PtyResize { cols, rows };
        let payload = serde_json::to_vec(&resize)
            .map_err(|e| BoxError::ExecError(format!("Failed to serialize PtyResize: {}", e)))?;
        let frame = a3s_transport::Frame {
            frame_type: a3s_transport::FrameType::Heartbeat,
            payload,
        };
        self.writer
            .lock()
            .await
            .write_frame(&frame)
            .await
            .map_err(|e| BoxError::ExecError(format!("PTY resize write failed: {}", e)))
    }

    /// Close the PTY control stream. The guest treats this as a session stop.
    pub async fn close(&self) -> Result<()> {
        self.writer
            .lock()
            .await
            .write_frame(&a3s_transport::Frame::close())
            .await
            .map_err(|e| BoxError::ExecError(format!("PTY close write failed: {}", e)))
    }
}

/// Handle for reading PTY session output and exit status.
pub struct StreamingPty {
    reader: PtyFrameReader,
    writer: Arc<Mutex<PtyFrameWriter>>,
    started: std::time::Instant,
    stdout_bytes: u64,
    done: bool,
}

impl StreamingPty {
    /// Return a cloneable input handle for this PTY session.
    pub fn input(&self) -> StreamingPtyInput {
        StreamingPtyInput {
            writer: self.writer.clone(),
        }
    }

    /// Read the next PTY output or exit event.
    pub async fn next_event(&mut self) -> Result<Option<a3s_box_core::exec::ExecEvent>> {
        use a3s_box_core::exec::{ExecChunk, ExecEvent, ExecExit, StreamType};

        if self.done {
            return Ok(None);
        }

        let Some(frame) = self
            .reader
            .read_frame()
            .await
            .map_err(|e| BoxError::ExecError(format!("PTY frame read failed: {}", e)))?
        else {
            self.done = true;
            return Ok(None);
        };

        match frame.frame_type as u8 {
            a3s_box_core::pty::FRAME_PTY_DATA => {
                self.stdout_bytes += frame.payload.len() as u64;
                Ok(Some(ExecEvent::Chunk(ExecChunk {
                    stream: StreamType::Stdout,
                    data: frame.payload,
                })))
            }
            a3s_box_core::pty::FRAME_PTY_EXIT => {
                let exit: a3s_box_core::pty::PtyExit = serde_json::from_slice(&frame.payload)
                    .map_err(|e| BoxError::ExecError(format!("Failed to parse PTY exit: {}", e)))?;
                self.done = true;
                Ok(Some(ExecEvent::Exit(ExecExit {
                    exit_code: exit.exit_code,
                    oom_killed: false,
                })))
            }
            a3s_box_core::pty::FRAME_PTY_ERROR => {
                let msg = String::from_utf8_lossy(&frame.payload);
                self.done = true;
                Err(BoxError::ExecError(format!("PTY session error: {}", msg)))
            }
            other => Err(BoxError::ExecError(format!(
                "Unexpected PTY frame type in stream: 0x{other:02x}"
            ))),
        }
    }

    /// Request cancellation of the running PTY session.
    pub async fn cancel(&mut self) -> Result<()> {
        self.input().close().await
    }

    /// Whether the PTY stream has finished.
    pub fn is_done(&self) -> bool {
        self.done
    }

    /// Get execution metrics so far.
    pub fn metrics(&self) -> a3s_box_core::exec::ExecMetrics {
        a3s_box_core::exec::ExecMetrics {
            duration_ms: self.started.elapsed().as_millis() as u64,
            peak_memory_bytes: None,
            stdout_bytes: self.stdout_bytes,
            stderr_bytes: 0,
        }
    }
}

impl std::fmt::Debug for StreamingPty {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StreamingPty")
            .field("done", &self.done)
            .field("stdout_bytes", &self.stdout_bytes)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::UnixListener;

    fn bind_test_listener(path: &Path) -> Option<UnixListener> {
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

    #[tokio::test]
    async fn test_pty_client_connect_nonexistent() {
        let result = PtyClient::connect(Path::new("/tmp/nonexistent-pty-test.sock")).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_pty_frame_roundtrip() {
        let tmp = tempfile::TempDir::new().unwrap();
        let sock_path = tmp.path().join("pty.sock");
        let Some(listener) = bind_test_listener(&sock_path) else {
            return;
        };

        let sock_path_clone = sock_path.clone();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            // Read a frame: [type:1][len:4][payload]
            let mut header = [0u8; 5];
            stream.read_exact(&mut header).await.unwrap();
            let frame_type = header[0];
            let len = u32::from_be_bytes([header[1], header[2], header[3], header[4]]) as usize;
            let mut payload = vec![0u8; len];
            if len > 0 {
                stream.read_exact(&mut payload).await.unwrap();
            }
            // Echo it back
            stream.write_all(&header).await.unwrap();
            stream.write_all(&payload).await.unwrap();
            (frame_type, payload)
        });

        let mut client = PtyClient::connect(&sock_path_clone).await.unwrap();
        client.send_data(b"hello world").await.unwrap();

        let frame = client.read_frame().await.unwrap().unwrap();
        assert_eq!(frame.0, a3s_box_core::pty::FRAME_PTY_DATA);
        assert_eq!(&frame.1[..], b"hello world");

        let (server_type, server_payload) = server.await.unwrap();
        assert_eq!(server_type, a3s_box_core::pty::FRAME_PTY_DATA);
        assert_eq!(&server_payload[..], b"hello world");
    }

    #[tokio::test]
    async fn test_pty_send_resize() {
        let tmp = tempfile::TempDir::new().unwrap();
        let sock_path = tmp.path().join("pty_resize.sock");
        let Some(listener) = bind_test_listener(&sock_path) else {
            return;
        };

        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut header = [0u8; 5];
            stream.read_exact(&mut header).await.unwrap();
            let frame_type = header[0];
            let len = u32::from_be_bytes([header[1], header[2], header[3], header[4]]) as usize;
            let mut payload = vec![0u8; len];
            stream.read_exact(&mut payload).await.unwrap();

            assert_eq!(frame_type, a3s_box_core::pty::FRAME_PTY_RESIZE);
            let resize: a3s_box_core::pty::PtyResize = serde_json::from_slice(&payload).unwrap();
            assert_eq!(resize.cols, 120);
            assert_eq!(resize.rows, 40);
        });

        let mut client = PtyClient::connect(&sock_path).await.unwrap();
        client.send_resize(120, 40).await.unwrap();
    }

    #[tokio::test]
    async fn test_pty_read_frame_eof() {
        let tmp = tempfile::TempDir::new().unwrap();
        let sock_path = tmp.path().join("pty_eof.sock");
        let Some(listener) = bind_test_listener(&sock_path) else {
            return;
        };

        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            drop(stream); // Close immediately → EOF
        });

        let mut client = PtyClient::connect(&sock_path).await.unwrap();
        let frame = client.read_frame().await.unwrap();
        assert!(frame.is_none()); // EOF
    }

    #[tokio::test]
    async fn test_pty_client_start_stream_reads_data_and_exit() {
        let tmp = tempfile::TempDir::new().unwrap();
        let sock_path = tmp.path().join("pty_stream.sock");
        let Some(listener) = bind_test_listener(&sock_path) else {
            return;
        };

        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let (r, w) = tokio::io::split(stream);
            let mut reader = a3s_transport::FrameReader::new(r);
            let mut writer = a3s_transport::FrameWriter::new(w);

            let request = reader.read_frame().await.unwrap().unwrap();
            assert_eq!(
                request.frame_type as u8,
                a3s_box_core::pty::FRAME_PTY_REQUEST
            );

            writer
                .write_frame(&a3s_transport::Frame {
                    frame_type: a3s_transport::FrameType::Control,
                    payload: b"tty output".to_vec(),
                })
                .await
                .unwrap();

            let exit = a3s_box_core::pty::PtyExit { exit_code: 9 };
            writer
                .write_frame(&a3s_transport::Frame {
                    frame_type: a3s_transport::FrameType::Error,
                    payload: serde_json::to_vec(&exit).unwrap(),
                })
                .await
                .unwrap();
        });

        let client = PtyClient::connect(&sock_path).await.unwrap();
        let req = a3s_box_core::pty::PtyRequest {
            cmd: vec!["/bin/sh".to_string()],
            env: vec![],
            working_dir: None,
            rootfs: None,
            user: None,
            cols: 80,
            rows: 24,
        };
        let mut stream = client.start_stream(&req).await.unwrap();

        match stream.next_event().await.unwrap().unwrap() {
            a3s_box_core::exec::ExecEvent::Chunk(chunk) => {
                assert_eq!(chunk.stream, a3s_box_core::exec::StreamType::Stdout);
                assert_eq!(chunk.data, b"tty output");
            }
            other => panic!("unexpected event: {other:?}"),
        }
        match stream.next_event().await.unwrap().unwrap() {
            a3s_box_core::exec::ExecEvent::Exit(exit) => assert_eq!(exit.exit_code, 9),
            other => panic!("unexpected event: {other:?}"),
        }
    }

    #[tokio::test]
    async fn test_pty_stream_cancel_writes_close_frame() {
        let tmp = tempfile::TempDir::new().unwrap();
        let sock_path = tmp.path().join("pty_cancel.sock");
        let Some(listener) = bind_test_listener(&sock_path) else {
            return;
        };

        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let (r, _w) = tokio::io::split(stream);
            let mut reader = a3s_transport::FrameReader::new(r);

            let request = reader.read_frame().await.unwrap().unwrap();
            assert_eq!(
                request.frame_type as u8,
                a3s_box_core::pty::FRAME_PTY_REQUEST
            );
            let close = reader.read_frame().await.unwrap().unwrap();
            assert_eq!(close.frame_type, a3s_transport::FrameType::Close);
            assert!(close.payload.is_empty());
        });

        let client = PtyClient::connect(&sock_path).await.unwrap();
        let req = a3s_box_core::pty::PtyRequest {
            cmd: vec!["sleep".to_string(), "60".to_string()],
            env: vec![],
            working_dir: None,
            rootfs: None,
            user: None,
            cols: 80,
            rows: 24,
        };
        let mut stream = client.start_stream(&req).await.unwrap();
        stream.cancel().await.unwrap();
    }
}
