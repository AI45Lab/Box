//! `a3s-box monitor` command — Background daemon that restarts dead boxes.
//!
//! Polls `boxes.json` periodically, detects dead VMs via PID liveness checks,
//! and restarts boxes according to their restart policy. Also monitors health
//! check status and restarts unhealthy boxes. Uses exponential backoff to
//! prevent crash loops.
//!
//! Usage: `a3s-box monitor` (long-running, typically run as a background service)

use std::collections::HashMap;
use std::time::{Duration, Instant};

use clap::Args;

use crate::boot;
#[cfg(not(windows))]
use crate::health;
use crate::state::{policy, BoxRecord, StateFile};
use crate::status;

/// Minimum backoff delay before retrying a restart.
const MIN_BACKOFF: Duration = Duration::from_secs(1);

/// Maximum backoff delay (cap).
const MAX_BACKOFF: Duration = Duration::from_secs(60);

/// How long a box must stay alive before its backoff resets.
const STABLE_THRESHOLD: Duration = Duration::from_secs(30);

#[derive(Args)]
pub struct MonitorArgs {
    /// Poll interval in seconds (default: 5)
    #[arg(long, default_value = "5")]
    pub interval: u64,

    /// Install + enable the monitor as a supervised per-user service
    /// (systemd `--user` on Linux, launchd LaunchAgent on macOS) and exit.
    /// Without this, the monitor runs in the foreground.
    #[arg(long, conflicts_with = "uninstall")]
    pub install: bool,

    /// Disable and remove the installed monitor service, then exit.
    #[arg(long)]
    pub uninstall: bool,

    /// Serve Prometheus metrics + `/healthz` on this address (e.g.
    /// `127.0.0.1:9100`). Off when unset. Bind loopback — there is no auth.
    #[arg(long)]
    pub metrics_addr: Option<String>,
}

/// Per-box backoff state for restart attempts.
#[derive(Debug)]
struct BackoffEntry {
    /// Current backoff delay.
    delay: Duration,
    /// When the last restart attempt was made.
    last_attempt: Instant,
    /// When the box was last seen running (to detect stability).
    last_seen_running: Option<Instant>,
}

impl BackoffEntry {
    fn new() -> Self {
        Self {
            delay: MIN_BACKOFF,
            last_attempt: Instant::now() - MAX_BACKOFF, // allow immediate first attempt
            last_seen_running: None,
        }
    }

    /// Check if enough time has passed since the last attempt.
    fn ready(&self) -> bool {
        self.last_attempt.elapsed() >= self.delay
    }

    /// Record a restart attempt and increase backoff.
    fn record_attempt(&mut self) {
        self.last_attempt = Instant::now();
        self.delay = (self.delay * 2).min(MAX_BACKOFF);
    }

    /// Mark the box as currently running. If it stays running long enough,
    /// the backoff resets.
    fn mark_running(&mut self) {
        let now = Instant::now();
        match self.last_seen_running {
            Some(since) if now.duration_since(since) >= STABLE_THRESHOLD => {
                // Box has been stable — reset backoff
                self.delay = MIN_BACKOFF;
            }
            None => {
                self.last_seen_running = Some(now);
            }
            _ => {} // still within threshold, keep tracking
        }
    }

    /// Mark the box as no longer running.
    fn mark_dead(&mut self) {
        self.last_seen_running = None;
    }
}

/// Tracks backoff state for all boxes.
pub struct BackoffTracker {
    entries: HashMap<String, BackoffEntry>,
}

impl BackoffTracker {
    pub fn new() -> Self {
        Self {
            entries: HashMap::new(),
        }
    }

    /// Check if a box is ready for a restart attempt.
    pub fn ready(&self, box_id: &str) -> bool {
        self.entries.get(box_id).is_none_or(|e| e.ready())
    }

    /// Record a restart attempt for a box.
    pub fn record_attempt(&mut self, box_id: &str) {
        self.entries
            .entry(box_id.to_string())
            .or_insert_with(BackoffEntry::new)
            .record_attempt();
    }

    /// Mark a box as currently running (for stability tracking).
    pub fn mark_running(&mut self, box_id: &str) {
        self.entries
            .entry(box_id.to_string())
            .or_insert_with(BackoffEntry::new)
            .mark_running();
    }

