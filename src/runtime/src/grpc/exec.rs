//! Command execution and streaming clients.

use std::path::{Path, PathBuf};

use a3s_box_core::error::{BoxError, Result};
use tokio::io::AsyncWriteExt;
use tokio::net::UnixStream;

/// Client for executing commands in the guest over Unix socket.
///
/// Uses the Frame wire protocol: sends a Data frame with JSON ExecRequest,
/// receives a Data frame with JSON ExecOutput.
#[derive(Debug)]
pub struct ExecClient {
    socket_path: PathBuf,
}

impl ExecClient {
    /// Connect to the exec server via Unix socket.
    ///
    /// Verifies the socket is connectable.
    pub async fn connect(socket_path: &Path) -> Result<Self> {
        let _stream = UnixStream::connect(socket_path).await.map_err(|e| {
            BoxError::ExecError(format!(
                "Failed to connect to exec server at {}: {}",
                socket_path.display(),
                e,
            ))
        })?;

        Ok(Self {
            socket_path: socket_path.to_path_buf(),
        })
    }

    /// Get the socket path this client is connected to.
    pub fn socket_path(&self) -> &Path {
        &self.socket_path
    }

    /// Execute a command in the guest.
    ///
    /// Sends a Data frame with JSON ExecRequest, reads a Data frame with JSON ExecOutput.
    pub async fn exec_command(
        &self,
        request: &a3s_box_core::exec::ExecRequest,
    ) -> Result<a3s_box_core::exec::ExecOutput> {
        let payload = serde_json::to_vec(request)
            .map_err(|e| BoxError::ExecError(format!("Failed to serialize exec request: {}", e)))?;

        let mut stream = UnixStream::connect(&self.socket_path).await.map_err(|e| {
            BoxError::ExecError(format!(
                "Exec connection failed to {}: {}",
                self.socket_path.display(),
                e,
            ))
        })?;

        // Send request as Data frame
        let request_frame = a3s_transport::Frame::data(payload);
        let encoded = request_frame.encode().map_err(|e| {
            BoxError::ExecError(format!("Failed to encode exec request frame: {}", e))
        })?;
        stream
            .write_all(&encoded)
            .await
            .map_err(|e| BoxError::ExecError(format!("Exec request write failed: {}", e)))?;

        // Read response frame
        let (r, _w) = tokio::io::split(stream);
        let mut reader = a3s_transport::FrameReader::new(r);
        let frame = reader
            .read_frame()
            .await
            .map_err(|e| BoxError::ExecError(format!("Exec response read failed: {}", e)))?
            .ok_or_else(|| {
                BoxError::ExecError("Exec server closed without response".to_string())
            })?;

        match frame.frame_type {
            a3s_transport::FrameType::Data => {
                let output: a3s_box_core::exec::ExecOutput = serde_json::from_slice(&frame.payload)
                    .map_err(|e| {
                        BoxError::ExecError(format!("Failed to parse exec response: {}", e))
                    })?;
                Ok(output)
            }
            a3s_transport::FrameType::Error => {
                let msg = String::from_utf8_lossy(&frame.payload);
                Err(BoxError::ExecError(format!("Exec server error: {}", msg)))
            }
            _ => Err(BoxError::ExecError(format!(
                "Unexpected frame type: {:?}",
                frame.frame_type
            ))),
        }
    }

    /// Execute a command in streaming mode.
    ///
    /// Sends a Data frame with JSON ExecRequest (streaming=true), then reads
    /// multiple frames: ExecChunk frames for stdout/stderr data, and a final
    /// ExecExit frame with the exit code.
    ///
    /// Returns a `StreamingExec` handle for reading events.
    pub async fn exec_stream(
        &self,
        request: &a3s_box_core::exec::ExecRequest,
    ) -> Result<StreamingExec> {
        let mut req = request.clone();
        req.streaming = true;

        let payload = serde_json::to_vec(&req)
            .map_err(|e| BoxError::ExecError(format!("Failed to serialize exec request: {}", e)))?;

        let stream = UnixStream::connect(&self.socket_path).await.map_err(|e| {
            BoxError::ExecError(format!(
                "Exec connection failed to {}: {}",
                self.socket_path.display(),
                e,
            ))
        })?;

        // Send request as Data frame
        let (r, mut w) = tokio::io::split(stream);
        let request_frame = a3s_transport::Frame::data(payload);
        let encoded = request_frame.encode().map_err(|e| {
            BoxError::ExecError(format!("Failed to encode exec request frame: {}", e))
        })?;
        w.write_all(&encoded)
            .await
            .map_err(|e| BoxError::ExecError(format!("Exec request write failed: {}", e)))?;

        let reader = a3s_transport::FrameReader::new(r);
        let started = std::time::Instant::now();

        Ok(StreamingExec {
            reader,
            started,
            stdout_bytes: 0,
            stderr_bytes: 0,
            done: false,
        })
    }

