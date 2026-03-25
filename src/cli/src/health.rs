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

use crate::state::{HealthCheck, StateFile};

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
    let timeout_ns = hc.timeout_secs.saturating_mul(1_000_000_000);

    loop {
        tokio::time::sleep(interval).await;

        // Box stopped — exec socket is gone
        if !exec_socket_path.exists() {
            break;
        }

        let healthy = run_probe(&exec_socket_path, &hc.cmd, timeout_ns).await;

        let Ok(mut state) = StateFile::load_default() else {
            continue;
        };
        let Some(record) = state.find_by_id_mut(&box_id) else {
            break; // Box removed from state
        };

        if healthy {
            record.health_status = "healthy".to_string();
            record.health_retries = 0;
        } else {
            record.health_retries += 1;
            if record.health_retries >= hc.retries {
                record.health_status = "unhealthy".to_string();
            }
        }
        record.health_last_check = Some(chrono::Utc::now());

        let _ = state.save();
    }
}

#[cfg(not(windows))]
async fn run_probe(exec_socket_path: &std::path::Path, cmd: &[String], timeout_ns: u64) -> bool {
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
        stdin: None,
        user: None,
        streaming: false,
    };

    match client.exec_command(&request).await {
        Ok(output) => output.exit_code == 0,
        Err(_) => false,
    }
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
        let timeout_ns = 5u64.saturating_mul(1_000_000_000);
        assert_eq!(timeout_ns, 5_000_000_000);

        let big_timeout_ns = u64::MAX.saturating_mul(1_000_000_000);
        assert_eq!(big_timeout_ns, u64::MAX); // saturates instead of overflowing
    }
}
