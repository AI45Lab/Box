//! `a3s-box container-update` command — Update resource limits on a running box.
//!
//! Similar to `docker update`, allows changing CPU and memory limits
//! without restarting the box. Changes are persisted to the state file.

use clap::Args;

use super::common;
use crate::output::parse_memory;
use crate::resolve;
use crate::state::StateFile;

#[derive(Args)]
pub struct ContainerUpdateArgs {
    /// Box name or ID
    pub name: String,

    /// Number of CPUs
    #[arg(long)]
    pub cpus: Option<u32>,

    /// Memory limit (e.g., "512m", "2g")
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
    let mut updated = Vec::new();

    if let Some(cpus) = args.cpus {
        record.cpus = cpus;
        updated.push(format!("cpus={cpus}"));
    }

    if let Some(ref mem_str) = args.memory {
        let mb = parse_memory(mem_str).map_err(|e| format!("Invalid --memory: {e}"))?;
        record.memory_mb = mb;
        updated.push(format!("memory={mem_str}"));
    }

    if let Some(ref reservation) = args.memory_reservation {
        let bytes = common::parse_memory_bytes(reservation)
            .map_err(|e| format!("Invalid --memory-reservation: {e}"))?;
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
        record.resource_limits.memory_swap = Some(val);
        updated.push(format!("memory-swap={swap}"));
    }

    if let Some(pids) = args.pids_limit {
        record.resource_limits.pids_limit = Some(pids);
        updated.push(format!("pids-limit={pids}"));
    }

    if let Some(shares) = args.cpu_shares {
        record.resource_limits.cpu_shares = Some(shares);
        updated.push(format!("cpu-shares={shares}"));
    }

    if let Some(quota) = args.cpu_quota {
        record.resource_limits.cpu_quota = Some(quota);
        updated.push(format!("cpu-quota={quota}"));
    }

    if let Some(period) = args.cpu_period {
        record.resource_limits.cpu_period = Some(period);
        updated.push(format!("cpu-period={period}"));
    }

    if let Some(ref cpuset) = args.cpuset_cpus {
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

    state.save()?;
    println!("{name}");

    Ok(())
}
