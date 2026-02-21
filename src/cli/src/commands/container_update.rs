//! `a3s-box container-update` command — Update resource limits on a running box.
//!
//! Similar to `docker update`, allows changing cgroup-based limits on a running
//! box without restarting it. Changes are applied live via the exec channel and
//! persisted to the state file.
//!
//! Tier 1 limits (--cpus, --memory) cannot be changed on a running microVM
//! because libkrun does not expose a hot-resize API. These are rejected with
//! a clear error message.

use clap::Args;

use a3s_box_core::exec::ExecRequest;
use a3s_box_runtime::resize::{validate_update, ResourceUpdate};
use a3s_box_runtime::ExecClient;

use super::common;
use crate::output::parse_memory;
use crate::resolve;
use crate::state::StateFile;

#[derive(Args)]
pub struct ContainerUpdateArgs {
    /// Box name or ID
    pub name: String,

    /// Number of CPUs (requires restart — cannot hot-resize)
    #[arg(long)]
    pub cpus: Option<u32>,

    /// Memory limit (requires restart — cannot hot-resize)
    #[arg(long)]
    pub memory: Option<String>,

    /// Memory reservation/soft limit (e.g., "256m", "1g")
    #[arg(long)]
    pub memory_reservation: Option<String>,

    /// Memory+swap limit (e.g., "1g", "-1" for unlimited)
    #[arg(long)]
    pub memory_swap: Option<String>,

    /// Limit PIDs inside the box
    #[arg(long)]
    pub pids_limit: Option<u64>,

    /// CPU shares (relative weight, 2-262144)
    #[arg(long)]
    pub cpu_shares: Option<u64>,

    /// CPU quota in microseconds per cpu-period
    #[arg(long)]
    pub cpu_quota: Option<i64>,

    /// CPU period in microseconds
    #[arg(long)]
    pub cpu_period: Option<u64>,

    /// Pin to specific CPUs (e.g., "0,1,3" or "0-3")
    #[arg(long)]
    pub cpuset_cpus: Option<String>,

    /// Restart policy: no, always, on-failure, unless-stopped
    #[arg(long)]
    pub restart: Option<String>,
}

