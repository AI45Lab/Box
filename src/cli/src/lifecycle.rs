//! Shared lifecycle validation helpers for commands backed by a host process.

use crate::process;
use crate::state::BoxRecord;

/// Require a record to point at a currently live host process.
pub fn require_live_pid(record: &BoxRecord, action: &str) -> Result<u32, String> {
    match record.pid {
        Some(pid) if process::is_process_alive_with_identity(pid, record.pid_start_time) => Ok(pid),
        Some(pid) => Err(format!(
            "Cannot {action} box {} because its recorded PID {pid} is not running. The box state may be stale; run `a3s-box ps` to reconcile state, then `a3s-box restart {}` if it should still be running.",
            record.name, record.name
        )),
        None => Err(format!(
            "Cannot {action} box {} because it has no recorded PID. The box state may be stale; run `a3s-box ps` to reconcile state, then `a3s-box restart {}` if it should still be running.",
            record.name, record.name
        )),
    }
}

/// Resume a paused process before sending a terminating lifecycle signal.
pub fn resume_paused_for_termination(
    record: &BoxRecord,
    pid: u32,
    action: &str,
) -> Result<(), String> {
    if record.status != "paused" {
        return Ok(());
    }

    #[cfg(unix)]
    {
        process::send_signal(pid, libc::SIGCONT).map_err(|err| {
            format!(
                "Failed to resume paused box {} before {action}: {err}",
                record.name
            )
        })
    }
    #[cfg(windows)]
    {
        let _ = pid;
        Err(crate::platform::unsupported_command(
            action,
            "resuming a paused host process before termination",
        )
        .to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_helpers::fixtures::make_record;

    #[test]
    fn test_require_live_pid_accepts_current_process() {
        let record = make_record("id", "box", "running", Some(std::process::id()));

        assert_eq!(
            require_live_pid(&record, "pause").unwrap(),
            std::process::id()
        );
    }

    #[test]
    fn test_require_live_pid_rejects_missing_pid_with_guidance() {
        let record = make_record("id", "box", "running", None);

        let error = require_live_pid(&record, "pause").unwrap_err();

        assert!(error.contains("no recorded PID"));
        assert!(error.contains("a3s-box ps"));
        assert!(error.contains("a3s-box restart box"));
    }

    #[test]
    fn test_resume_paused_for_termination_noops_for_running() {
        let record = make_record("id", "box", "running", Some(std::process::id()));

        assert!(resume_paused_for_termination(&record, std::process::id(), "stop").is_ok());
    }

    // A live PID whose recorded start-time identity does NOT match (a reused PID
    // after a crash/reboot) must be rejected so stop/kill/pause never signals an
    // unrelated host process. Without the identity check this returns Ok(pid).
    #[cfg(target_os = "linux")]
    #[test]
    fn test_require_live_pid_rejects_reused_pid_via_identity_mismatch() {
        let mut record = make_record("id", "box", "running", Some(std::process::id()));
        record.pid_start_time = Some(u64::MAX);

        let error = require_live_pid(&record, "stop").unwrap_err();
        assert!(error.contains("is not running"));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn test_require_live_pid_accepts_matching_identity() {
        let mut record = make_record("id", "box", "running", Some(std::process::id()));
        record.pid_start_time = crate::process::pid_start_time(std::process::id());

        assert_eq!(
            require_live_pid(&record, "stop").unwrap(),
            std::process::id()
        );
    }
}
