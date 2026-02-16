//! Exec types for host-to-guest command execution.
//!
//! Shared request/response types used by both the guest exec server
//! and the host exec client.

use serde::{Deserialize, Serialize};

/// Default exec timeout: 5 seconds.
pub const DEFAULT_EXEC_TIMEOUT_NS: u64 = 5_000_000_000;

/// Maximum output size per stream (stdout/stderr): 16 MiB.
pub const MAX_OUTPUT_BYTES: usize = 16 * 1024 * 1024;

/// Frame type byte for streaming exec chunks.
pub const FRAME_EXEC_CHUNK: u8 = 0x01;

/// Frame type byte for streaming exec exit.
pub const FRAME_EXEC_EXIT: u8 = 0x02;

/// Request to execute a command in the guest.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecRequest {
    /// Command and arguments (e.g., ["ls", "-la"]).
    pub cmd: Vec<String>,
    /// Timeout in nanoseconds. 0 means use the default.
    pub timeout_ns: u64,
    /// Additional environment variables (KEY=VALUE pairs).
    #[serde(default)]
    pub env: Vec<String>,
    /// Working directory for the command.
    #[serde(default)]
    pub working_dir: Option<String>,
    /// Optional stdin data to pipe to the command.
    #[serde(default)]
    pub stdin: Option<Vec<u8>>,
    /// User to run the command as (e.g., "root", "1000", "1000:1000").
    #[serde(default)]
    pub user: Option<String>,
    /// Enable streaming mode (receive output chunks as they arrive).
    #[serde(default)]
    pub streaming: bool,
}

/// Output from an executed command.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecOutput {
    /// Captured stdout bytes.
    pub stdout: Vec<u8>,
    /// Captured stderr bytes.
    pub stderr: Vec<u8>,
    /// Process exit code.
    pub exit_code: i32,
}

/// Which output stream a chunk belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum StreamType {
    /// Standard output.
    Stdout,
    /// Standard error.
    Stderr,
}

impl std::fmt::Display for StreamType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StreamType::Stdout => write!(f, "stdout"),
            StreamType::Stderr => write!(f, "stderr"),
        }
    }
}

/// A chunk of streaming output from a running command.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecChunk {
    /// Which stream this chunk belongs to.
    pub stream: StreamType,
    /// Raw output bytes.
    pub data: Vec<u8>,
}

/// Final exit notification from a streaming exec.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecExit {
    /// Process exit code.
    pub exit_code: i32,
}

/// A streaming exec event — either a chunk of output or the final exit.
#[derive(Debug, Clone)]
pub enum ExecEvent {
    /// A chunk of stdout or stderr data.
    Chunk(ExecChunk),
    /// The command has exited.
    Exit(ExecExit),
}

/// Metrics collected during command execution.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ExecMetrics {
    /// Wall-clock duration in milliseconds.
    pub duration_ms: u64,
    /// Peak memory usage in bytes (if available).
    #[serde(default)]
    pub peak_memory_bytes: Option<u64>,
    /// Total stdout bytes produced.
    pub stdout_bytes: u64,
    /// Total stderr bytes produced.
    pub stderr_bytes: u64,
}

/// File transfer request for upload/download between host and guest.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileRequest {
    /// Operation type.
    pub op: FileOp,
    /// Path inside the guest.
    pub guest_path: String,
    /// File content (for upload only, base64-encoded).
    #[serde(default)]
    pub data: Option<String>,
}

/// File transfer operation type.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum FileOp {
    /// Upload a file from host to guest.
    Upload,
    /// Download a file from guest to host.
    Download,
}

