//! PTY protocol types for interactive terminal sessions.
//!
//! Defines a binary framing protocol for bidirectional PTY communication
//! between the host CLI and the guest PTY server over vsock port 4090.
//!
//! Wire format: `[type: u8] [length: u32 BE] [payload: length bytes]`
//! (same as `a3s-transport::Frame`)

use serde::{Deserialize, Serialize};
use std::io;

/// Vsock port for the PTY server.
pub const PTY_VSOCK_PORT: u32 = a3s_transport::ports::PTY_SERVER;

/// Maximum frame payload size: 64 KiB.
pub const MAX_FRAME_PAYLOAD: usize = 64 * 1024;

/// Frame type: PTY session request (host → guest).
pub const FRAME_PTY_REQUEST: u8 = 0x01;
/// Frame type: terminal data (bidirectional).
pub const FRAME_PTY_DATA: u8 = 0x02;
/// Frame type: terminal resize (host → guest).
pub const FRAME_PTY_RESIZE: u8 = 0x03;
/// Frame type: process exited (guest → host).
pub const FRAME_PTY_EXIT: u8 = 0x04;
/// Frame type: error message (guest → host).
pub const FRAME_PTY_ERROR: u8 = 0x05;

// Re-export low-level frame I/O from a3s-transport (identical wire format).
// The exec_server already uses its own copy; PTY server and core share these.
pub use a3s_transport::frame::Frame as TransportFrame;

/// Request to open an interactive PTY session in the guest.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PtyRequest {
    /// Command and arguments (e.g., ["/bin/sh"]).
    pub cmd: Vec<String>,
    /// Additional environment variables (KEY=VALUE pairs).
    #[serde(default)]
    pub env: Vec<String>,
    /// Working directory for the command.
    #[serde(default)]
    pub working_dir: Option<String>,
    /// Optional guest-visible rootfs path to chroot into before executing.
    #[serde(default)]
    pub rootfs: Option<String>,
    /// User to run the command as (supported: "root", "1000", "1000:1000").
    #[serde(default)]
    pub user: Option<String>,
    /// Terminal width in columns.
    pub cols: u16,
    /// Terminal height in rows.
    pub rows: u16,
}

/// Terminal resize notification.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PtyResize {
    pub cols: u16,
    pub rows: u16,
}

/// Process exit notification.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PtyExit {
    pub exit_code: i32,
}

/// A parsed protocol frame.
#[derive(Debug)]
pub enum PtyFrame {
    Request(PtyRequest),
    Data(Vec<u8>),
    Resize(PtyResize),
    Exit(PtyExit),
    Error(String),
}

/// Write a frame to a stream: [type: u8] [length: u32 BE] [payload].
pub fn write_frame(w: &mut impl io::Write, frame_type: u8, payload: &[u8]) -> io::Result<()> {
    let len = payload.len() as u32;
    w.write_all(&[frame_type])?;
    w.write_all(&len.to_be_bytes())?;
    w.write_all(payload)?;
    w.flush()
}

/// Read a raw frame from a stream. Returns (frame_type, payload).
///
/// Returns `Ok(None)` on EOF.
pub fn read_frame(r: &mut impl io::Read) -> io::Result<Option<(u8, Vec<u8>)>> {
    let mut header = [0u8; 5];
    match r.read_exact(&mut header) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }

    let frame_type = header[0];
    let len = u32::from_be_bytes([header[1], header[2], header[3], header[4]]) as usize;

    if len > MAX_FRAME_PAYLOAD {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "PTY frame too large: {} bytes (max {})",
                len, MAX_FRAME_PAYLOAD
            ),
        ));
    }

    let mut payload = vec![0u8; len];
    if len > 0 {
        r.read_exact(&mut payload)?;
    }

    Ok(Some((frame_type, payload)))
}