    /// Mark a box as dead.
    pub fn mark_dead(&mut self, box_id: &str) {
        if let Some(entry) = self.entries.get_mut(box_id) {
            entry.mark_dead();
        }
    }

    /// Get the current backoff delay for a box.
    pub fn current_delay(&self, box_id: &str) -> Duration {
        self.entries.get(box_id).map_or(MIN_BACKOFF, |e| e.delay)
    }
}

pub async fn execute(args: MonitorArgs) -> Result<(), Box<dyn std::error::Error>> {
    if args.install {
        return super::monitor_service::install(args.interval);
    }
    if args.uninstall {
        return super::monitor_service::uninstall();
    }

    let interval = Duration::from_secs(args.interval);
    let mut tracker = BackoffTracker::new();

    println!(
        "a3s-box monitor started (poll interval: {}s)",
        args.interval
    );

    // Shared last-poll clock so the metrics endpoint's /healthz reflects whether
    // the poll loop is actually alive (not just that the HTTP task is up).
    let last_poll = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(
        super::monitor_metrics::now_secs(),
    ));

    // Optional metrics/health endpoint, served alongside the poll loop.
    if let Some(addr) = args.metrics_addr.clone() {
        let last_poll = std::sync::Arc::clone(&last_poll);
        // A poll is "stale" after 3 intervals, with a 30s floor for short intervals.
        let stale_after = args.interval.saturating_mul(3).max(30);
        tokio::spawn(async move {
            if let Err(e) = super::monitor_metrics::serve(addr, last_poll, stale_after).await {
                eprintln!("monitor metrics: failed to serve: {e}");
            }
        });
    }

    loop {
        if let Err(e) = poll_once(&mut tracker).await {
            eprintln!("monitor: poll error: {e}");
        }
        // Mark the loop alive (a hung poll_once stops updating this, so /healthz
        // goes 503; a transient poll error keeps the loop alive and healthy).
        last_poll.store(
            super::monitor_metrics::now_secs(),
            std::sync::atomic::Ordering::Relaxed,
        );
        tokio::time::sleep(interval).await;
    }
}

