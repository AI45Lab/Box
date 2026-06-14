//! Per-container cgroup v2 (memory + cpu limits) for the guest.
//!
//! The CRI `LinuxContainerResources` limits are enforced inside the guest by
//! placing the container — and, crucially, every process it forks — in its own
//! cgroup v2 with `memory.max` and/or `cpu.max` set. The container joins the
//! cgroup from its pre-exec hook (writing its own PID to `cgroup.procs` before
//! exec), so workers it forks immediately cannot escape the limit. When memory
//! is exceeded the kernel OOM-killer reaps the cgroup; `memory.events`'s
//! `oom_kill` counter then lets us report the exit reason as `OOMKilled`,
//! matching runc/containerd.
//!
//! This is Linux-only and entirely best-effort: any failure (cgroup v2 absent,
//! permission denied, controller unavailable) degrades to "no enforcement, no
//! OOM detection" rather than failing the container launch.

#![cfg(target_os = "linux")]

use std::io::Write;
use std::sync::atomic::{AtomicU64, Ordering};

use tracing::{debug, warn};

const CGROUP_ROOT: &str = "/sys/fs/cgroup";
static CGROUP_SEQ: AtomicU64 = AtomicU64::new(0);

/// Ensure cgroup v2 is mounted at `/sys/fs/cgroup` with the `memory`, `cpu`, and
/// `pids` controllers delegated to child cgroups. Idempotent; returns `false`
/// only if cgroup v2 cannot be mounted or exposes no controllers at all. Each
/// individual limit (memory.max / cpu.max / pids.max) is best-effort in
/// `ContainerCgroup::create`, so a missing controller degrades to "that limit
/// is not enforced" rather than failing the launch.
fn ensure_cgroup2_ready() -> bool {
    let controllers_path = format!("{CGROUP_ROOT}/cgroup.controllers");
    if std::fs::metadata(&controllers_path).is_err() {
        // Not mounted yet — mount the unified hierarchy.
        use nix::mount::{mount, MsFlags};
        let _ = std::fs::create_dir_all(CGROUP_ROOT);
        if let Err(error) = mount(
            Some("cgroup2"),
            CGROUP_ROOT,
            Some("cgroup2"),
            MsFlags::empty(),
            None::<&str>,
        ) {
            warn!(error = %error, "cgroup: failed to mount cgroup2");
            return false;
        }
    }

    let available = match std::fs::read_to_string(&controllers_path) {
        Ok(controllers) => controllers,
        Err(error) => {
            warn!(error = %error, "cgroup: cannot read cgroup.controllers");
            return false;
        }
    };
    let has = |ctrl: &str| available.split_whitespace().any(|c| c == ctrl);

    // Delegate controllers to child cgroups via the root's subtree_control (the
    // root cgroup is exempt from the no-internal-processes rule). memory drives
    // limits + OOM accounting, cpu the cpu.max throttle, pids the process-count
    // cap (`--pids-limit`) — each best-effort.
    let subtree = format!("{CGROUP_ROOT}/cgroup.subtree_control");
    let current = std::fs::read_to_string(&subtree).unwrap_or_default();
    for ctrl in ["memory", "cpu", "pids"] {
        if has(ctrl) && !current.split_whitespace().any(|c| c == ctrl) {
            if let Err(error) = write_cgroup_file(&subtree, &format!("+{ctrl}")) {
                warn!(error = %error, ctrl, "cgroup: failed to delegate controller");
            }
        }
    }

    // Usable as long as the unified hierarchy exposes at least one controller;
    // the specific limit writes in `create` fail gracefully if their controller
    // is absent.
    if available.split_whitespace().next().is_none() {
        warn!("cgroup: no v2 controllers available in cgroup.controllers");
        return false;
    }
    true
}

fn write_cgroup_file(path: &str, value: &str) -> std::io::Result<()> {
    let mut file = std::fs::OpenOptions::new().write(true).open(path)?;
    file.write_all(value.as_bytes())
}

/// Map a CRI `cpu_shares` value (cgroup v1 range [2, 262144], default 1024) to a
/// cgroup v2 `cpu.weight` (range [1, 10000]), using runc's conversion.
fn shares_to_weight(shares: u64) -> u64 {
    let shares = shares.clamp(2, 262_144);
    (1 + ((shares - 2) * 9999) / 262_142).clamp(1, 10_000)
}

/// A per-container cgroup v2 (memory + cpu limits). Dropping it removes the
/// cgroup directory.
pub struct ContainerCgroup {
    path: String,
}

