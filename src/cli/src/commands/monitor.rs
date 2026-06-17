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

    // Exit promptly on SIGTERM/SIGINT, but only AFTER a complete poll_once: the
    // boot→persist step inside poll_once leaves a freshly-booted microVM tracked
    // only once StateFile::modify records its pid, so interrupting it mid-flight
    // would orphan the VM (reparented to init) while its record stays dead/pid=None,
    // and the next start would boot a SECOND VM. The CRI server installs the same
    // handler; the monitor was the inconsistent outlier (no handler → a stop
    // mid-poll orphaned the in-flight boot).
    let mut shutdown = std::pin::pin!(monitor_shutdown_signal());
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
        tokio::select! {
            _ = tokio::time::sleep(interval) => {}
            _ = &mut shutdown => {
                println!("a3s-box monitor: received shutdown signal, exiting after completing the poll cycle");
                break;
            }
        }
    }
    Ok(())
}

/// Resolve when the monitor receives SIGTERM/SIGINT (Ctrl-C on non-unix). Mirrors
/// the CRI server's `shutdown_signal` so the always-on supervisor daemon shuts
/// down gracefully instead of being terminated at an arbitrary suspension point.
#[cfg(unix)]
async fn monitor_shutdown_signal() {
    use tokio::signal::unix::{signal, SignalKind};
    match (
        signal(SignalKind::terminate()),
        signal(SignalKind::interrupt()),
    ) {
        (Ok(mut sigterm), Ok(mut sigint)) => {
            tokio::select! {
                _ = sigterm.recv() => {}
                _ = sigint.recv() => {}
            }
        }
        // Could not install handlers — never resolve, so the loop keeps polling
        // (matches the previous always-on behaviour rather than exiting early).
        _ => std::future::pending::<()>().await,
    }
}