/// Single poll iteration: load state, find dead boxes, restart eligible ones.
/// Also checks for unhealthy boxes that have a restart policy.
async fn poll_once(tracker: &mut BackoffTracker) -> Result<(), Box<dyn std::error::Error>> {
    let state = StateFile::load_default()?;

    // Track active boxes for stability detection.
    for record in state.records() {
        if status::is_active(record) {
            tracker.mark_running(&record.id);
        }
    }

    run_due_health_checks(&state).await?;

    // Find boxes that need restarting: dead boxes + unhealthy running boxes
    let mut candidates = state.pending_restarts();

    // Also restart running boxes that are unhealthy and have a restart policy
    let unhealthy: Vec<String> = state
        .records()
        .iter()
        .filter(|r| is_unhealthy_restart_candidate(r))
        .map(|r| r.id.clone())
        .collect();
    candidates.extend(unhealthy);

    for box_id in candidates {
        let record = match state.find_by_id(&box_id) {
            Some(r) => r.clone(),
            None => continue,
        };

        // Check backoff
        if !tracker.ready(&box_id) {
            let delay = tracker.current_delay(&box_id);
            eprintln!("{}", backoff_log_line(&record, delay));
            continue;
        }

        let is_unhealthy = is_unhealthy_restart_candidate(&record);

        // If unhealthy, kill the process first before restarting
        if is_unhealthy {
            println!("{}", restart_log_line(&record, RestartReason::Unhealthy));
            if let Some(pid) = record.pid {
                crate::process::graceful_stop(pid, libc::SIGTERM, 10).await;
            }
            tracker.mark_dead(&box_id);
            // Mark as dead so boot_from_record works; re-load fresh under the
            // lock and touch only this box's fields.
            StateFile::modify(|s| {
                if let Some(rec) = s.find_by_id_mut(&box_id) {
                    rec.status = "dead".to_string();
                    rec.pid = None;
                    rec.health_status = "none".to_string();
                    rec.health_retries = 0;
                }
                Ok::<(), std::io::Error>(())
            })?;
        } else {
            tracker.mark_dead(&box_id);
            println!("{}", restart_log_line(&record, RestartReason::Dead));
        }

        // Attempt restart
        match boot::boot_from_record(&record).await {
            Ok(result) => {
                // Capture the shim pid before `result` is moved into the closure,
                // so we can tear the VM down if the record vanished mid-boot.
                let booted_pid = result.pid;
                // Re-load fresh under the lock and apply only this box's restart
                // fields. Returns the new restart count, or None if the record is
                // gone — a concurrent `rm` removed the box while we were booting.
                let new_count = StateFile::modify(|s| {
                    let count = s.find_by_id_mut(&box_id).map(|rec| {
                        boot::apply_boot_result(rec, result, boot::RestartCountUpdate::Increment);
                        rec.restart_count
                    });
                    Ok::<Option<u32>, std::io::Error>(count)
                })?;
                tracker.record_attempt(&box_id);
                match new_count {
                    Some(new_count) => println!(
                        "monitor: box {name} ({short_id}) restarted (count: {new_count})",
                        name = record.name,
                        short_id = record.short_id,
                    ),
                    None => {
                        // The box was removed during the restart boot: the VM we
                        // just started has no record tracking it. Tear it down so
                        // it doesn't leak as an orphan shim + overlay mount.
                        eprintln!(
                            "monitor: box {name} ({short_id}) was removed during restart; tearing down the orphaned VM",
                            name = record.name,
                            short_id = record.short_id,
                        );
                        if let Some(pid) = booted_pid {
                            crate::process::graceful_stop(pid, libc::SIGTERM, 5).await;
                        }
                        crate::cleanup::cleanup_removed_box(&record);
                    }
                }
            }
            Err(e) => {
                tracker.record_attempt(&box_id);
                let delay = tracker.current_delay(&box_id);
                eprintln!(
                    "monitor: failed to restart box {name} ({short_id}): {e} (next retry in {:.0}s)",
                    delay.as_secs_f64(),
                    name = record.name,
                    short_id = record.short_id,
                );
            }
        }
    }

    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RestartReason {
    Dead,
    Unhealthy,
}

fn is_unhealthy_restart_candidate(record: &BoxRecord) -> bool {
    record.status == "running"
        && record.health_status == "unhealthy"
        && record.health_check.is_some()
        && policy::should_restart(record)
}

fn restart_log_line(record: &BoxRecord, reason: RestartReason) -> String {
    match reason {
        RestartReason::Dead => format!(
            "monitor: restarting dead box {} ({}, policy: {}, exit: {})...",
            record.name,
            record.short_id,
            record.restart_policy,
            format_exit_code(record.exit_code)
        ),
        RestartReason::Unhealthy => format!(
            "monitor: box {} ({}, policy: {}) is unhealthy, restarting...",
            record.name, record.short_id, record.restart_policy
        ),
    }
}

fn backoff_log_line(record: &BoxRecord, delay: Duration) -> String {
    format!(
        "monitor: box {} ({}) backing off ({:.0}s remaining)",
        record.name,
        record.short_id,
        delay.as_secs_f64()
    )
}

fn format_exit_code(exit_code: Option<i32>) -> String {
    exit_code
        .map(|code| code.to_string())
        .unwrap_or_else(|| "unknown".to_string())
}

#[cfg(not(windows))]
async fn run_due_health_checks(state: &StateFile) -> Result<(), Box<dyn std::error::Error>> {
    let now = chrono::Utc::now();
    let probes: Vec<_> = state
        .records()
        .iter()
        .filter(|record| health::should_probe(record, now))
        .filter_map(|record| {
            record.health_check.as_ref().map(|hc| {
                (
                    record.id.clone(),
                    record.exec_socket_path.clone(),
                    hc.clone(),
                )
            })
        })
        .collect();

    if probes.is_empty() {
        return Ok(());
    }

    for (box_id, exec_socket_path, health_check) in probes {
        let healthy = health::run_probe(
            &exec_socket_path,
            &health_check.cmd,
            health::probe_timeout_ns(&health_check),
        )
        .await;
        // Re-load fresh under the lock and apply only this box's health fields,
        // so concurrent CLI/health-checker writes are preserved.
        StateFile::modify(|s| {
            if let Some(record) = s.find_by_id_mut(&box_id) {
                health::apply_probe_result(record, healthy, chrono::Utc::now());
            }
            Ok::<(), std::io::Error>(())
        })?;
    }

    Ok(())
}

#[cfg(windows)]
async fn run_due_health_checks(_state: &StateFile) -> Result<(), Box<dyn std::error::Error>> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_helpers::fixtures::make_record;

    // --- BackoffTracker tests ---

    #[test]
    fn test_backoff_tracker_new_box_is_ready() {
        let tracker = BackoffTracker::new();
        assert!(tracker.ready("box-1"));
    }

    #[test]
    fn test_backoff_tracker_not_ready_after_attempt() {
        let mut tracker = BackoffTracker::new();
        tracker.record_attempt("box-1");
        // Immediately after attempt, should not be ready (backoff is at least 1s)
        assert!(!tracker.ready("box-1"));
    }

    #[test]
    fn test_backoff_tracker_exponential_increase() {
        let mut tracker = BackoffTracker::new();

        tracker.record_attempt("box-1");
        let d1 = tracker.current_delay("box-1");

        tracker.record_attempt("box-1");
        let d2 = tracker.current_delay("box-1");

        tracker.record_attempt("box-1");
        let d3 = tracker.current_delay("box-1");

        // Each delay should double
        assert!(d2 > d1, "d2={d2:?} should be > d1={d1:?}");
        assert!(d3 > d2, "d3={d3:?} should be > d2={d2:?}");
    }

    #[test]
    fn test_backoff_tracker_caps_at_max() {
        let mut tracker = BackoffTracker::new();

        // Record many attempts to exceed max
        for _ in 0..20 {
            tracker.record_attempt("box-1");
        }

        let delay = tracker.current_delay("box-1");
        assert!(
            delay <= MAX_BACKOFF,
            "delay={delay:?} should be <= {MAX_BACKOFF:?}"
        );
    }

    #[test]
    fn test_backoff_tracker_default_delay() {
        let tracker = BackoffTracker::new();
        assert_eq!(tracker.current_delay("unknown"), MIN_BACKOFF);
    }

    #[test]
    fn test_backoff_tracker_remove() {
        let mut tracker = BackoffTracker::new();
        tracker.record_attempt("box-1");
        assert!(!tracker.ready("box-1"));

        tracker.entries.remove("box-1");
        assert!(tracker.ready("box-1"));
        assert_eq!(tracker.current_delay("box-1"), MIN_BACKOFF);
    }

    #[test]
    fn test_backoff_tracker_independent_boxes() {
        let mut tracker = BackoffTracker::new();
        tracker.record_attempt("box-1");

        // box-2 should still be ready
        assert!(tracker.ready("box-2"));
    }

    #[test]
    fn test_backoff_entry_mark_dead_resets_running_tracker() {
        let mut entry = BackoffEntry::new();
        entry.mark_running();
        assert!(entry.last_seen_running.is_some());

        entry.mark_dead();
        assert!(entry.last_seen_running.is_none());
    }

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
    fn test_unhealthy_restart_candidate_respects_restart_policy() {
        let mut record = make_record("id-1", "box", "running", Some(1));
        record.health_check = Some(health_check());
        record.health_status = "unhealthy".to_string();
        record.restart_policy = "no".to_string();
        assert!(!is_unhealthy_restart_candidate(&record));

        record.restart_policy = "on-failure:2".to_string();
        record.restart_count = 2;
        assert!(!is_unhealthy_restart_candidate(&record));

        record.restart_count = 1;
        assert!(is_unhealthy_restart_candidate(&record));
    }

    #[test]
    fn test_restart_log_line_for_dead_includes_policy_and_exit_code() {
        let mut record = make_record("id-1", "box", "dead", None);
        record.short_id = "id1".to_string();
        record.restart_policy = "always".to_string();
        record.exit_code = Some(137);

        let line = restart_log_line(&record, RestartReason::Dead);

        assert!(line.contains("restarting dead box box (id1"));
        assert!(line.contains("policy: always"));
        assert!(line.contains("exit: 137"));
    }

    #[test]
    fn test_backoff_log_line_includes_name_and_short_id() {
        let mut record = make_record("id-1", "box", "dead", None);
        record.short_id = "id1".to_string();

        let line = backoff_log_line(&record, Duration::from_secs(4));

        assert!(line.contains("box box (id1)"));
        assert!(line.contains("4s remaining"));
    }
}
