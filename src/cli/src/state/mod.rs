//! State management for box instances.
//!
//! Persists box metadata to `~/.a3s/boxes.json` with atomic writes.
//! On every load, dead PIDs are reconciled to mark boxes as dead.

mod file;
pub(crate) mod policy;
#[cfg(test)]
mod tests;

pub use file::StateFile;
pub use policy::{generate_name, parse_restart_policy, validate_restart_policy};

use std::collections::HashMap;
use std::path::PathBuf;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Metadata record for a single box instance.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BoxRecord {
    /// Full UUID
    pub id: String,
    /// First 12 hex chars of the UUID (no dashes)
    pub short_id: String,
    /// User-assigned or auto-generated name
    pub name: String,
    /// OCI image reference
    pub image: String,
    /// "created" | "running" | "stopped" | "dead"
    pub status: String,
    /// Shim process PID (set when running)
    pub pid: Option<u32>,
    /// Number of vCPUs
    pub cpus: u32,
    /// Memory in MB
    pub memory_mb: u32,
    /// Volume mounts ("host:guest" pairs)
    pub volumes: Vec<String>,
    /// Environment variables
    pub env: HashMap<String, String>,
    /// Entrypoint override
    pub cmd: Vec<String>,
    /// Entrypoint override (if set via --entrypoint)
    #[serde(default)]
    pub entrypoint: Option<Vec<String>>,
    /// Box working directory (~/.a3s/boxes/<id>/)
    pub box_dir: PathBuf,
    /// Path to exec socket
    #[serde(default)]
    pub exec_socket_path: PathBuf,
    /// Path to console log
    pub console_log: PathBuf,
    /// Creation timestamp
    pub created_at: DateTime<Utc>,
    /// Start timestamp
    pub started_at: Option<DateTime<Utc>>,
    /// Whether to auto-remove on stop
    pub auto_remove: bool,
    /// Custom hostname for the box
    #[serde(default)]
    pub hostname: Option<String>,
    /// User to run as inside the box
    #[serde(default)]
    pub user: Option<String>,
    /// Working directory inside the box
    #[serde(default)]
    pub workdir: Option<String>,
    /// Restart policy: "no", "always", "on-failure", "unless-stopped"
    #[serde(default = "default_restart_policy")]
    pub restart_policy: String,
    /// Port mappings ("host_port:guest_port" pairs)
    #[serde(default)]
    pub port_map: Vec<String>,
    /// User-defined labels (key=value metadata)
    #[serde(default)]
    pub labels: HashMap<String, String>,
    /// Whether the box was explicitly stopped by the user (for "unless-stopped" policy)
    #[serde(default)]
    pub stopped_by_user: bool,
    /// Number of automatic restarts performed
    #[serde(default)]
    pub restart_count: u32,
    /// Maximum restart count for "on-failure:N" policy (0 = unlimited)
    #[serde(default)]
    pub max_restart_count: u32,
    /// Exit code from the last run (None if not yet captured)
    #[serde(default)]
    pub exit_code: Option<i32>,
    /// Health check configuration
    #[serde(default)]
    pub health_check: Option<HealthCheck>,
    /// Current health status: "none", "starting", "healthy", "unhealthy"
    #[serde(default = "default_health_status")]
    pub health_status: String,
    /// Consecutive health check failures
    #[serde(default)]
    pub health_retries: u32,
    /// Timestamp of last health check
    #[serde(default)]
    pub health_last_check: Option<DateTime<Utc>>,
    /// Network mode for this box
    #[serde(default)]
    pub network_mode: a3s_box_core::NetworkMode,
    /// Network name (if connected to a bridge network)
    #[serde(default)]
    pub network_name: Option<String>,
    /// Named volumes attached to this box
    #[serde(default)]
    pub volume_names: Vec<String>,
    /// tmpfs mounts for this box
    #[serde(default)]
    pub tmpfs: Vec<String>,
    /// Anonymous volumes auto-created from OCI VOLUME directives
    #[serde(default)]
    pub anonymous_volumes: Vec<String>,
    /// Resource limits (PID limits, CPU pinning, ulimits, cgroup controls)
    #[serde(default)]
    pub resource_limits: a3s_box_core::config::ResourceLimits,
    /// Logging configuration
    #[serde(default)]
    pub log_config: a3s_box_core::log::LogConfig,
    /// Custom host-to-IP mappings (host:ip)
    #[serde(default)]
    pub add_host: Vec<String>,
    /// Target platform (e.g., "linux/amd64")
    #[serde(default)]
    pub platform: Option<String>,
    /// Use init process as PID 1
    #[serde(default)]
    pub init: bool,
    /// Read-only root filesystem
    #[serde(default)]
    pub read_only: bool,
    /// Added Linux capabilities
    #[serde(default)]
    pub cap_add: Vec<String>,
    /// Dropped Linux capabilities
    #[serde(default)]
    pub cap_drop: Vec<String>,
    /// Security options
    #[serde(default)]
    pub security_opt: Vec<String>,
    /// Extended privileges
    #[serde(default)]
    pub privileged: bool,
    /// Device mappings (host_path:guest_path:perms)
    #[serde(default)]
    pub devices: Vec<String>,
    /// GPU devices
    #[serde(default)]
    pub gpus: Option<String>,
    /// Shared memory size in bytes
    #[serde(default)]
    pub shm_size: Option<u64>,
    /// Signal to stop the box
    #[serde(default)]
    pub stop_signal: Option<String>,
    /// Timeout to stop the box before killing
    #[serde(default)]
    pub stop_timeout: Option<u64>,
    /// OOM killer disabled
    #[serde(default)]
    pub oom_kill_disable: bool,
    /// OOM score adjustment
    #[serde(default)]
    pub oom_score_adj: Option<i32>,
}

fn default_health_status() -> String {
    "none".to_string()
}

fn default_restart_policy() -> String {
    "no".to_string()
}

/// Health check configuration for a box.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HealthCheck {
    /// Command to run for health check (via exec channel)
    pub cmd: Vec<String>,
    /// Check interval in seconds (default: 30)
    #[serde(default = "default_health_interval")]
    pub interval_secs: u64,
    /// Per-check timeout in seconds (default: 5)
    #[serde(default = "default_health_timeout")]
    pub timeout_secs: u64,
    /// Consecutive failures before marking unhealthy (default: 3)
    #[serde(default = "default_health_retries")]
    pub retries: u32,
    /// Grace period after start before checks begin, in seconds (default: 0)
    #[serde(default)]
    pub start_period_secs: u64,
}

fn default_health_interval() -> u64 {
    30
}

fn default_health_timeout() -> u64 {
    5
}

fn default_health_retries() -> u32 {
    3
}

impl BoxRecord {
    /// Generate a short ID from a full UUID (first 12 hex characters, no dashes).
    pub fn make_short_id(id: &str) -> String {
        id.replace('-', "").chars().take(12).collect()
    }
}