/// Write a PtyRequest frame.
pub fn write_request(w: &mut impl io::Write, req: &PtyRequest) -> io::Result<()> {
    let payload = serde_json::to_vec(req).map_err(|e| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("Failed to serialize PtyRequest: {}", e),
        )
    })?;
    write_frame(w, FRAME_PTY_REQUEST, &payload)
}

/// Write a PtyData frame.
pub fn write_data(w: &mut impl io::Write, data: &[u8]) -> io::Result<()> {
    write_frame(w, FRAME_PTY_DATA, data)
}

/// Write a PtyResize frame.
pub fn write_resize(w: &mut impl io::Write, cols: u16, rows: u16) -> io::Result<()> {
    let resize = PtyResize { cols, rows };
    let payload = serde_json::to_vec(&resize).map_err(|e| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("Failed to serialize PtyResize: {}", e),
        )
    })?;
    write_frame(w, FRAME_PTY_RESIZE, &payload)
}

/// Write a PtyExit frame.
pub fn write_exit(w: &mut impl io::Write, exit_code: i32) -> io::Result<()> {
    let exit = PtyExit { exit_code };
    let payload = serde_json::to_vec(&exit).map_err(|e| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("Failed to serialize PtyExit: {}", e),
        )
    })?;
    write_frame(w, FRAME_PTY_EXIT, &payload)
}

/// Write a PtyError frame.
pub fn write_error(w: &mut impl io::Write, message: &str) -> io::Result<()> {
    write_frame(w, FRAME_PTY_ERROR, message.as_bytes())
}