    /// Transfer a file to/from the guest.
    ///
    /// Sends a Data frame with JSON FileRequest, reads a Data frame with JSON FileResponse.
    pub async fn file_transfer(
        &self,
        request: &a3s_box_core::exec::FileRequest,
    ) -> Result<a3s_box_core::exec::FileResponse> {
        let payload = serde_json::to_vec(request)
            .map_err(|e| BoxError::ExecError(format!("Failed to serialize file request: {}", e)))?;

        let mut stream = UnixStream::connect(&self.socket_path).await.map_err(|e| {
            BoxError::ExecError(format!(
                "Exec connection failed to {}: {}",
                self.socket_path.display(),
                e,
            ))
        })?;

        let request_frame = a3s_transport::Frame::data(payload);
        let encoded = request_frame.encode().map_err(|e| {
            BoxError::ExecError(format!("Failed to encode file request frame: {}", e))
        })?;
        stream
            .write_all(&encoded)
            .await
            .map_err(|e| BoxError::ExecError(format!("File request write failed: {}", e)))?;

        let (r, _w) = tokio::io::split(stream);
        let mut reader = a3s_transport::FrameReader::new(r);
        let frame = reader
            .read_frame()
            .await
            .map_err(|e| BoxError::ExecError(format!("File response read failed: {}", e)))?
            .ok_or_else(|| {
                BoxError::ExecError("Exec server closed without response".to_string())
            })?;

        match frame.frame_type {
            a3s_transport::FrameType::Data => {
                let response: a3s_box_core::exec::FileResponse =
                    serde_json::from_slice(&frame.payload).map_err(|e| {
                        BoxError::ExecError(format!("Failed to parse file response: {}", e))
                    })?;
                Ok(response)
            }
            a3s_transport::FrameType::Error => {
                let msg = String::from_utf8_lossy(&frame.payload);
                Err(BoxError::ExecError(format!("File transfer error: {}", msg)))
            }
            _ => Err(BoxError::ExecError(format!(
                "Unexpected frame type: {:?}",
                frame.frame_type
            ))),
        }
    }

    /// Send a Heartbeat frame and wait for a Heartbeat response.
    ///
    /// Returns `true` if the exec server responds, `false` otherwise.
    pub async fn heartbeat(&self) -> Result<bool> {
        let mut stream = match UnixStream::connect(&self.socket_path).await {
            Ok(s) => s,
            Err(_) => return Ok(false),
        };

        let frame = a3s_transport::Frame::heartbeat();
        let encoded = match frame.encode() {
            Ok(e) => e,
            Err(_) => return Ok(false),
        };

        if stream.write_all(&encoded).await.is_err() {
            return Ok(false);
        }

        let (r, _w) = tokio::io::split(stream);
        let mut reader = a3s_transport::FrameReader::new(r);
        match reader.read_frame().await {
            Ok(Some(f)) if f.frame_type == a3s_transport::FrameType::Heartbeat => Ok(true),
            _ => Ok(false),
        }
    }
}

/// Handle for reading streaming exec events.
///
/// Reads frames from the exec server: Data frames contain `ExecChunk` (stdout/stderr),
/// Control frames contain `ExecExit` (final exit code).
pub struct StreamingExec {
    reader: a3s_transport::FrameReader<tokio::io::ReadHalf<tokio::net::UnixStream>>,
    started: std::time::Instant,
    stdout_bytes: u64,
    stderr_bytes: u64,
    done: bool,
}

