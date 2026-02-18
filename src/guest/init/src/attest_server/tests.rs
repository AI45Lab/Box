//! Tests for RA-TLS attestation server.

use super::*;

#[test]
fn test_attest_vsock_port_constant() {
    assert_eq!(ATTEST_VSOCK_PORT, a3s_transport::ports::TEE_CHANNEL);
}

#[test]
fn test_is_simulate_mode_default() {
    // Should be false unless env var is set
    // (don't set it in tests to avoid side effects)
    let _ = handlers::is_simulate_mode();
}

#[test]
fn test_build_simulated_report_size() {
    let data = [0u8; SNP_USER_DATA_SIZE];
    let report = handlers::build_simulated_report(&data);
    assert_eq!(report.len(), 1184);
}

#[test]
fn test_build_simulated_report_version() {
    let data = [0u8; SNP_USER_DATA_SIZE];
    let report = handlers::build_simulated_report(&data);
    let version = u32::from_le_bytes(report[0..4].try_into().unwrap());
    assert_eq!(version, 0xA3);
}

#[test]
fn test_build_simulated_report_contains_report_data() {
    let mut data = [0u8; SNP_USER_DATA_SIZE];
    data[0..4].copy_from_slice(&[0xDE, 0xAD, 0xBE, 0xEF]);
    let report = handlers::build_simulated_report(&data);
    assert_eq!(&report[0x50..0x54], &[0xDE, 0xAD, 0xBE, 0xEF]);
}

#[test]
fn test_oid_constants() {
    assert_eq!(OID_SNP_REPORT, &[1, 3, 6, 1, 4, 1, 58270, 1, 1]);
    assert_eq!(OID_CERT_CHAIN, &[1, 3, 6, 1, 4, 1, 58270, 1, 2]);
}

#[test]
#[cfg(target_os = "linux")]
fn test_guid_bytes() {
    let guid = snp::guid_bytes("63da758d-e664-4564-adc5-f4b93be8accd");
    assert_eq!(guid[0], 0x8d);
    assert_eq!(guid[1], 0x75);
    assert_eq!(guid[2], 0xda);
    assert_eq!(guid[3], 0x63);
}

#[test]
#[cfg(target_os = "linux")]
fn test_hex_val() {
    assert_eq!(snp::hex_val(b'0'), 0);
    assert_eq!(snp::hex_val(b'9'), 9);
    assert_eq!(snp::hex_val(b'a'), 10);
    assert_eq!(snp::hex_val(b'f'), 15);
}

#[test]
fn test_valid_secret_names() {
    assert!(handlers::is_valid_secret_name("API_KEY"));
    assert!(handlers::is_valid_secret_name("my-secret"));
    assert!(handlers::is_valid_secret_name("config.json"));
    assert!(handlers::is_valid_secret_name("a"));
    assert!(handlers::is_valid_secret_name("SECRET_123"));
}

#[test]
fn test_invalid_secret_names() {
    assert!(!handlers::is_valid_secret_name(""));
    assert!(!handlers::is_valid_secret_name(".hidden"));
    assert!(!handlers::is_valid_secret_name("path/traversal"));
    assert!(!handlers::is_valid_secret_name("has space"));
    assert!(!handlers::is_valid_secret_name("null\0byte"));
    assert!(!handlers::is_valid_secret_name(&"x".repeat(257)));
}

#[test]
fn test_secrets_dir_constant() {
    assert_eq!(handlers::SECRETS_DIR, "/run/secrets");
}

#[test]
fn test_hkdf_salt_matches_runtime() {
    assert_eq!(handlers::HKDF_SALT, b"a3s-sealed-storage-v1");
}

/// Build a fake 1184-byte report with known measurement and chip_id.
fn make_test_report() -> Vec<u8> {
    let mut report = vec![0u8; 1184];
    for i in 0..48 {
        report[0x90 + i] = (i as u8).wrapping_mul(0xA3);
    }
    for b in &mut report[0x1A0..0x1E0] {
        *b = 0xA3;
    }
    report
}

#[test]
fn test_derive_guest_sealing_key_measurement_and_chip() {
    let report = make_test_report();
    let key =
        handlers::derive_guest_sealing_key(&report, "test-ctx", "MeasurementAndChip").unwrap();
    assert_eq!(key.len(), 32);
    // Key should be deterministic
    let key2 =
        handlers::derive_guest_sealing_key(&report, "test-ctx", "MeasurementAndChip").unwrap();
    assert_eq!(key, key2);
}

#[test]
fn test_derive_guest_sealing_key_measurement_only() {
    let report = make_test_report();
    let key = handlers::derive_guest_sealing_key(&report, "ctx", "MeasurementOnly").unwrap();
    assert_eq!(key.len(), 32);
}

#[test]
fn test_derive_guest_sealing_key_chip_only() {
    let report = make_test_report();
    let key = handlers::derive_guest_sealing_key(&report, "ctx", "ChipOnly").unwrap();
    assert_eq!(key.len(), 32);
}

#[test]
fn test_derive_guest_sealing_key_different_contexts() {
    let report = make_test_report();
    let key_a =
        handlers::derive_guest_sealing_key(&report, "context-a", "MeasurementAndChip").unwrap();
    let key_b =
        handlers::derive_guest_sealing_key(&report, "context-b", "MeasurementAndChip").unwrap();
    assert_ne!(key_a, key_b);
}

