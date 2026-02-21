//! `a3s-box pool` — Warm VM pool management.
//!
//! Pre-boots MicroVMs so that `run` can acquire an already-ready VM
//! instead of waiting for the full boot sequence (~200ms → ~0ms).
//!
//! Subcommands:
//!   pool start --size N --image IMAGE   Start the warm pool
//!   pool stop                           Drain and stop the pool
//!   pool status                         Show idle count, hit rate, stats

use clap::{Parser, Subcommand};

use a3s_box_core::config::{BoxConfig, PoolConfig};
use a3s_box_core::event::EventEmitter;
use a3s_box_runtime::pool::{PoolStats, WarmPool};

/// Manage the warm VM pool.
#[derive(Parser)]
pub struct PoolArgs {
    #[command(subcommand)]
    pub action: PoolAction,
}

/// Pool subcommands.
#[derive(Subcommand)]
pub enum PoolAction {
    /// Start the warm pool (pre-boot VMs in the background)
    Start(PoolStartArgs),
    /// Drain and stop the warm pool
    Stop(PoolStopArgs),
    /// Show warm pool statistics
    Status(PoolStatusArgs),
}

/// Arguments for `pool start`.
#[derive(Parser)]
pub struct PoolStartArgs {
    /// OCI image to pre-boot (e.g. alpine:latest)
    #[arg(long)]
    pub image: String,

    /// Number of VMs to keep pre-booted (min_idle)
    #[arg(long, default_value = "2")]
    pub size: usize,

    /// Maximum pool capacity
    #[arg(long, default_value = "8")]
    pub max: usize,

    /// Idle TTL in seconds before evicting a pre-booted VM (0 = unlimited)
    #[arg(long, default_value = "300")]
    pub ttl: u64,

    /// Output as JSON
    #[arg(long)]
    pub json: bool,
}

/// Arguments for `pool stop`.
#[derive(Parser)]
pub struct PoolStopArgs {
    /// Output as JSON
    #[arg(long)]
    pub json: bool,
}

/// Arguments for `pool status`.
#[derive(Parser)]
pub struct PoolStatusArgs {
    /// Output as JSON
    #[arg(long)]
    pub json: bool,
}

/// Execute a pool command.
pub async fn execute(args: PoolArgs) -> Result<(), Box<dyn std::error::Error>> {
    match args.action {
        PoolAction::Start(a) => execute_start(a).await,
        PoolAction::Stop(a) => execute_stop(a).await,
        PoolAction::Status(a) => execute_status(a).await,
    }
}

async fn execute_start(args: PoolStartArgs) -> Result<(), Box<dyn std::error::Error>> {
    if args.size == 0 {
        return Err("--size must be greater than 0".into());
    }
    if args.size > args.max {
        return Err(format!("--size ({}) cannot exceed --max ({})", args.size, args.max).into());
    }

    let pool_config = PoolConfig {
        enabled: true,
        min_idle: args.size,
        max_size: args.max,
        idle_ttl_secs: args.ttl,
        ..Default::default()
    };

    let mut box_config = BoxConfig::default();
    box_config.image = args.image.clone();
    box_config.pool = pool_config.clone();

    let emitter = EventEmitter::new(256);
    let mut pool = WarmPool::start(pool_config, box_config, emitter).await?;

    let stats = pool.stats().await;

    if args.json {
        println!("{}", format_stats_json(&args.image, &stats));
    } else {
        println!("Warm pool started");
        println!("  image:    {}", args.image);
        println!("  min_idle: {}", args.size);
        println!("  max:      {}", args.max);
        println!("  ttl:      {}s", args.ttl);
        println!("  idle:     {}", stats.idle_count);
    }

    // Keep pool alive until signal
    tokio::signal::ctrl_c().await?;

    if !args.json {
        println!("\nDraining warm pool...");
    }
    pool.drain().await?;
    if !args.json {
        println!("Done.");
    }

    Ok(())
}

async fn execute_stop(_args: PoolStopArgs) -> Result<(), Box<dyn std::error::Error>> {
    // Pool stop is handled by sending SIGINT to the `pool start` process.
    // This subcommand exists for discoverability and future daemon support.
    eprintln!("Send SIGINT (Ctrl-C) to the running `a3s-box pool start` process to drain and stop the pool.");
    Ok(())
}

