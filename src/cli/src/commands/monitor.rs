//! `a3s-box monitor` command — Background daemon that restarts dead boxes.
//!
//! Polls `boxes.json` periodically, detects dead VMs via PID liveness checks,
//! and restarts boxes according to their restart policy. Uses exponential
//! backoff to prevent crash loops.
//!
//! Usage: `a3s-box monitor` (long-running, typically run as a background service)

use std::collections::HashMap;
use std::time::{Duration, Instant};

use clap::Args;

use crate::boot;
use crate::state::StateFile;

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
    let interval = Duration::from_secs(args.interval);
    let mut tracker = BackoffTracker::new();

    println!(
        "a3s-box monitor started (poll interval: {}s)",
        args.interval
    );

    loop {
        if let Err(e) = poll_once(&mut tracker).await {
            eprintln!("monitor: poll error: {e}");
        }
        tokio::time::sleep(interval).await;
    }
}

/// Single poll iteration: load state, find dead boxes, restart eligible ones.
async fn poll_once(tracker: &mut BackoffTracker) -> Result<(), Box<dyn std::error::Error>> {
    let mut state = StateFile::load_default()?;

    // Track running boxes for stability detection
    for record in state.records() {
        if record.status == "running" {
            tracker.mark_running(&record.id);
        }
    }

    // Find boxes that need restarting
    let candidates = state.pending_restarts();

    for box_id in candidates {
        // Check backoff
        if !tracker.ready(&box_id) {
            let delay = tracker.current_delay(&box_id);
            eprintln!(
                "monitor: box {} backing off ({:.0}s remaining)",
                &box_id[..12.min(box_id.len())],
                delay.as_secs_f64()
            );
            continue;
        }

        tracker.mark_dead(&box_id);

        // Attempt restart
        let record = match state.find_by_id(&box_id) {
            Some(r) => r.clone(),
            None => continue,
        };

        let name = record.name.clone();
        let short_id = record.short_id.clone();
        println!("monitor: restarting box {name} ({short_id})...");

        match boot::boot_from_record(&record).await {
            Ok(result) => {
                // Update record to running
                if let Some(rec) = state.find_by_id_mut(&box_id) {
                    rec.status = "running".to_string();
                    rec.pid = result.pid;
                    rec.started_at = Some(chrono::Utc::now());
                    rec.restart_count += 1;
                    rec.stopped_by_user = false;
                }
                state.save()?;
                tracker.record_attempt(&box_id);
                println!(
                    "monitor: box {name} ({short_id}) restarted (count: {})",
                    state.find_by_id(&box_id).map_or(0, |r| r.restart_count)
                );
            }
            Err(e) => {
                tracker.record_attempt(&box_id);
                let delay = tracker.current_delay(&box_id);
                eprintln!(
                    "monitor: failed to restart box {name} ({short_id}): {e} (next retry in {:.0}s)",
                    delay.as_secs_f64()
                );
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

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

        tracker.remove("box-1");
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
}
