//! Best-effort audit-trail emission for box lifecycle events.
//!
//! The audit log (read with `a3s-box audit`) records security-relevant actions —
//! who created/started/stopped/removed a box, and what was exec'd or pulled. The
//! reader + CLI command and the `AuditLog` writer were fully built, but no
//! production code ever emitted an event, so the trail was always empty. These
//! helpers wire the writer into the lifecycle commands.
//!
//! Emission is **best-effort**: a failure to record the audit trail must never
//! fail the operation it describes. Auditing is on by default
//! (`AuditConfig::default().enabled == true`); `AuditLog::log` no-ops when it is
//! disabled.

use a3s_box_core::audit::{AuditAction, AuditEvent, AuditOutcome};
use a3s_box_runtime::AuditLog;

/// Emit one audit event to `log`, best-effort. Separated from [`record`] so the
/// emission can be unit-tested against a temporary log.
pub(crate) fn record_to(
    log: &AuditLog,
    action: AuditAction,
    outcome: AuditOutcome,
    box_id: &str,
    message: &str,
) {
    let event = AuditEvent::new(action, outcome)
        .with_box_id(box_id)
        .with_message(message);
    let _ = log.log(&event);
}

/// Emit one audit event to the default audit log (`~/.a3s/audit/audit.jsonl`),
/// best-effort. A log that can't be opened is silently skipped.
pub(crate) fn record(action: AuditAction, outcome: AuditOutcome, box_id: &str, message: &str) {
    if let Ok(log) = AuditLog::default_path() {
        record_to(&log, action, outcome, box_id, message);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use a3s_box_core::audit::AuditConfig;
    use a3s_box_runtime::{read_audit_log, AuditQuery};

    #[test]
    fn record_to_appends_a_readable_event() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("audit.jsonl");
        let log = AuditLog::new(&path, AuditConfig::default()).unwrap();

        record_to(
            &log,
            AuditAction::BoxStop,
            AuditOutcome::Success,
            "box-123",
            "stopped via a3s-box stop",
        );

        // The reader (a3s-box audit) must now surface the event — previously the
        // writer was never called so this list was always empty.
        let events = read_audit_log(&path, &AuditQuery::default()).unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].box_id.as_deref(), Some("box-123"));
        assert!(matches!(events[0].action, AuditAction::BoxStop));
    }

    #[test]
    fn record_to_is_silent_when_disabled() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("audit.jsonl");
        let disabled = AuditConfig {
            enabled: false,
            ..AuditConfig::default()
        };
        let log = AuditLog::new(&path, disabled).unwrap();

        record_to(&log, AuditAction::BoxStart, AuditOutcome::Success, "b", "x");

        // Disabled: nothing is written (no file, or an empty read).
        let events = read_audit_log(&path, &AuditQuery::default()).unwrap_or_default();
        assert!(events.is_empty());
    }
}