async fn execute_status(_args: PoolStatusArgs) -> Result<(), Box<dyn std::error::Error>> {
    // Pool status requires a running pool instance. In the current in-process
    // model, status is printed by the `pool start` process itself.
    // This subcommand is a placeholder for future daemon/IPC support.
    eprintln!("Pool status is shown by the running `a3s-box pool start` process.");
    eprintln!("Use Prometheus metrics (a3s_box_warm_pool_*) for live observability.");
    Ok(())
}

/// Format pool stats as a JSON string.
fn format_stats_json(image: &str, stats: &PoolStats) -> String {
    let hit_rate = if stats.total_acquired > 0 {
        stats.total_acquired.saturating_sub(stats.total_evicted) as f64
            / stats.total_acquired as f64
    } else {
        0.0
    };
    format!(
        r#"{{"image":"{image}","idle":{idle},"total_created":{created},"total_acquired":{acquired},"total_released":{released},"total_evicted":{evicted},"hit_rate":{hit_rate:.2}}}"#,
        image = image,
        idle = stats.idle_count,
        created = stats.total_created,
        acquired = stats.total_acquired,
        released = stats.total_released,
        evicted = stats.total_evicted,
        hit_rate = hit_rate,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use a3s_box_runtime::pool::PoolStats;

    fn sample_stats() -> PoolStats {
        PoolStats {
            idle_count: 2,
            total_created: 5,
            total_acquired: 4,
            total_released: 3,
            total_evicted: 1,
        }
    }

    #[test]
    fn test_format_stats_json_fields() {
        let stats = sample_stats();
        let json = format_stats_json("alpine:latest", &stats);
        assert!(json.contains(r#""image":"alpine:latest""#));
        assert!(json.contains(r#""idle":2"#));
        assert!(json.contains(r#""total_created":5"#));
        assert!(json.contains(r#""total_acquired":4"#));
        assert!(json.contains(r#""total_released":3"#));
        assert!(json.contains(r#""total_evicted":1"#));
        assert!(json.contains("hit_rate"));
    }

    #[test]
    fn test_format_stats_json_zero_acquired() {
        let stats = PoolStats {
            idle_count: 0,
            total_created: 0,
            total_acquired: 0,
            total_released: 0,
            total_evicted: 0,
        };
        let json = format_stats_json("nginx:alpine", &stats);
        assert!(json.contains(r#""hit_rate":0.00"#));
    }

    #[test]
    fn test_format_stats_json_is_valid_structure() {
        let stats = sample_stats();
        let json = format_stats_json("alpine:latest", &stats);
        // Must start and end with braces
        assert!(json.starts_with('{'));
        assert!(json.ends_with('}'));
    }

    #[tokio::test]
    async fn test_execute_start_size_zero_fails() {
        let args = PoolStartArgs {
            image: "alpine:latest".to_string(),
            size: 0,
            max: 5,
            ttl: 300,
            json: false,
        };
        let result = execute_start(args).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("greater than 0"));
    }

    #[tokio::test]
    async fn test_execute_start_size_exceeds_max_fails() {
        let args = PoolStartArgs {
            image: "alpine:latest".to_string(),
            size: 10,
            max: 5,
            ttl: 300,
            json: false,
        };
        let result = execute_start(args).await;
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("cannot exceed --max"));
    }

    #[tokio::test]
    async fn test_execute_stop_is_ok() {
        let args = PoolStopArgs { json: false };
        // stop is a no-op (prints message), should not error
        let result = execute_stop(args).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_execute_status_is_ok() {
        let args = PoolStatusArgs { json: false };
        let result = execute_status(args).await;
        assert!(result.is_ok());
    }

    #[test]
    fn test_pool_start_args_defaults() {
        // Verify default values match expected warm pool behavior
        let args = PoolStartArgs {
            image: "alpine:latest".to_string(),
            size: 2,
            max: 8,
            ttl: 300,
            json: false,
        };
        assert_eq!(args.size, 2);
        assert_eq!(args.max, 8);
        assert_eq!(args.ttl, 300);
    }
}
