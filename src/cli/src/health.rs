//! Health check executor for running containers.
//!
//! Spawns a background task that periodically runs the user-defined health
//! check command via the exec socket and updates the box state accordingly.
//!
//! Follows Docker health check semantics:
//! - Wait `start_period_secs` before the first check
//! - Run every `interval_secs`; timeout each run at `timeout_secs`
//! - Exit code 0 → healthy; non-zero → failure
//! - After `retries` consecutive failures → status becomes "unhealthy"
//! - Socket disappearing → box has stopped; checker exits

use std::path::PathBuf;

#[cfg(any(not(windows), test))]
use crate::state::BoxRecord;
use crate::state::HealthCheck;
#[cfg(not(windows))]
use crate::state::StateFile;

/// Spawn a background health checker task for a running box.
///
/// Returns a `JoinHandle` that the caller can abort when the box stops.
/// In detached/daemon scenarios the handle may be dropped; the task will
/// self-terminate once the exec socket disappears.
pub fn spawn_health_checker(
    box_id: String,
    exec_socket_path: PathBuf,
    health_check: HealthCheck,
) -> tokio::task::JoinHandle<()> {
    #[cfg(not(windows))]
    {
        tokio::spawn(async move {
            run_health_loop(box_id, exec_socket_path, health_check).await;
        })
    }
    #[cfg(windows)]
    {
        // Health checks require exec socket (Unix domain sockets); no-op on Windows.
        let _ = (box_id, exec_socket_path, health_check);
        tokio::spawn(async {})
    }
}

#[cfg(not(windows))]
async fn run_health_loop(box_id: String, exec_socket_path: PathBuf, hc: HealthCheck) {
    use std::time::Duration;

    // Honour start_period before the first probe
    if hc.start_period_secs > 0 {
        tokio::time::sleep(Duration::from_secs(hc.start_period_secs)).await;
    }

    let interval = Duration::from_secs(hc.interval_secs.max(1));
    let timeout_ns = probe_timeout_ns(&hc);

    loop {
        tokio::time::sleep(interval).await;

        // Box stopped — exec socket is gone
        if !exec_socket_path.exists() {
            break;
        }

        let healthy = run_probe(&exec_socket_path, &hc.cmd, timeout_ns).await;

        // Reload fresh under the state lock and apply ONLY this box's health
        // fields, so concurrent monitor/CLI writers are not clobbered.
        let keep_going = StateFile::modify(|state| {
            let Some(record) = state.find_by_id_mut(&box_id) else {
                return Ok::<bool, std::io::Error>(false); // box removed
            };
            if record.status != "running" {
                return Ok(false); // box stopped
            }
            apply_probe_result(record, healthy, chrono::Utc::now());
            Ok(true)
        });
        match keep_going {
            Ok(true) => {}
            Ok(false) => break,
            Err(_) => continue,
        }
    }
}

#[cfg(not(windows))]
pub(crate) async fn run_probe(
    exec_socket_path: &std::path::Path,
    cmd: &[String],
    timeout_ns: u64,
) -> bool {
    use a3s_box_core::exec::ExecRequest;
    use a3s_box_runtime::ExecClient;

    let client = match ExecClient::connect(exec_socket_path).await {
        Ok(c) => c,
        Err(_) => return false,
    };

    let request = ExecRequest {
        cmd: cmd.to_vec(),
        timeout_ns,
        env: vec![],
        working_dir: None,
        rootfs: None,
        stdin: None,
        stdin_streaming: false,
        user: None,
        streaming: false,
    };

    match client.exec_command(&request).await {
        Ok(output) => output.exit_code == 0,
        Err(_) => false,
    }
}

#[cfg(any(not(windows), test))]
pub(crate) fn probe_timeout_ns(hc: &HealthCheck) -> u64 {
    hc.timeout_secs.saturating_mul(1_000_000_000)
}

#[cfg(any(not(windows), test))]
pub(crate) fn should_probe(record: &BoxRecord, now: chrono::DateTime<chrono::Utc>) -> bool {
    let Some(hc) = record.health_check.as_ref() else {
        return false;
    };
    if record.status != "running" {
        return false;
    }

    if let Some(started_at) = record.started_at {
        let start_period = bounded_chrono_seconds(hc.start_period_secs);
        if now < started_at + start_period {
            return false;
        }
    }

    let Some(last_check) = record.health_last_check else {
        return true;
    };

    now >= last_check + bounded_chrono_seconds(hc.interval_secs.max(1))
}

