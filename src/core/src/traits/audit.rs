//! Audit sink abstraction.
//!
//! Decouples audit event recording from the file-based JSON-lines
//! implementation in `a3s-box-runtime`. Implementations can write to
//! any backend: files, databases, SIEM systems, cloud logging, etc.

use crate::audit::AuditEvent;
use crate::error::Result;

/// Abstraction over audit event recording.
///
/// The runtime calls `record` whenever a security-relevant action occurs
/// (box creation, exec commands, image pulls, etc.). Implementations
/// decide how and where to persist these events.
///
/// # Thread Safety
///
/// Implementations must be `Send + Sync`. Concurrent `record` calls
/// must be safe.
pub trait AuditSink: Send + Sync {
    /// Record an audit event.
    ///
    /// Implementations should be best-effort — a failure to record
    /// an audit event should not prevent the audited operation from
    /// proceeding. Callers may log the error but will not propagate it.
    fn record(&self, event: &AuditEvent) -> Result<()>;

    /// Flush any buffered events to the underlying storage.
    ///
    /// Called during graceful shutdown. Default implementation is a no-op.
    fn flush(&self) -> Result<()> {
        Ok(())
    }
}