impl StreamingExec {
    /// Read the next event from the stream.
    ///
    /// Returns `None` when the command has exited and all output has been read.
    pub async fn next_event(&mut self) -> Result<Option<a3s_box_core::exec::ExecEvent>> {
        use a3s_box_core::exec::{ExecChunk, ExecEvent, ExecExit};

        if self.done {
            return Ok(None);
        }

        let frame = match self.reader.read_frame().await {
            Ok(Some(f)) => f,
            Ok(None) => {
                self.done = true;
                return Ok(None);
            }
            Err(e) => {
                self.done = true;
                return Err(BoxError::ExecError(format!(
                    "Streaming exec read failed: {}",
                    e
                )));
            }
        };

        match frame.frame_type {
            a3s_transport::FrameType::Data => {
                // Data frame = ExecChunk (stdout/stderr)
                let chunk: ExecChunk = serde_json::from_slice(&frame.payload).map_err(|e| {
                    BoxError::ExecError(format!("Failed to parse exec chunk: {}", e))
                })?;
                match chunk.stream {
                    a3s_box_core::exec::StreamType::Stdout => {
                        self.stdout_bytes += chunk.data.len() as u64;
                    }
                    a3s_box_core::exec::StreamType::Stderr => {
                        self.stderr_bytes += chunk.data.len() as u64;
                    }
                }
                Ok(Some(ExecEvent::Chunk(chunk)))
            }
            a3s_transport::FrameType::Control => {
                // Control frame = ExecExit
                let exit: ExecExit = serde_json::from_slice(&frame.payload).map_err(|e| {
                    BoxError::ExecError(format!("Failed to parse exec exit: {}", e))
                })?;
                self.done = true;
                Ok(Some(ExecEvent::Exit(exit)))
            }
            a3s_transport::FrameType::Error => {
                let msg = String::from_utf8_lossy(&frame.payload);
                self.done = true;
                Err(BoxError::ExecError(format!(
                    "Streaming exec error: {}",
                    msg
                )))
            }
            _ => Err(BoxError::ExecError(format!(
                "Unexpected frame type in stream: {:?}",
                frame.frame_type
            ))),
        }
    }

    /// Collect all remaining output and return the final result with metrics.
    ///
    /// Consumes the stream, buffering all stdout/stderr until the command exits.
    pub async fn collect(
        mut self,
    ) -> Result<(
        a3s_box_core::exec::ExecOutput,
        a3s_box_core::exec::ExecMetrics,
    )> {
        use a3s_box_core::exec::{ExecEvent, ExecMetrics, ExecOutput};

        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let mut exit_code = -1;

        while let Some(event) = self.next_event().await? {
            match event {
                ExecEvent::Chunk(chunk) => match chunk.stream {
                    a3s_box_core::exec::StreamType::Stdout => stdout.extend_from_slice(&chunk.data),
                    a3s_box_core::exec::StreamType::Stderr => stderr.extend_from_slice(&chunk.data),
                },
                ExecEvent::Exit(exit) => {
                    exit_code = exit.exit_code;
                }
            }
        }

        let metrics = ExecMetrics {
            duration_ms: self.started.elapsed().as_millis() as u64,
            peak_memory_bytes: None,
            stdout_bytes: self.stdout_bytes,
            stderr_bytes: self.stderr_bytes,
        };

        let output = ExecOutput {
            stdout,
            stderr,
            exit_code,
        };

        Ok((output, metrics))
    }

    /// Whether the stream has finished (command exited or connection closed).
    pub fn is_done(&self) -> bool {
        self.done
    }

    /// Get execution metrics so far.
    pub fn metrics(&self) -> a3s_box_core::exec::ExecMetrics {
        a3s_box_core::exec::ExecMetrics {
            duration_ms: self.started.elapsed().as_millis() as u64,
            peak_memory_bytes: None,
            stdout_bytes: self.stdout_bytes,
            stderr_bytes: self.stderr_bytes,
        }
    }
}

