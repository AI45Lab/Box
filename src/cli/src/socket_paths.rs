//! Helpers for resolving runtime socket paths from persisted box records.

use std::path::PathBuf;

use crate::state::BoxRecord;

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

pub fn pty(record: &BoxRecord) -> PathBuf {
    sibling(record, "pty.sock")
}

pub fn attest(record: &BoxRecord) -> PathBuf {
    sibling(record, "attest.sock")
}
