//! PTY terminal client for interactive sessions.

use std::path::Path;

use a3s_box_core::error::{BoxError, Result};

/// Client for interactive PTY sessions in the guest over Unix socket.
///
/// Connects to the PTY server (vsock port 4090) and provides async
/// frame-based communication for bidirectional terminal I/O.
/// Uses `a3s_transport::FrameReader`/`FrameWriter` for wire I/O.
#[derive(Debug)]
pub struct PtyClient {
    reader: a3s_transport::FrameReader<tokio::io::ReadHalf<tokio::net::UnixStream>>,
    writer: a3s_transport::FrameWriter<tokio::io::WriteHalf<tokio::net::UnixStream>>,
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

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::UnixListener;

    #[tokio::test]
    async fn test_pty_client_connect_nonexistent() {
        let result = PtyClient::connect(Path::new("/tmp/nonexistent-pty-test.sock")).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_pty_frame_roundtrip() {
        let tmp = tempfile::TempDir::new().unwrap();
        let sock_path = tmp.path().join("pty.sock");
        let listener = UnixListener::bind(&sock_path).unwrap();

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
        let listener = UnixListener::bind(&sock_path).unwrap();

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
        let listener = UnixListener::bind(&sock_path).unwrap();

        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            drop(stream); // Close immediately → EOF
        });

        let mut client = PtyClient::connect(&sock_path).await.unwrap();
        let frame = client.read_frame().await.unwrap();
        assert!(frame.is_none()); // EOF
    }
}
