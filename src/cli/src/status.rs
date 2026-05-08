//! Shared box status formatting and diagnostics for CLI output.

use serde::Serialize;

use crate::state::BoxRecord;

/// Structured status details added to `inspect` output.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct StatusDetails {
    /// Persisted lifecycle state.
    pub state: String,
    /// Human-readable summary used by tabular CLI output.
    pub summary: String,
    /// Whether the box should be treated as active by default listings.
    pub active: bool,
    /// Recorded host process PID, when active.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
    /// Last recorded exit code, when known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    /// Current health state, when a health check is active.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub health: Option<String>,
    /// Number of automatic restart attempts already recorded.
    pub restart_count: u32,
    /// Actionable lifecycle guidance for non-running states.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hint: Option<String>,
}

pub fn is_active(record: &BoxRecord) -> bool {
    is_active_status(&record.status)
}

pub fn is_active_status(status: &str) -> bool {
    matches!(status, "running" | "paused")
}

pub fn require_active(record: &BoxRecord, action: &str) -> Result<(), String> {
    if is_active(record) {
        return Ok(());
    }

    let hint =
        status_hint(record).unwrap_or_else(|| "Use `a3s-box ps -a` to inspect state.".to_string());
    Err(format!(
        "Cannot {action} box {} because it is {}. {hint}",
        record.name, record.status
    ))
}

pub fn is_default_ps_visible(record: &BoxRecord) -> bool {
    is_active(record)
}

/// Format box status with health, exit-code, and restart annotations.
pub fn format_status(record: &BoxRecord) -> String {
    let mut annotations = Vec::new();

    if is_active(record) && record.health_check.is_some() && record.health_status != "none" {
        annotations.push(record.health_status.clone());
    }

    if matches!(record.status.as_str(), "stopped" | "dead") {
        if let Some(exit_code) = record.exit_code {
            annotations.push(format!("Exit {exit_code}"));
        }
    }

    if record.restart_count > 0 {
        annotations.push(format!("Restarts: {}", record.restart_count));
    }

    if annotations.is_empty() {
        record.status.clone()
    } else {
        format!("{} ({})", record.status, annotations.join(", "))
    }
}

pub fn status_details(record: &BoxRecord) -> StatusDetails {
    let health =
        if is_active(record) && record.health_check.is_some() && record.health_status != "none" {
            Some(record.health_status.clone())
        } else {
            None
        };

    StatusDetails {
        state: record.status.clone(),
        summary: format_status(record),
        active: is_active(record),
        pid: record.pid,
        exit_code: record.exit_code,
        health,
        restart_count: record.restart_count,
        hint: status_hint(record),
    }
}

fn status_hint(record: &BoxRecord) -> Option<String> {
    match record.status.as_str() {
        "created" => Some(format!(
            "Use `a3s-box start {}` to start it or `a3s-box rm {}` to remove it.",
            record.name, record.name
        )),
        "paused" => Some(format!(
            "Use `a3s-box unpause {}` to resume it or `a3s-box stop {}` to stop it.",
            record.name, record.name
        )),
        "stopped" => Some(format!(
            "Use `a3s-box start {}` to start it again or `a3s-box rm {}` to remove it.",
            record.name, record.name
        )),
        "dead" => Some(format!(
            "Run `a3s-box logs {}` to inspect failure output, then `a3s-box restart {}` or `a3s-box rm {}`.",
            record.name, record.name, record.name
        )),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_helpers::fixtures::make_record;

    fn health_check() -> crate::state::HealthCheck {
        crate::state::HealthCheck {
            cmd: vec!["true".to_string()],
            interval_secs: 30,
            timeout_secs: 5,
            retries: 3,
            start_period_secs: 0,
        }
    }

    #[test]
    fn test_default_ps_visible_includes_running_and_paused_only() {
        assert!(is_default_ps_visible(&make_record(
            "id-1",
            "running",
            "running",
            Some(1)
        )));
        assert!(is_default_ps_visible(&make_record(
            "id-2",
            "paused",
            "paused",
            Some(1)
        )));
        assert!(!is_default_ps_visible(&make_record(
            "id-3", "created", "created", None
        )));
        assert!(!is_default_ps_visible(&make_record(
            "id-4", "stopped", "stopped", None
        )));
        assert!(!is_default_ps_visible(&make_record(
            "id-5", "dead", "dead", None
        )));
    }

    #[test]
    fn test_is_active_status() {
        assert!(is_active_status("running"));
        assert!(is_active_status("paused"));
        assert!(!is_active_status("created"));
        assert!(!is_active_status("stopped"));
        assert!(!is_active_status("dead"));
    }

    #[test]
    fn test_format_status_with_health() {
        let mut record = make_record("id", "box", "running", Some(1));
        record.health_check = Some(health_check());
        record.health_status = "healthy".to_string();

        assert_eq!(format_status(&record), "running (healthy)");
    }

    #[test]
    fn test_format_status_ignores_stale_health_for_stopped() {
        let mut record = make_record("id", "box", "stopped", None);
        record.health_check = Some(health_check());
        record.health_status = "healthy".to_string();

        assert_eq!(format_status(&record), "stopped");
    }

    #[test]
    fn test_format_status_with_exit_code() {
        let mut record = make_record("id", "box", "dead", None);
        record.exit_code = Some(137);

        assert_eq!(format_status(&record), "dead (Exit 137)");
    }

    #[test]
    fn test_format_status_with_restart_count() {
        let mut record = make_record("id", "box", "running", Some(1));
        record.restart_count = 3;

        assert_eq!(format_status(&record), "running (Restarts: 3)");
    }

    #[test]
    fn test_status_details_adds_paused_hint() {
        let record = make_record("id", "box", "paused", Some(1));
        let details = status_details(&record);

        assert_eq!(details.summary, "paused");
        assert!(details.active);
        assert!(details.hint.unwrap().contains("a3s-box unpause box"));
    }

    #[test]
    fn test_require_active_allows_paused() {
        let record = make_record("id", "box", "paused", Some(1));

        assert!(require_active(&record, "show stats for").is_ok());
    }

    #[test]
    fn test_require_active_rejects_stopped_with_hint() {
        let record = make_record("id", "box", "stopped", None);

        let error = require_active(&record, "show stats for").unwrap_err();

        assert!(error.contains("Cannot show stats for box box because it is stopped"));
        assert!(error.contains("a3s-box start box"));
    }
}