#[cfg(not(unix))]
async fn monitor_shutdown_signal() {
    let _ = tokio::signal::ctrl_c().await;
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
            // Only signal a PID we can confirm is still this box's shim — a
            // reused PID after a crash/reboot must never be SIGTERM'd.
            if let Some(pid) = record.pid {
                if crate::process::is_process_alive_with_identity(pid, record.pid_start_time) {
                    crate::process::graceful_stop(pid, libc::SIGTERM, 10).await;
                }
            }
            tracker.mark_dead(&box_id);
            // Mark as dead so boot_from_record works; re-load fresh under the lock.
            // graceful_stop above can take up to 10s, during which the user may have
            // `stop`ped (or `rm`ed) the box. Re-validate fresh state and ABORT the
            // restart if so — otherwise we silently resurrect a box the user
            // explicitly stopped (overwriting their stopped/stopped_by_user record).
            let proceed = StateFile::modify(|s| {
                Ok::<bool, std::io::Error>(match s.find_by_id_mut(&box_id) {
                    Some(rec) if health_restart_still_wanted(rec) => {
                        rec.status = "dead".to_string();
                        rec.pid = None;
                        rec.health_status = "none".to_string();
                        rec.health_retries = 0;
                        true
                    }
                    // User stopped/removed the box during the graceful-stop window.
                    _ => false,
                })
            })?;
            if !proceed {
                println!(
                    "monitor: box {name} ({short_id}) health-restart aborted — stopped by the user during shutdown",
                    name = record.name,
                    short_id = record.short_id,
                );
                continue;
            }
        } else {
            tracker.mark_dead(&box_id);
            println!("{}", restart_log_line(&record, RestartReason::Dead));
        }

        // Attempt restart, SERIALIZED per box via a per-box boot lock so a
        // concurrent user `restart`/`start` and this monitor restart cannot both
        // boot the same box (the second record write would overwrite the first's
        // pid, orphaning a VM). The orphan-on-`rm`-during-boot teardown now lives
        // inside `boot_and_record`.
        match boot::boot_and_record(&record, boot::RestartCountUpdate::Increment).await {
            Ok(boot::BootOutcome::Restarted { restart_count }) => {
                tracker.record_attempt(&box_id);
                println!(
                    "monitor: box {name} ({short_id}) restarted (count: {restart_count})",
                    name = record.name,
                    short_id = record.short_id,
                );
            }
            Ok(boot::BootOutcome::AlreadyRunning) => {
                // Another actor (a user restart/start) already brought this box
                // back under the per-box boot lock — nothing to do.
                println!(
                    "monitor: box {name} ({short_id}) already restarted by another actor; skipping",
                    name = record.name,
                    short_id = record.short_id,
                );
            }
            Ok(boot::BootOutcome::RemovedDuringBoot) => {
                tracker.record_attempt(&box_id);
                eprintln!(
                    "monitor: box {name} ({short_id}) was removed during restart; tore down the orphaned VM",
                    name = record.name,
                    short_id = record.short_id,
                );
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

/// Whether a health-restart should still proceed after the (up-to-10s)
/// graceful-stop await, given the FRESHLY-loaded record. If the user `stop`ped
/// the box (status=stopped or stopped_by_user) during that window, the restart
/// must abort so we don't resurrect a box the user explicitly stopped.
fn health_restart_still_wanted(record: &BoxRecord) -> bool {
    record.status != "stopped" && !record.stopped_by_user
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

    // Run the due probes concurrently with bounded fan-out (see probe_all): the
    // old serial loop made one wedged-but-connectable guest stall every other
    // box's health check AND every restart this cycle, since each probe is
    // host-bounded to its timeout + slack. State writes stay serialized below.
    let results = probe_all(probes).await;

    // Apply each result under the state lock (writes stay serialized; only the
    // slow network probes were parallelized). Re-load fresh per box so a
    // concurrent CLI/health-checker write is preserved.
    for (box_id, healthy, checked_at) in results {
        StateFile::modify(|s| {
            if let Some(record) = s.find_by_id_mut(&box_id) {
                health::apply_probe_result(record, healthy, checked_at);
            }
            Ok::<(), std::io::Error>(())
        })?;
    }

    Ok(())
}

/// Health-check probe input: (box id, exec socket path, health check).
#[cfg(not(windows))]
type ProbeJob = (String, std::path::PathBuf, crate::state::HealthCheck);

/// Run the given probes concurrently with bounded fan-out, returning
/// `(box_id, healthy, checked_at)` for each. Production wrapper over
/// [`probe_all_with`] using the real exec probe.
#[cfg(not(windows))]
async fn probe_all(probes: Vec<ProbeJob>) -> Vec<(String, bool, chrono::DateTime<chrono::Utc>)> {
    probe_all_with(probes, |sock, cmd, timeout_ns| async move {
        health::run_probe(&sock, &cmd, timeout_ns).await
    })
    .await
}

/// Bounded-concurrency fan-out over health probes. The probe function is
/// injected so the concurrency is unit-testable without a live guest.
#[cfg(not(windows))]
async fn probe_all_with<F, Fut>(
    probes: Vec<ProbeJob>,
    probe: F,
) -> Vec<(String, bool, chrono::DateTime<chrono::Utc>)>
where
    F: Fn(std::path::PathBuf, Vec<String>, u64) -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    use futures::stream::StreamExt;
    // Cap on in-flight probes so a large fleet can't open an unbounded number of
    // exec connections at once.
    const MAX_CONCURRENT_PROBES: usize = 16;
    futures::stream::iter(probes)
        .map(|(box_id, exec_socket_path, health_check)| {
            let timeout_ns = health::probe_timeout_ns(&health_check);
            let fut = probe(exec_socket_path, health_check.cmd, timeout_ns);
            async move { (box_id, fut.await, chrono::Utc::now()) }
        })
        .buffer_unordered(MAX_CONCURRENT_PROBES)
        .collect()
        .await
}

#[cfg(windows)]
async fn run_due_health_checks(_state: &StateFile) -> Result<(), Box<dyn std::error::Error>> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_helpers::fixtures::make_record;

    #[test]
    fn health_restart_aborts_if_user_stopped_during_window() {
        // Still running → a health-restart proceeds.
        let running = make_record("id-1", "box", "running", Some(1));
        assert!(health_restart_still_wanted(&running));
        // User `stop`ped the box during the up-to-10s graceful-stop window → abort
        // (do NOT resurrect a box the user explicitly stopped).
        let stopped = make_record("id-1", "box", "stopped", None);
        assert!(!health_restart_still_wanted(&stopped));
        // stopped_by_user set (even if the status field still reads running) → abort.
        let mut by_user = make_record("id-1", "box", "running", Some(1));
        by_user.stopped_by_user = true;
        assert!(!health_restart_still_wanted(&by_user));
    }

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

    // The per-cycle health probes must run concurrently (bounded), not serially:
    // a serial loop made one slow/wedged guest stall every other box's check and
    // every restart that cycle. With a fake 200ms probe, 8 boxes complete in
    // ~200ms (concurrent) instead of ~1.6s (serial), and genuinely overlap.
    #[cfg(not(windows))]
    #[tokio::test]
    async fn probe_all_runs_probes_concurrently() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;
        use std::time::{Duration, Instant};

        let n = 8usize;
        let probes: Vec<ProbeJob> = (0..n)
            .map(|i| {
                (
                    format!("box-{i}"),
                    std::path::PathBuf::from("/nonexistent"),
                    crate::state::HealthCheck {
                        cmd: vec!["true".to_string()],
                        interval_secs: 0,
                        timeout_secs: 5,
                        retries: 3,
                        start_period_secs: 0,
                    },
                )
            })
            .collect();

        let in_flight = Arc::new(AtomicUsize::new(0));
        let max_in_flight = Arc::new(AtomicUsize::new(0));

        let start = Instant::now();
        let results = probe_all_with(probes, |_sock, _cmd, _timeout_ns| {
            let in_flight = Arc::clone(&in_flight);
            let max_in_flight = Arc::clone(&max_in_flight);
            async move {
                let cur = in_flight.fetch_add(1, Ordering::SeqCst) + 1;
                max_in_flight.fetch_max(cur, Ordering::SeqCst);
                tokio::time::sleep(Duration::from_millis(200)).await;
                in_flight.fetch_sub(1, Ordering::SeqCst);
                false
            }
        })
        .await;
        let elapsed = start.elapsed();

        // Every probe produced a result — none dropped by the fan-out.
        assert_eq!(results.len(), n);
        assert!(results.iter().all(|(_, healthy, _)| !*healthy));
        // Serial would be n × 200ms = 1.6s; concurrent (cap 16 ≥ 8) ≈ 200ms.
        assert!(
            elapsed < Duration::from_millis(900),
            "probes did not run concurrently: {elapsed:?}"
        );
        assert!(
            max_in_flight.load(Ordering::SeqCst) >= 2,
            "expected overlapping probes, max in-flight was {}",
            max_in_flight.load(Ordering::SeqCst)
        );
    }
}