impl std::fmt::Debug for StreamingExec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StreamingExec")
            .field("done", &self.done)
            .field("stdout_bytes", &self.stdout_bytes)
            .field("stderr_bytes", &self.stderr_bytes)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncReadExt;
    use tokio::net::UnixListener;

    #[tokio::test]
    async fn test_exec_connect_nonexistent_socket() {
        let result = ExecClient::connect(Path::new("/tmp/nonexistent-a3s-exec-test.sock")).await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(matches!(err, BoxError::ExecError(_)));
    }

    #[tokio::test]
    async fn test_exec_connect_and_socket_path() {
        let tmp = tempfile::TempDir::new().unwrap();
        let sock_path = tmp.path().join("exec.sock");
        let _listener = UnixListener::bind(&sock_path).unwrap();

        let client = ExecClient::connect(&sock_path).await.unwrap();
        assert_eq!(client.socket_path(), sock_path);
    }

    #[tokio::test]
    async fn test_exec_heartbeat_with_echo_server() {
        let tmp = tempfile::TempDir::new().unwrap();
        let sock_path = tmp.path().join("hb_echo.sock");
        let listener = UnixListener::bind(&sock_path).unwrap();

        tokio::spawn(async move {
            // Accept connect verification
            let (stream, _) = listener.accept().await.unwrap();
            drop(stream);
            // Accept heartbeat connection and echo back
            let (mut stream, _) = listener.accept().await.unwrap();
            // Read frame header
            let mut header = [0u8; 5];
            stream.read_exact(&mut header).await.unwrap();
            let len = u32::from_be_bytes([header[1], header[2], header[3], header[4]]) as usize;
            let mut payload = vec![0u8; len];
            if len > 0 {
                stream.read_exact(&mut payload).await.unwrap();
            }
            // Respond with Heartbeat frame
            let response = a3s_transport::Frame::heartbeat();
            let encoded = response.encode().unwrap();
            stream.write_all(&encoded).await.unwrap();
        });

        tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;

        let client = ExecClient::connect(&sock_path).await.unwrap();
        let result = client.heartbeat().await.unwrap();
        assert!(result);
    }

    #[tokio::test]
    async fn test_exec_heartbeat_no_response() {
        let tmp = tempfile::TempDir::new().unwrap();
        let sock_path = tmp.path().join("hb_close.sock");
        let listener = UnixListener::bind(&sock_path).unwrap();

        tokio::spawn(async move {
            // Accept connect verification
            let (stream, _) = listener.accept().await.unwrap();
            drop(stream);
            // Accept heartbeat connection, read request, then close
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut buf = vec![0u8; 1024];
            let _ = stream.read(&mut buf).await;
            drop(stream);
        });

        tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;

        let client = ExecClient::connect(&sock_path).await.unwrap();
        let result = client.heartbeat().await.unwrap();
        assert!(!result);
    }

    #[tokio::test]
    async fn test_exec_heartbeat_nonexistent_socket() {
        // heartbeat() on a non-connectable socket should return false, not error
        let client = ExecClient {
            socket_path: PathBuf::from("/tmp/nonexistent-hb-test.sock"),
        };
        let result = client.heartbeat().await.unwrap();
        assert!(!result);
    }

    #[tokio::test]
    async fn test_exec_client_exec_command() {
        let tmp = tempfile::TempDir::new().unwrap();
        let sock_path = tmp.path().join("exec_cmd.sock");
        let listener = UnixListener::bind(&sock_path).unwrap();

        tokio::spawn(async move {
            // Accept connect verification
            let (stream, _) = listener.accept().await.unwrap();
            drop(stream);
            // Accept exec request — read Frame, respond with Frame
            let (stream, _) = listener.accept().await.unwrap();
            let (r, w) = tokio::io::split(stream);
            let mut reader = a3s_transport::FrameReader::new(r);
            let mut writer = a3s_transport::FrameWriter::new(w);

            // Read request frame
            let _frame = reader.read_frame().await.unwrap().unwrap();

            // Send response as Data frame
            let output = a3s_box_core::exec::ExecOutput {
                stdout: b"hello\n".to_vec(),
                stderr: vec![],
                exit_code: 0,
            };
            let payload = serde_json::to_vec(&output).unwrap();
            writer.write_data(&payload).await.unwrap();
        });

        tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;

        let client = ExecClient::connect(&sock_path).await.unwrap();
        let req = a3s_box_core::exec::ExecRequest {
            cmd: vec!["echo".to_string(), "hello".to_string()],
            env: vec![],
            working_dir: None,
            user: None,
            stdin: None,
            timeout_ns: 0,
            streaming: false,
        };
        let output = client.exec_command(&req).await.unwrap();
        assert_eq!(output.exit_code, 0);
        assert_eq!(&output.stdout[..], b"hello\n");
        assert!(output.stderr.is_empty());
    }

    #[tokio::test]
    async fn test_exec_client_malformed_response() {
        let tmp = tempfile::TempDir::new().unwrap();
        let sock_path = tmp.path().join("exec_bad.sock");
        let listener = UnixListener::bind(&sock_path).unwrap();

        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            drop(stream);
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut buf = vec![0u8; 4096];
            let _ = stream.read(&mut buf).await;
            // Send garbage — not a valid frame
            stream.write_all(b"garbage").await.unwrap();
            drop(stream);
        });

        tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;

        let client = ExecClient::connect(&sock_path).await.unwrap();
        let req = a3s_box_core::exec::ExecRequest {
            cmd: vec!["test".to_string()],
            env: vec![],
            working_dir: None,
            user: None,
            stdin: None,
            timeout_ns: 0,
            streaming: false,
        };
        let result = client.exec_command(&req).await;
        assert!(result.is_err());
    }
}
