//! Wire protocol frame handling for RA-TLS connections.

use std::io::{Read, Write};

/// Read a single frame from a synchronous stream.
/// Returns (frame_type, payload) or None on EOF.
pub(super) fn read_frame(r: &mut impl Read) -> std::io::Result<Option<(u8, Vec<u8>)>> {
    let mut header = [0u8; 5];
    match r.read_exact(&mut header) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    let frame_type = header[0];
    let len = u32::from_be_bytes([header[1], header[2], header[3], header[4]]) as usize;
    let mut payload = vec![0u8; len];
    if len > 0 {
        r.read_exact(&mut payload)?;
    }
    Ok(Some((frame_type, payload)))
}

/// Write a frame to a synchronous stream.
pub(super) fn write_frame(
    w: &mut impl Write,
    frame_type: u8,
    payload: &[u8],
) -> std::io::Result<()> {
    let len = payload.len() as u32;
    let mut header = [0u8; 5];
    header[0] = frame_type;
    header[1..5].copy_from_slice(&len.to_be_bytes());
    w.write_all(&header)?;
    if !payload.is_empty() {
        w.write_all(payload)?;
    }
    Ok(())
}

/// Send a Data frame response (success).
pub(super) fn send_data_response(tls: &mut impl Write, body: &[u8]) {
    let _ = write_frame(tls, 0x01, body); // FrameType::Data
}

/// Send an Error frame response.
pub(super) fn send_error_response(tls: &mut impl Write, message: &str) {
    let _ = write_frame(tls, 0x04, message.as_bytes()); // FrameType::Error
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Cursor, ErrorKind};

    #[test]
    fn write_then_read_frame_round_trips_type_and_payload() {
        let mut bytes = Vec::new();
        write_frame(&mut bytes, 0x02, b"hello").unwrap();

        assert_eq!(&bytes[..5], &[0x02, 0, 0, 0, 5]);
        let mut cursor = Cursor::new(bytes);
        let frame = read_frame(&mut cursor).unwrap().unwrap();

        assert_eq!(frame, (0x02, b"hello".to_vec()));
    }

    #[test]
    fn read_frame_supports_empty_payload() {
        let mut bytes = Vec::new();
        write_frame(&mut bytes, 0x03, b"").unwrap();

        let mut cursor = Cursor::new(bytes);
        let frame = read_frame(&mut cursor).unwrap().unwrap();

        assert_eq!(frame, (0x03, Vec::new()));
    }

    #[test]
    fn read_frame_returns_none_on_clean_eof_before_header() {
        let mut cursor = Cursor::new(Vec::<u8>::new());

        assert!(read_frame(&mut cursor).unwrap().is_none());
    }

    #[test]
    fn read_frame_errors_on_truncated_payload() {
        let mut cursor = Cursor::new(vec![0x01, 0, 0, 0, 4, b'o', b'k']);

        let err = read_frame(&mut cursor).unwrap_err();

        assert_eq!(err.kind(), ErrorKind::UnexpectedEof);
    }

    #[test]
    fn response_helpers_emit_expected_frame_types() {
        let mut data = Vec::new();
        send_data_response(&mut data, b"body");
        let mut data_cursor = Cursor::new(data);
        assert_eq!(
            read_frame(&mut data_cursor).unwrap().unwrap(),
            (0x01, b"body".to_vec())
        );

        let mut error = Vec::new();
        send_error_response(&mut error, "nope");
        let mut error_cursor = Cursor::new(error);
        assert_eq!(
            read_frame(&mut error_cursor).unwrap().unwrap(),
            (0x04, b"nope".to_vec())
        );
    }
}