pub async fn execute(args: ContainerUpdateArgs) -> Result<(), Box<dyn std::error::Error>> {
    let mut state = StateFile::load_default()?;
    let record = resolve::resolve_mut(&mut state, &args.name)?;

    let name = record.name.clone();
    let is_running = record.status == "running";
    let mut updated = Vec::new();

    // Build ResourceUpdate for live application
    let mut update = ResourceUpdate::default();

    // Tier 1: vCPU and memory — reject if box is running
    if let Some(cpus) = args.cpus {
        update.vcpus = Some(cpus);
        record.cpus = cpus;
        updated.push(format!("cpus={cpus}"));
    }

    if let Some(ref mem_str) = args.memory {
        let mb = parse_memory(mem_str).map_err(|e| format!("Invalid --memory: {e}"))?;
        update.memory_mb = Some(mb);
        record.memory_mb = mb;
        updated.push(format!("memory={mem_str}"));
    }

    // Tier 2: cgroup-based limits — can be applied live
    if let Some(ref reservation) = args.memory_reservation {
        let bytes = common::parse_memory_bytes(reservation)
            .map_err(|e| format!("Invalid --memory-reservation: {e}"))?;
        update.limits.memory_reservation = Some(bytes);
        record.resource_limits.memory_reservation = Some(bytes);
        updated.push(format!("memory-reservation={reservation}"));
    }

    if let Some(ref swap) = args.memory_swap {
        let val = if swap == "-1" {
            -1i64
        } else {
            common::parse_memory_bytes(swap).map_err(|e| format!("Invalid --memory-swap: {e}"))?
                as i64
        };
        update.limits.memory_swap = Some(val);
        record.resource_limits.memory_swap = Some(val);
        updated.push(format!("memory-swap={swap}"));
    }

    if let Some(pids) = args.pids_limit {
        update.limits.pids_limit = Some(pids);
        record.resource_limits.pids_limit = Some(pids);
        updated.push(format!("pids-limit={pids}"));
    }

    if let Some(shares) = args.cpu_shares {
        update.limits.cpu_shares = Some(shares);
        record.resource_limits.cpu_shares = Some(shares);
        updated.push(format!("cpu-shares={shares}"));
    }

    if let Some(quota) = args.cpu_quota {
        update.limits.cpu_quota = Some(quota);
        record.resource_limits.cpu_quota = Some(quota);
        updated.push(format!("cpu-quota={quota}"));
    }

    if let Some(period) = args.cpu_period {
        update.limits.cpu_period = Some(period);
        record.resource_limits.cpu_period = Some(period);
        updated.push(format!("cpu-period={period}"));
    }

    if let Some(ref cpuset) = args.cpuset_cpus {
        update.limits.cpuset_cpus = Some(cpuset.clone());
        record.resource_limits.cpuset_cpus = Some(cpuset.clone());
        updated.push(format!("cpuset-cpus={cpuset}"));
    }

    if let Some(ref restart) = args.restart {
        let (policy, max_count) = crate::state::parse_restart_policy(restart)
            .map_err(|e| -> Box<dyn std::error::Error> { e.into() })?;
        record.restart_policy = policy;
        record.max_restart_count = max_count;
        updated.push(format!("restart={restart}"));
    }

    if updated.is_empty() {
        println!("No updates specified.");
        return Ok(());
    }

    // If the box is running, validate and apply live changes
    if is_running {
        // Tier 1 changes on a running box → clear error
        if update.has_tier1_changes() {
            validate_update(&update)
                .map_err(|e| -> Box<dyn std::error::Error> { e.to_string().into() })?;
        }

        // Tier 2 changes → apply via exec channel
        if update.has_tier2_changes() {
            let exec_socket_path = if !record.exec_socket_path.as_os_str().is_empty() {
                record.exec_socket_path.clone()
            } else {
                record.box_dir.join("sockets").join("exec.sock")
            };

            if !exec_socket_path.exists() {
                eprintln!(
                    "Warning: exec socket not found at {}, changes saved but not applied live",
                    exec_socket_path.display()
                );
            } else {
                let client = ExecClient::connect(&exec_socket_path).await?;
                let commands = update.build_cgroup_commands();

                for cmd_str in &commands {
                    let request = ExecRequest {
                        cmd: vec!["sh".to_string(), "-c".to_string(), cmd_str.clone()],
                        timeout_ns: 5_000_000_000,
                        env: vec![],
                        working_dir: None,
                        stdin: None,
                        user: None,
                        streaming: false,
                    };

                    match client.exec_command(&request).await {
                        Ok(output) if output.exit_code == 0 => {}
                        Ok(output) => {
                            let stderr = String::from_utf8_lossy(&output.stderr);
                            eprintln!(
                                "Warning: cgroup update failed (exit {}): {}",
                                output.exit_code,
                                stderr.trim()
                            );
                        }
                        Err(e) => {
                            eprintln!("Warning: failed to apply live update: {e}");
                        }
                    }
                }
            }
        }
    }

    // Always persist to state file (applies on next restart for Tier 1)
    state.save()?;
    println!("{name}");

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use a3s_box_core::config::ResourceLimits;

    #[test]
    fn test_tier1_rejected_on_running() {
        let update = ResourceUpdate {
            vcpus: Some(4),
            ..Default::default()
        };
        let err = validate_update(&update);
        assert!(err.is_err());
        assert!(err.unwrap_err().to_string().contains("vCPU"));
    }

    #[test]
    fn test_tier2_builds_commands() {
        let update = ResourceUpdate {
            limits: ResourceLimits {
                cpu_shares: Some(512),
                pids_limit: Some(100),
                ..Default::default()
            },
            ..Default::default()
        };
        assert!(validate_update(&update).is_ok());
        let cmds = update.build_cgroup_commands();
        assert_eq!(cmds.len(), 2);
    }

    #[test]
    fn test_memory_change_rejected() {
        let update = ResourceUpdate {
            memory_mb: Some(2048),
            ..Default::default()
        };
        let err = validate_update(&update);
        assert!(err.is_err());
        assert!(err.unwrap_err().to_string().contains("memory"));
    }
}
