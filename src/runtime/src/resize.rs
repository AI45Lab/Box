//! Live resource resize for running MicroVMs.
//!
//! Tier 1 (vCPU count, memory size): NOT supported — libkrun sets these at
//! boot via `krun_set_vm_config` and exposes no hot-resize API.
//!
//! Tier 2 (cgroup-based limits): Supported on Linux guests by writing to
//! cgroup v2 control files inside the guest via the exec channel.

use a3s_box_core::config::ResourceLimits;
use a3s_box_core::error::{BoxError, Result};

/// A resource update request.
///
/// Fields set to `None` are left unchanged.
#[derive(Debug, Clone, Default)]
pub struct ResourceUpdate {
    /// vCPU count change (Tier 1 — will be rejected).
    pub vcpus: Option<u32>,
    /// Memory in MiB change (Tier 1 — will be rejected).
    pub memory_mb: Option<u32>,
    /// Cgroup-based limits (Tier 2 — applied via exec).
    pub limits: ResourceLimits,
}

/// Result of a resize attempt.
#[derive(Debug)]
pub struct ResizeResult {
    /// Fields that were successfully applied.
    pub applied: Vec<String>,
    /// Fields that were rejected with reasons.
    pub rejected: Vec<(String, String)>,
}

impl ResourceUpdate {
    /// Check if any Tier 1 (immutable) fields are requested.
    pub fn has_tier1_changes(&self) -> bool {
        self.vcpus.is_some() || self.memory_mb.is_some()
    }

    /// Check if any Tier 2 (cgroup) fields are requested.
    pub fn has_tier2_changes(&self) -> bool {
        self.limits.cpu_shares.is_some()
            || self.limits.cpu_quota.is_some()
            || self.limits.cpu_period.is_some()
            || self.limits.memory_reservation.is_some()
            || self.limits.memory_swap.is_some()
            || self.limits.pids_limit.is_some()
            || self.limits.cpuset_cpus.is_some()
    }

    /// Build shell commands to apply Tier 2 cgroup changes inside the guest.
    ///
    /// Returns a list of shell commands that write to cgroup v2 control files.
    /// The guest init process runs as PID 1 in the root cgroup, so we write
    /// to `/sys/fs/cgroup/` (the root cgroup or the box's cgroup slice).
    pub fn build_cgroup_commands(&self) -> Vec<String> {
        let mut cmds = Vec::new();
        let cg = "/sys/fs/cgroup";

        // cpu.max: "$QUOTA $PERIOD" (or "max $PERIOD" for unlimited)
        if self.limits.cpu_quota.is_some() || self.limits.cpu_period.is_some() {
            let quota = self
                .limits
                .cpu_quota
                .map(|q| {
                    if q < 0 {
                        "max".to_string()
                    } else {
                        q.to_string()
                    }
                })
                .unwrap_or_else(|| "max".to_string());
            let period = self.limits.cpu_period.unwrap_or(100_000);
            cmds.push(format!("echo '{quota} {period}' > {cg}/cpu.max"));
        }

        // cpu.weight: 1-10000 (maps from Docker's cpu-shares 2-262144)
        if let Some(shares) = self.limits.cpu_shares {
            // Docker shares (2-262144) → cgroup v2 weight (1-10000)
            // Formula: weight = (1 + ((shares - 2) * 9999) / 262142)
            let weight = if shares <= 2 {
                1
            } else {
                1 + ((shares.saturating_sub(2)) * 9999 / 262142).min(10000)
            };
            cmds.push(format!("echo '{weight}' > {cg}/cpu.weight"));
        }

        // memory.low (soft limit / reservation)
        if let Some(reservation) = self.limits.memory_reservation {
            cmds.push(format!("echo '{reservation}' > {cg}/memory.low"));
        }

        // memory.swap.max
        if let Some(swap) = self.limits.memory_swap {
            let val = if swap < 0 {
                "max".to_string()
            } else {
                swap.to_string()
            };
            cmds.push(format!("echo '{val}' > {cg}/memory.swap.max"));
        }

        // pids.max
        if let Some(pids) = self.limits.pids_limit {
            cmds.push(format!("echo '{pids}' > {cg}/pids.max"));
        }

        // cpuset.cpus
        if let Some(ref cpuset) = self.limits.cpuset_cpus {
            cmds.push(format!("echo '{cpuset}' > {cg}/cpuset.cpus"));
        }

        cmds
    }
}