/// File transfer response.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileResponse {
    /// Whether the operation succeeded.
    pub success: bool,
    /// File content (for download only, base64-encoded).
    #[serde(default)]
    pub data: Option<String>,
    /// File size in bytes.
    #[serde(default)]
    pub size: u64,
    /// Error message if the operation failed.
    #[serde(default)]
    pub error: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_exec_request_serialization_roundtrip() {
        let req = ExecRequest {
            cmd: vec!["ls".to_string(), "-la".to_string()],
            timeout_ns: 3_000_000_000,
            env: vec!["FOO=bar".to_string()],
            working_dir: Some("/tmp".to_string()),
            stdin: None,
            user: None,
            streaming: false,
        };
        let json = serde_json::to_string(&req).unwrap();
        let parsed: ExecRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.cmd, vec!["ls", "-la"]);
        assert_eq!(parsed.timeout_ns, 3_000_000_000);
        assert_eq!(parsed.env, vec!["FOO=bar"]);
        assert_eq!(parsed.working_dir, Some("/tmp".to_string()));
        assert!(parsed.stdin.is_none());
        assert!(parsed.user.is_none());
        assert!(!parsed.streaming);
    }

    #[test]
    fn test_exec_request_streaming_flag() {
        let req = ExecRequest {
            cmd: vec!["tail".to_string(), "-f".to_string()],
            timeout_ns: 0,
            env: vec![],
            working_dir: None,
            stdin: None,
            user: None,
            streaming: true,
        };
        let json = serde_json::to_string(&req).unwrap();
        let parsed: ExecRequest = serde_json::from_str(&json).unwrap();
        assert!(parsed.streaming);
    }

    #[test]
    fn test_exec_output_serialization_roundtrip() {
        let output = ExecOutput {
            stdout: b"hello\n".to_vec(),
            stderr: b"warning\n".to_vec(),
            exit_code: 0,
        };
        let json = serde_json::to_string(&output).unwrap();
        let parsed: ExecOutput = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.stdout, b"hello\n");
        assert_eq!(parsed.stderr, b"warning\n");
        assert_eq!(parsed.exit_code, 0);
    }

    #[test]
    fn test_exec_output_non_zero_exit() {
        let output = ExecOutput {
            stdout: vec![],
            stderr: b"not found\n".to_vec(),
            exit_code: 127,
        };
        let json = serde_json::to_string(&output).unwrap();
        let parsed: ExecOutput = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.exit_code, 127);
        assert!(parsed.stdout.is_empty());
    }

    #[test]
    fn test_default_timeout_constant() {
        assert_eq!(DEFAULT_EXEC_TIMEOUT_NS, 5_000_000_000);
    }

    #[test]
    fn test_max_output_bytes_constant() {
        assert_eq!(MAX_OUTPUT_BYTES, 16 * 1024 * 1024);
    }

    #[test]
    fn test_exec_request_empty_cmd() {
        let req = ExecRequest {
            cmd: vec![],
            timeout_ns: 0,
            env: vec![],
            working_dir: None,
            stdin: None,
            user: None,
            streaming: false,
        };
        let json = serde_json::to_string(&req).unwrap();
        let parsed: ExecRequest = serde_json::from_str(&json).unwrap();
        assert!(parsed.cmd.is_empty());
        assert_eq!(parsed.timeout_ns, 0);
        assert!(parsed.env.is_empty());
        assert!(parsed.working_dir.is_none());
        assert!(parsed.user.is_none());
    }

    #[test]
    fn test_exec_request_backward_compatible_deserialization() {
        // Old format without streaming field should still parse (defaults to false)
        let json = r#"{"cmd":["ls"],"timeout_ns":0}"#;
        let parsed: ExecRequest = serde_json::from_str(json).unwrap();
        assert_eq!(parsed.cmd, vec!["ls"]);
        assert!(parsed.env.is_empty());
        assert!(parsed.working_dir.is_none());
        assert!(parsed.stdin.is_none());
        assert!(parsed.user.is_none());
        assert!(!parsed.streaming);
    }

    #[test]
    fn test_exec_request_with_stdin() {
        let req = ExecRequest {
            cmd: vec!["sh".to_string()],
            timeout_ns: 0,
            env: vec![],
            working_dir: None,
            stdin: Some(b"echo hello\n".to_vec()),
            user: None,
            streaming: false,
        };
        let json = serde_json::to_string(&req).unwrap();
        let parsed: ExecRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.stdin, Some(b"echo hello\n".to_vec()));
    }

    #[test]
    fn test_exec_request_with_user() {
        let req = ExecRequest {
            cmd: vec!["whoami".to_string()],
            timeout_ns: 0,
            env: vec![],
            working_dir: None,
            stdin: None,
            user: Some("root".to_string()),
            streaming: false,
        };
        let json = serde_json::to_string(&req).unwrap();
        let parsed: ExecRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.user, Some("root".to_string()));
    }

    #[test]
    fn test_exec_request_with_user_uid_gid() {
        let req = ExecRequest {
            cmd: vec!["id".to_string()],
            timeout_ns: 0,
            env: vec![],
            working_dir: None,
            stdin: None,
            user: Some("1000:1000".to_string()),
            streaming: false,
        };
        let json = serde_json::to_string(&req).unwrap();
        let parsed: ExecRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.user, Some("1000:1000".to_string()));
    }

    #[test]
    fn test_exec_output_empty() {
        let output = ExecOutput {
            stdout: vec![],
            stderr: vec![],
            exit_code: 0,
        };
        assert!(output.stdout.is_empty());
        assert!(output.stderr.is_empty());
        assert_eq!(output.exit_code, 0);
    }

    // --- Streaming types ---

    #[test]
    fn test_stream_type_display() {
        assert_eq!(StreamType::Stdout.to_string(), "stdout");
        assert_eq!(StreamType::Stderr.to_string(), "stderr");
    }

    #[test]
    fn test_exec_chunk_serde_roundtrip() {
        let chunk = ExecChunk {
            stream: StreamType::Stdout,
            data: b"hello world\n".to_vec(),
        };
        let json = serde_json::to_string(&chunk).unwrap();
        let parsed: ExecChunk = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.stream, StreamType::Stdout);
        assert_eq!(parsed.data, b"hello world\n");
    }

    #[test]
    fn test_exec_chunk_stderr() {
        let chunk = ExecChunk {
            stream: StreamType::Stderr,
            data: b"error: not found\n".to_vec(),
        };
        let json = serde_json::to_string(&chunk).unwrap();
        let parsed: ExecChunk = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.stream, StreamType::Stderr);
    }

    #[test]
    fn test_exec_exit_serde_roundtrip() {
        let exit = ExecExit { exit_code: 42 };
        let json = serde_json::to_string(&exit).unwrap();
        let parsed: ExecExit = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.exit_code, 42);
    }

    #[test]
    fn test_exec_metrics_default() {
        let m = ExecMetrics::default();
        assert_eq!(m.duration_ms, 0);
        assert!(m.peak_memory_bytes.is_none());
        assert_eq!(m.stdout_bytes, 0);
        assert_eq!(m.stderr_bytes, 0);
    }

    #[test]
    fn test_exec_metrics_serde_roundtrip() {
        let m = ExecMetrics {
            duration_ms: 1234,
            peak_memory_bytes: Some(65536),
            stdout_bytes: 100,
            stderr_bytes: 50,
        };
        let json = serde_json::to_string(&m).unwrap();
        let parsed: ExecMetrics = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.duration_ms, 1234);
        assert_eq!(parsed.peak_memory_bytes, Some(65536));
        assert_eq!(parsed.stdout_bytes, 100);
        assert_eq!(parsed.stderr_bytes, 50);
    }

    // --- File transfer types ---

    #[test]
    fn test_file_request_upload() {
        let req = FileRequest {
            op: FileOp::Upload,
            guest_path: "/tmp/test.txt".to_string(),
            data: Some("aGVsbG8=".to_string()),
        };
        let json = serde_json::to_string(&req).unwrap();
        let parsed: FileRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.op, FileOp::Upload);
        assert_eq!(parsed.guest_path, "/tmp/test.txt");
        assert_eq!(parsed.data.as_deref(), Some("aGVsbG8="));
    }

    #[test]
    fn test_file_request_download() {
        let req = FileRequest {
            op: FileOp::Download,
            guest_path: "/etc/hostname".to_string(),
            data: None,
        };
        let json = serde_json::to_string(&req).unwrap();
        let parsed: FileRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.op, FileOp::Download);
        assert!(parsed.data.is_none());
    }

    #[test]
    fn test_file_response_success() {
        let resp = FileResponse {
            success: true,
            data: Some("Y29udGVudA==".to_string()),
            size: 7,
            error: None,
        };
        let json = serde_json::to_string(&resp).unwrap();
        let parsed: FileResponse = serde_json::from_str(&json).unwrap();
        assert!(parsed.success);
        assert_eq!(parsed.size, 7);
        assert!(parsed.error.is_none());
    }

    #[test]
    fn test_file_response_error() {
        let resp = FileResponse {
            success: false,
            data: None,
            size: 0,
            error: Some("file not found".to_string()),
        };
        let json = serde_json::to_string(&resp).unwrap();
        let parsed: FileResponse = serde_json::from_str(&json).unwrap();
        assert!(!parsed.success);
        assert_eq!(parsed.error.as_deref(), Some("file not found"));
    }

    #[test]
    fn test_frame_exec_constants() {
        assert_eq!(FRAME_EXEC_CHUNK, 0x01);
        assert_eq!(FRAME_EXEC_EXIT, 0x02);
    }
}
