//! Helpers for resolving runtime socket paths from persisted box records.

use std::path::PathBuf;

use crate::state::BoxRecord;

/// Runtime socket kind used by host-side control commands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuntimeSocket {
    Exec,
    Pty,
    Attest,
}

impl RuntimeSocket {
    fn file_name(self) -> &'static str {
        match self {
            Self::Exec => "exec.sock",
            Self::Pty => "pty.sock",
            Self::Attest => "attest.sock",
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Exec => "exec",
            Self::Pty => "PTY",
            Self::Attest => "attestation",
        }
    }

    fn action(self) -> &'static str {
        match self {
            Self::Exec => "exec in",
            Self::Pty => "open a PTY in",
            Self::Attest => "request attestation from",
        }
    }
}

/// Resolve a sibling socket next to the recorded exec socket.
///
/// Newer runtimes may place sockets outside `box_dir` to avoid Unix socket path
/// length limits. Older records keep sockets under `box_dir/sockets`.
pub fn sibling(record: &BoxRecord, socket_name: &str) -> PathBuf {
    if let Some(parent) = record.exec_socket_path.parent() {
        return parent.join(socket_name);
    }
    record.box_dir.join("sockets").join(socket_name)
}

pub fn exec(record: &BoxRecord) -> PathBuf {
    if !record.exec_socket_path.as_os_str().is_empty() {
        return record.exec_socket_path.clone();
    }
    record.box_dir.join("sockets").join("exec.sock")
}

pub fn pty(record: &BoxRecord) -> PathBuf {
    sibling(record, "pty.sock")
}

pub fn attest(record: &BoxRecord) -> PathBuf {
    sibling(record, "attest.sock")
}

pub fn runtime_socket(record: &BoxRecord, socket: RuntimeSocket) -> PathBuf {
    match socket {
        RuntimeSocket::Exec => exec(record),
        RuntimeSocket::Pty | RuntimeSocket::Attest => sibling(record, socket.file_name()),
    }
}

pub fn require_running(record: &BoxRecord, action: &str) -> Result<(), String> {
    if record.status == "running" {
        return Ok(());
    }

    Err(format!(
        "Cannot {action} box {} because it is {}. Use `a3s-box start {}` to start it or `a3s-box ps -a` to inspect state.",
        record.name, record.status, record.name
    ))
}

pub fn require_runtime_socket(
    record: &BoxRecord,
    socket: RuntimeSocket,
) -> Result<PathBuf, String> {
    require_running(record, socket.action())?;
    let path = runtime_socket(record, socket);
    if path.exists() {
        return Ok(path);
    }

    Err(format!(
        "{} socket is missing for running box {} at {}. The box state may be stale or the guest control channel is not ready; run `a3s-box ps` to reconcile state, then `a3s-box restart {}` if the socket is still missing.",
        socket.label(),
        record.name,
        path.display(),
        record.name
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_helpers::fixtures::make_record;

    #[test]
    fn test_exec_uses_recorded_exec_socket_path() {
        let mut record = make_record("id", "box", "running", Some(1));
        record.exec_socket_path = PathBuf::from("/tmp/a3s-custom/exec.sock");

        assert_eq!(exec(&record), PathBuf::from("/tmp/a3s-custom/exec.sock"));
    }

    #[test]
    fn test_pty_uses_exec_socket_sibling() {
        let mut record = make_record("id", "box", "running", Some(1));
        record.exec_socket_path = PathBuf::from("/tmp/a3s-custom/exec.sock");

        assert_eq!(pty(&record), PathBuf::from("/tmp/a3s-custom/pty.sock"));
    }

    #[test]
    fn test_require_running_returns_actionable_error() {
        let record = make_record("id", "box", "dead", None);

        let error = require_running(&record, "exec").unwrap_err();

        assert!(error.contains("Cannot exec box box because it is dead"));
        assert!(error.contains("a3s-box start box"));
    }

    #[test]
    fn test_require_runtime_socket_returns_actionable_missing_socket_error() {
        let record = make_record("id", "box", "running", Some(1));

        let error = require_runtime_socket(&record, RuntimeSocket::Exec).unwrap_err();

        assert!(error.contains("exec socket is missing"));
        assert!(error.contains("a3s-box ps"));
        assert!(error.contains("a3s-box restart box"));
    }

    #[test]
    fn test_require_runtime_socket_accepts_existing_socket() {
        let tmp = tempfile::tempdir().unwrap();
        let socket_path = tmp.path().join("exec.sock");
        std::fs::write(&socket_path, b"not-a-real-socket").unwrap();
        let mut record = make_record("id", "box", "running", Some(1));
        record.exec_socket_path = socket_path.clone();

        assert_eq!(
            require_runtime_socket(&record, RuntimeSocket::Exec).unwrap(),
            socket_path
        );
    }
}