impl ContainerCgroup {
    /// Create a per-container cgroup applying the given limits: `memory.max`
    /// (bytes), `cpu.max` (`cpu_quota` µs per `cpu_period` µs), and/or `pids.max`
    /// (max process count, `--pids-limit`). Returns `None` when no limit is
    /// requested or cgroup v2 is unavailable, in which case the caller proceeds
    /// without enforcement.
    pub fn create(
        memory_max: Option<u64>,
        cpu_quota: Option<i64>,
        cpu_period: Option<u64>,
        cpu_shares: Option<u64>,
        pids_max: Option<u64>,
    ) -> Option<Self> {
        let want_memory = memory_max.is_some_and(|m| m > 0);
        let want_cpu = cpu_quota.is_some_and(|q| q > 0);
        let want_weight = cpu_shares.is_some_and(|s| s > 0);
        let want_pids = pids_max.is_some_and(|p| p > 0);
        if (!want_memory && !want_cpu && !want_weight && !want_pids) || !ensure_cgroup2_ready() {
            return None;
        }
        let seq = CGROUP_SEQ.fetch_add(1, Ordering::Relaxed);
        let path = format!("{CGROUP_ROOT}/box-{}-{}", std::process::id(), seq);
        if let Err(error) = std::fs::create_dir(&path) {
            warn!(error = %error, path, "cgroup: failed to create container cgroup");
            return None;
        }
        if want_memory {
            let limit = memory_max.unwrap_or(0);
            if let Err(error) = write_cgroup_file(&format!("{path}/memory.max"), &limit.to_string())
            {
                warn!(error = %error, "cgroup: failed to set memory.max");
                let _ = std::fs::remove_dir(&path);
                return None;
            }
            // Kill the whole cgroup on OOM so the container's main process dies
            // even if the over-allocating process was a child (best-effort).
            let _ = write_cgroup_file(&format!("{path}/memory.oom.group"), "1");
        }
        if want_cpu {
            // cgroup v2 `cpu.max` = "<quota_us> <period_us>"; CRI defaults the
            // period to 100ms when unset.
            let period = cpu_period.filter(|p| *p > 0).unwrap_or(100_000);
            let value = format!("{} {}", cpu_quota.unwrap_or(0), period);
            // Non-fatal: keep the cgroup so any memory limit still applies.
            if let Err(error) = write_cgroup_file(&format!("{path}/cpu.max"), &value) {
                warn!(error = %error, value, "cgroup: failed to set cpu.max");
            }
        }
        if want_weight {
            let weight = shares_to_weight(cpu_shares.unwrap_or(1024));
            if let Err(error) =
                write_cgroup_file(&format!("{path}/cpu.weight"), &weight.to_string())
            {
                warn!(error = %error, weight, "cgroup: failed to set cpu.weight");
            }
        }
        if want_pids {
            // cgroup v2 `pids.max` caps the number of processes/threads in the
            // cgroup; a fork past the limit fails with EAGAIN.
            let limit = pids_max.unwrap_or(0);
            if let Err(error) = write_cgroup_file(&format!("{path}/pids.max"), &limit.to_string()) {
                warn!(error = %error, limit, "cgroup: failed to set pids.max");
            }
        }
        debug!(
            path,
            ?memory_max,
            ?cpu_quota,
            ?cpu_shares,
            ?pids_max,
            "cgroup: created container cgroup"
        );
        Some(Self { path })
    }

    /// Path to this cgroup's `cgroup.procs` (where a process writes its own PID
    /// to join). Used by the pre-exec hook so the container — and every process
    /// it forks — is in the cgroup from birth (a parent-side join after spawn
    /// races with workers the container forks immediately).
    pub fn procs_path(&self) -> String {
        format!("{}/cgroup.procs", self.path)
    }

    /// Number of OOM kills recorded in this cgroup (`memory.events` `oom_kill`).
    /// A non-zero value means the container was OOM-killed.
    pub fn oom_kills(&self) -> u64 {
        let events = match std::fs::read_to_string(format!("{}/memory.events", self.path)) {
            Ok(events) => events,
            Err(_) => return 0,
        };
        events
            .lines()
            .find_map(|line| line.strip_prefix("oom_kill "))
            .and_then(|count| count.trim().parse::<u64>().ok())
            .unwrap_or(0)
    }
}

impl Drop for ContainerCgroup {
    fn drop(&mut self) {
        // Safe only once the cgroup is empty (the container has been reaped).
        if let Err(error) = std::fs::remove_dir(&self.path) {
            debug!(error = %error, path = %self.path, "cgroup: cleanup rmdir failed");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::shares_to_weight;

    #[test]
    fn test_shares_to_weight_mapping() {
        // Endpoints + the cgroup v1 default map to the runc-equivalent weights.
        assert_eq!(shares_to_weight(2), 1);
        assert_eq!(shares_to_weight(262_144), 10_000);
        assert_eq!(shares_to_weight(1024), 39); // runc's mapping for the default
                                                // Out-of-range inputs are clamped, never panic / overflow.
        assert_eq!(shares_to_weight(0), 1);
        assert_eq!(shares_to_weight(u64::MAX), 10_000);
    }
}