/// Validate a resource update request.
///
/// Returns `Err` if Tier 1 changes are requested (not supported by libkrun).
/// Returns `Ok(())` if only Tier 2 changes or no changes.
pub fn validate_update(update: &ResourceUpdate) -> Result<()> {
    if let Some(vcpus) = update.vcpus {
        return Err(BoxError::ResizeError(format!(
            "Cannot change vCPU count to {} on a running VM: libkrun does not support \
             hot-plug vCPUs. Stop and recreate the box with the desired CPU count.",
            vcpus
        )));
    }
    if let Some(memory_mb) = update.memory_mb {
        return Err(BoxError::ResizeError(format!(
            "Cannot change memory to {}MB on a running VM: libkrun does not support \
             memory ballooning. Stop and recreate the box with the desired memory size.",
            memory_mb
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_empty_update_has_no_changes() {
        let update = ResourceUpdate::default();
        assert!(!update.has_tier1_changes());
        assert!(!update.has_tier2_changes());
        assert!(update.build_cgroup_commands().is_empty());
    }

    #[test]
    fn test_tier1_vcpus_detected() {
        let update = ResourceUpdate {
            vcpus: Some(4),
            ..Default::default()
        };
        assert!(update.has_tier1_changes());
        assert!(!update.has_tier2_changes());
    }

    #[test]
    fn test_tier1_memory_detected() {
        let update = ResourceUpdate {
            memory_mb: Some(2048),
            ..Default::default()
        };
        assert!(update.has_tier1_changes());
    }

    #[test]
    fn test_validate_rejects_vcpu_change() {
        let update = ResourceUpdate {
            vcpus: Some(8),
            ..Default::default()
        };
        let err = validate_update(&update).unwrap_err();
        assert!(err.to_string().contains("vCPU count"));
        assert!(err.to_string().contains("libkrun"));
    }

    #[test]
    fn test_validate_rejects_memory_change() {
        let update = ResourceUpdate {
            memory_mb: Some(4096),
            ..Default::default()
        };
        let err = validate_update(&update).unwrap_err();
        assert!(err.to_string().contains("memory"));
        assert!(err.to_string().contains("ballooning"));
    }

    #[test]
    fn test_validate_allows_tier2_only() {
        let update = ResourceUpdate {
            limits: ResourceLimits {
                cpu_shares: Some(512),
                pids_limit: Some(100),
                ..Default::default()
            },
            ..Default::default()
        };
        assert!(validate_update(&update).is_ok());
    }

    #[test]
    fn test_cpu_max_command() {
        let update = ResourceUpdate {
            limits: ResourceLimits {
                cpu_quota: Some(50000),
                cpu_period: Some(100000),
                ..Default::default()
            },
            ..Default::default()
        };
        let cmds = update.build_cgroup_commands();
        assert_eq!(cmds.len(), 1);
        assert!(cmds[0].contains("50000 100000"));
        assert!(cmds[0].contains("cpu.max"));
    }

    #[test]
    fn test_cpu_max_unlimited_quota() {
        let update = ResourceUpdate {
            limits: ResourceLimits {
                cpu_quota: Some(-1),
                ..Default::default()
            },
            ..Default::default()
        };
        let cmds = update.build_cgroup_commands();
        assert_eq!(cmds.len(), 1);
        assert!(cmds[0].contains("max 100000"));
    }

    #[test]
    fn test_cpu_weight_conversion() {
        let update = ResourceUpdate {
            limits: ResourceLimits {
                cpu_shares: Some(1024),
                ..Default::default()
            },
            ..Default::default()
        };
        let cmds = update.build_cgroup_commands();
        assert_eq!(cmds.len(), 1);
        assert!(cmds[0].contains("cpu.weight"));
    }

    #[test]
    fn test_cpu_weight_minimum() {
        let update = ResourceUpdate {
            limits: ResourceLimits {
                cpu_shares: Some(2),
                ..Default::default()
            },
            ..Default::default()
        };
        let cmds = update.build_cgroup_commands();
        assert!(cmds[0].contains("'1'"));
    }

    #[test]
    fn test_memory_reservation_command() {
        let update = ResourceUpdate {
            limits: ResourceLimits {
                memory_reservation: Some(536870912), // 512MB
                ..Default::default()
            },
            ..Default::default()
        };
        let cmds = update.build_cgroup_commands();
        assert_eq!(cmds.len(), 1);
        assert!(cmds[0].contains("536870912"));
        assert!(cmds[0].contains("memory.low"));
    }

    #[test]
    fn test_memory_swap_unlimited() {
        let update = ResourceUpdate {
            limits: ResourceLimits {
                memory_swap: Some(-1),
                ..Default::default()
            },
            ..Default::default()
        };
        let cmds = update.build_cgroup_commands();
        assert!(cmds[0].contains("'max'"));
        assert!(cmds[0].contains("memory.swap.max"));
    }

    #[test]
    fn test_pids_max_command() {
        let update = ResourceUpdate {
            limits: ResourceLimits {
                pids_limit: Some(256),
                ..Default::default()
            },
            ..Default::default()
        };
        let cmds = update.build_cgroup_commands();
        assert!(cmds[0].contains("256"));
        assert!(cmds[0].contains("pids.max"));
    }

    #[test]
    fn test_cpuset_command() {
        let update = ResourceUpdate {
            limits: ResourceLimits {
                cpuset_cpus: Some("0,1,3".to_string()),
                ..Default::default()
            },
            ..Default::default()
        };
        let cmds = update.build_cgroup_commands();
        assert!(cmds[0].contains("0,1,3"));
        assert!(cmds[0].contains("cpuset.cpus"));
    }

    #[test]
    fn test_multiple_tier2_commands() {
        let update = ResourceUpdate {
            limits: ResourceLimits {
                cpu_shares: Some(512),
                pids_limit: Some(100),
                memory_reservation: Some(268435456),
                ..Default::default()
            },
            ..Default::default()
        };
        let cmds = update.build_cgroup_commands();
        assert_eq!(cmds.len(), 3);
    }

    #[test]
    fn test_resize_result_structure() {
        let result = ResizeResult {
            applied: vec!["cpu.weight".to_string()],
            rejected: vec![("vcpus".to_string(), "not supported".to_string())],
        };
        assert_eq!(result.applied.len(), 1);
        assert_eq!(result.rejected.len(), 1);
    }
}