#[test]
fn test_derive_guest_sealing_key_different_policies() {
    let report = make_test_report();
    let key_mc = handlers::derive_guest_sealing_key(&report, "ctx", "MeasurementAndChip").unwrap();
    let key_m = handlers::derive_guest_sealing_key(&report, "ctx", "MeasurementOnly").unwrap();
    let key_c = handlers::derive_guest_sealing_key(&report, "ctx", "ChipOnly").unwrap();
    assert_ne!(key_mc, key_m);
    assert_ne!(key_mc, key_c);
    assert_ne!(key_m, key_c);
}

#[test]
fn test_derive_guest_sealing_key_report_too_short() {
    let short = vec![0u8; 100];
    let result = handlers::derive_guest_sealing_key(&short, "ctx", "MeasurementAndChip");
    assert!(result.is_err());
}

#[test]
fn test_derive_guest_sealing_key_unknown_policy_defaults() {
    let report = make_test_report();
    // Unknown policy falls back to MeasurementAndChip
    let key_unknown = handlers::derive_guest_sealing_key(&report, "ctx", "Unknown").unwrap();
    let key_default =
        handlers::derive_guest_sealing_key(&report, "ctx", "MeasurementAndChip").unwrap();
    assert_eq!(key_unknown, key_default);
}

#[test]
fn test_derive_guest_sealing_key_chip_only_survives_measurement_change() {
    let report = make_test_report();
    let key1 = handlers::derive_guest_sealing_key(&report, "ctx", "ChipOnly").unwrap();

    let mut changed = report.clone();
    changed[0x90] = 0xFF; // change measurement
    let key2 = handlers::derive_guest_sealing_key(&changed, "ctx", "ChipOnly").unwrap();
    assert_eq!(key1, key2);
}

#[test]
fn test_derive_guest_sealing_key_measurement_only_survives_chip_change() {
    let report = make_test_report();
    let key1 = handlers::derive_guest_sealing_key(&report, "ctx", "MeasurementOnly").unwrap();

    let mut changed = report.clone();
    changed[0x1A0] = 0xFF; // change chip_id
    let key2 = handlers::derive_guest_sealing_key(&changed, "ctx", "MeasurementOnly").unwrap();
    assert_eq!(key1, key2);
}

#[test]
fn test_process_request_deserialization() {
    let json = r#"{"session_id":"s1","content":"hello","request_type":"process_message"}"#;
    let _: serde_json::Value = serde_json::from_str(json).unwrap();
}

#[test]
fn test_process_request_default_type() {
    let json = r#"{"session_id":"s1","content":"hello"}"#;
    let val: serde_json::Value = serde_json::from_str(json).unwrap();
    assert_eq!(val["session_id"], "s1");
    assert_eq!(val["content"], "hello");
}

#[test]
fn test_process_response_serialization() {
    let resp = serde_json::json!({
        "session_id": "s1",
        "content": "response text",
        "success": true,
    });
    let json = serde_json::to_string(&resp).unwrap();
    assert!(json.contains("response text"));
    assert!(json.contains("\"success\":true"));
}

#[test]
fn test_process_response_with_error() {
    let resp = serde_json::json!({
        "session_id": "s1",
        "content": "",
        "success": false,
        "error": "agent unreachable",
    });
    let json = serde_json::to_string(&resp).unwrap();
    assert!(json.contains("agent unreachable"));
    assert!(json.contains("\"success\":false"));
}

#[test]
fn test_frame_roundtrip() {
    let payload = b"hello frame";
    let mut buf = Vec::new();
    frame::write_frame(&mut buf, 0x01, payload).unwrap();
    let mut cursor = std::io::Cursor::new(buf);
    let (ft, data) = frame::read_frame(&mut cursor).unwrap().unwrap();
    assert_eq!(ft, 0x01);
    assert_eq!(data, payload);
}

#[test]
fn test_frame_read_eof() {
    let mut cursor = std::io::Cursor::new(Vec::<u8>::new());
    assert!(frame::read_frame(&mut cursor).unwrap().is_none());
}

#[test]
fn test_frame_empty_payload() {
    let mut buf = Vec::new();
    frame::write_frame(&mut buf, 0x04, b"").unwrap();
    let mut cursor = std::io::Cursor::new(buf);
    let (ft, data) = frame::read_frame(&mut cursor).unwrap().unwrap();
    assert_eq!(ft, 0x04);
    assert!(data.is_empty());
}

#[test]
fn test_send_data_response() {
    let mut buf = Vec::new();
    frame::send_data_response(&mut buf, b"{\"ok\":true}");
    let mut cursor = std::io::Cursor::new(buf);
    let (ft, data) = frame::read_frame(&mut cursor).unwrap().unwrap();
    assert_eq!(ft, 0x01); // Data
    assert_eq!(data, b"{\"ok\":true}");
}

#[test]
fn test_send_error_response() {
    let mut buf = Vec::new();
    frame::send_error_response(&mut buf, "something failed");
    let mut cursor = std::io::Cursor::new(buf);
    let (ft, data) = frame::read_frame(&mut cursor).unwrap().unwrap();
    assert_eq!(ft, 0x04); // Error
    assert_eq!(data, b"something failed");
}