#[cfg(any(not(windows), test))]
pub(crate) fn apply_probe_result(
    record: &mut BoxRecord,
    healthy: bool,
    checked_at: chrono::DateTime<chrono::Utc>,
) {
    if record.status != "running" {
        return;
    }

    if healthy {
        record.health_status = "healthy".to_string();
        record.health_retries = 0;
    } else {
        record.health_retries = record.health_retries.saturating_add(1);
        if let Some(hc) = record.health_check.as_ref() {
            if record.health_retries >= hc.retries {
                record.health_status = "unhealthy".to_string();
            }
        }
    }
    record.health_last_check = Some(checked_at);
}

#[cfg(any(not(windows), test))]
fn bounded_chrono_seconds(seconds: u64) -> chrono::Duration {
    chrono::Duration::seconds(seconds.min(i64::MAX as u64) as i64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_health_check_interval_floor() {
        // Ensure interval_secs of 0 doesn't cause busy-loop (max(1) guard)
        let hc = HealthCheck {
            cmd: vec!["true".to_string()],
            interval_secs: 0,
            timeout_secs: 5,
            retries: 3,
            start_period_secs: 0,
        };
        let interval = std::time::Duration::from_secs(hc.interval_secs.max(1));
        assert_eq!(interval, std::time::Duration::from_secs(1));
    }

    #[test]
    fn test_timeout_ns_overflow_safe() {
        // Large timeout_secs must not overflow u64
        let hc = HealthCheck {
            cmd: vec!["true".to_string()],
            interval_secs: 30,
            timeout_secs: 5,
            retries: 3,
            start_period_secs: 0,
        };
        assert_eq!(probe_timeout_ns(&hc), 5_000_000_000);

        let big_hc = HealthCheck {
            timeout_secs: u64::MAX,
            ..hc
        };
        assert_eq!(probe_timeout_ns(&big_hc), u64::MAX); // saturates instead of overflowing
    }

    #[test]
    fn test_should_probe_respects_start_period() {
        let now = chrono::Utc::now();
        let mut record =
            crate::test_helpers::fixtures::make_record("health-id", "health", "running", Some(1));
        record.started_at = Some(now);
        record.health_check = Some(HealthCheck {
            cmd: vec!["true".to_string()],
            interval_secs: 30,
            timeout_secs: 5,
            retries: 3,
            start_period_secs: 10,
        });

        assert!(!should_probe(&record, now + chrono::Duration::seconds(9)));
        assert!(should_probe(&record, now + chrono::Duration::seconds(10)));
    }

    #[test]
    fn test_should_probe_respects_interval() {
        let now = chrono::Utc::now();
        let mut record =
            crate::test_helpers::fixtures::make_record("health-id", "health", "running", Some(1));
        record.started_at = Some(now - chrono::Duration::seconds(60));
        record.health_last_check = Some(now);
        record.health_check = Some(HealthCheck {
            cmd: vec!["true".to_string()],
            interval_secs: 30,
            timeout_secs: 5,
            retries: 3,
            start_period_secs: 0,
        });

        assert!(!should_probe(&record, now + chrono::Duration::seconds(29)));
        assert!(should_probe(&record, now + chrono::Duration::seconds(30)));
    }

    #[test]
    fn test_apply_probe_result_tracks_retries_and_recovery() {
        let now = chrono::Utc::now();
        let mut record =
            crate::test_helpers::fixtures::make_record("health-id", "health", "running", Some(1));
        record.health_status = "starting".to_string();
        record.health_check = Some(HealthCheck {
            cmd: vec!["false".to_string()],
            interval_secs: 30,
            timeout_secs: 5,
            retries: 2,
            start_period_secs: 0,
        });

        apply_probe_result(&mut record, false, now);
        assert_eq!(record.health_status, "starting");
        assert_eq!(record.health_retries, 1);

        apply_probe_result(&mut record, false, now);
        assert_eq!(record.health_status, "unhealthy");
        assert_eq!(record.health_retries, 2);

        apply_probe_result(&mut record, true, now);
        assert_eq!(record.health_status, "healthy");
        assert_eq!(record.health_retries, 0);
    }

    #[test]
    fn test_apply_probe_result_ignores_stopped_records() {
        let now = chrono::Utc::now();
        let mut record =
            crate::test_helpers::fixtures::make_record("health-id", "health", "stopped", None);
        record.health_check = Some(HealthCheck {
            cmd: vec!["true".to_string()],
            interval_secs: 30,
            timeout_secs: 5,
            retries: 1,
            start_period_secs: 0,
        });

        apply_probe_result(&mut record, true, now);
        assert_eq!(record.health_status, "none");
        assert!(record.health_last_check.is_none());
    }
}