/// Parse a raw frame into a typed PtyFrame.
pub fn parse_frame(frame_type: u8, payload: Vec<u8>) -> io::Result<PtyFrame> {
    match frame_type {
        FRAME_PTY_REQUEST => {
            let req: PtyRequest = serde_json::from_slice(&payload).map_err(|e| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("Invalid PtyRequest: {}", e),
                )
            })?;
            Ok(PtyFrame::Request(req))
        }
        FRAME_PTY_DATA => Ok(PtyFrame::Data(payload)),
        FRAME_PTY_RESIZE => {
            let resize: PtyResize = serde_json::from_slice(&payload).map_err(|e| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("Invalid PtyResize: {}", e),
                )
            })?;
            Ok(PtyFrame::Resize(resize))
        }
        FRAME_PTY_EXIT => {
            let exit: PtyExit = serde_json::from_slice(&payload).map_err(|e| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("Invalid PtyExit: {}", e),
                )
            })?;
            Ok(PtyFrame::Exit(exit))
        }
        FRAME_PTY_ERROR => {
            let msg = String::from_utf8_lossy(&payload).to_string();
            Ok(PtyFrame::Error(msg))
        }
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("Unknown PTY frame type: 0x{:02x}", frame_type),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_frame_roundtrip_data() {
        let mut buf = Vec::new();
        write_data(&mut buf, b"hello world").unwrap();

        let mut cursor = std::io::Cursor::new(buf);
        let (ft, payload) = read_frame(&mut cursor).unwrap().unwrap();
        assert_eq!(ft, FRAME_PTY_DATA);
        assert_eq!(payload, b"hello world");
    }

    #[test]
    fn test_frame_roundtrip_request() {
        let req = PtyRequest {
            cmd: vec!["/bin/sh".to_string()],
            env: vec!["TERM=xterm".to_string()],
            working_dir: Some("/home".to_string()),
            rootfs: Some("/run/a3s/cri/container-rootfs/sb/c/rootfs".to_string()),
            user: None,
            cols: 80,
            rows: 24,
        };

        let mut buf = Vec::new();
        write_request(&mut buf, &req).unwrap();

        let mut cursor = std::io::Cursor::new(buf);
        let (ft, payload) = read_frame(&mut cursor).unwrap().unwrap();
        assert_eq!(ft, FRAME_PTY_REQUEST);

        let parsed = match parse_frame(ft, payload).unwrap() {
            PtyFrame::Request(r) => r,
            other => panic!("Expected Request, got {:?}", other),
        };
        assert_eq!(parsed.cmd, vec!["/bin/sh"]);
        assert_eq!(
            parsed.rootfs,
            Some("/run/a3s/cri/container-rootfs/sb/c/rootfs".to_string())
        );
        assert_eq!(parsed.cols, 80);
        assert_eq!(parsed.rows, 24);
    }

    #[test]
    fn test_frame_roundtrip_resize() {
        let mut buf = Vec::new();
        write_resize(&mut buf, 120, 40).unwrap();

        let mut cursor = std::io::Cursor::new(buf);
        let (ft, payload) = read_frame(&mut cursor).unwrap().unwrap();
        let frame = parse_frame(ft, payload).unwrap();
        match frame {
            PtyFrame::Resize(r) => {
                assert_eq!(r.cols, 120);
                assert_eq!(r.rows, 40);
            }
            other => panic!("Expected Resize, got {:?}", other),
        }
    }

    #[test]
    fn test_frame_roundtrip_exit() {
        let mut buf = Vec::new();
        write_exit(&mut buf, 42).unwrap();

        let mut cursor = std::io::Cursor::new(buf);
        let (ft, payload) = read_frame(&mut cursor).unwrap().unwrap();
        let frame = parse_frame(ft, payload).unwrap();
        match frame {
            PtyFrame::Exit(e) => assert_eq!(e.exit_code, 42),
            other => panic!("Expected Exit, got {:?}", other),
        }
    }

    #[test]
    fn test_frame_roundtrip_error() {
        let mut buf = Vec::new();
        write_error(&mut buf, "something went wrong").unwrap();

        let mut cursor = std::io::Cursor::new(buf);
        let (ft, payload) = read_frame(&mut cursor).unwrap().unwrap();
        let frame = parse_frame(ft, payload).unwrap();
        match frame {
            PtyFrame::Error(msg) => assert_eq!(msg, "something went wrong"),
            other => panic!("Expected Error, got {:?}", other),
        }
    }

    #[test]
    fn test_read_frame_eof() {
        let mut cursor = std::io::Cursor::new(Vec::<u8>::new());
        let result = read_frame(&mut cursor).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn test_frame_too_large() {
        let mut buf = Vec::new();
        buf.push(FRAME_PTY_DATA);
        let huge_len = (MAX_FRAME_PAYLOAD as u32) + 1;
        buf.extend_from_slice(&huge_len.to_be_bytes());

        let mut cursor = std::io::Cursor::new(buf);
        let result = read_frame(&mut cursor);
        assert!(result.is_err());
    }

    #[test]
    fn test_unknown_frame_type() {
        let result = parse_frame(0xFF, vec![]);
        assert!(result.is_err());
    }

    #[test]
    fn test_empty_data_frame() {
        let mut buf = Vec::new();
        write_data(&mut buf, b"").unwrap();

        let mut cursor = std::io::Cursor::new(buf);
        let (ft, payload) = read_frame(&mut cursor).unwrap().unwrap();
        assert_eq!(ft, FRAME_PTY_DATA);
        assert!(payload.is_empty());
    }

    #[test]
    fn test_pty_request_default_fields() {
        let json = r#"{"cmd":["/bin/sh"],"cols":80,"rows":24}"#;
        let req: PtyRequest = serde_json::from_str(json).unwrap();
        assert!(req.env.is_empty());
        assert!(req.working_dir.is_none());
        assert!(req.rootfs.is_none());
        assert!(req.user.is_none());
    }

    #[test]
    fn test_constants() {
        assert_eq!(PTY_VSOCK_PORT, 4090);
        assert_eq!(FRAME_PTY_REQUEST, 0x01);
        assert_eq!(FRAME_PTY_DATA, 0x02);
        assert_eq!(FRAME_PTY_RESIZE, 0x03);
        assert_eq!(FRAME_PTY_EXIT, 0x04);
        assert_eq!(FRAME_PTY_ERROR, 0x05);
    }
}
