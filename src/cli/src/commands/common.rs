//! Shared CLI helpers for box creation commands.

use std::collections::HashMap;

use a3s_box_core::config::ResourceLimits;
use clap::Args;

/// Common arguments shared between `run` and `create` commands.
#[derive(Args)]
pub struct CommonBoxArgs {
    /// OCI image reference
    pub image: String,

    /// Assign a name to the box
    #[arg(long)]
    pub name: Option<String>,

    /// Number of CPUs
    #[arg(long, default_value = "2")]
    pub cpus: u32,

    /// Memory (e.g., "512m", "2g")
    #[arg(long, default_value = "512m")]
    pub memory: String,

    /// Volume mount (host:guest), can be repeated
    #[arg(short = 'v', long = "volume")]
    pub volumes: Vec<String>,

    /// Environment variable (KEY=VALUE), can be repeated
    #[arg(short = 'e', long = "env")]
    pub env: Vec<String>,

    /// Publish a port (host_port:guest_port), can be repeated
    #[arg(short = 'p', long = "publish")]
    pub publish: Vec<String>,

    /// Set custom DNS servers, can be repeated
    #[arg(long)]
    pub dns: Vec<String>,

    /// Override the image entrypoint
    #[arg(long)]
    pub entrypoint: Option<String>,

    /// Set the box hostname
    #[arg(long)]
    pub hostname: Option<String>,

    /// Run as a specific user (e.g., "root", "1000:1000")
    #[arg(short = 'u', long)]
    pub user: Option<String>,

    /// Working directory inside the box
    #[arg(short = 'w', long)]
    pub workdir: Option<String>,

    /// Restart policy: no, always, on-failure, unless-stopped
    #[arg(long, default_value = "no")]
    pub restart: String,

    /// Set metadata labels (KEY=VALUE), can be repeated
    #[arg(short = 'l', long = "label")]
    pub labels: Vec<String>,

    /// Mount a tmpfs (e.g., "/tmp" or "/tmp:size=100m"), can be repeated
    #[arg(long)]
    pub tmpfs: Vec<String>,

    /// Connect to a network (e.g., "mynet")
    #[arg(long)]
    pub network: Option<String>,

    /// Health check command (e.g., "curl -f http://localhost/health")
    #[arg(long)]
    pub health_cmd: Option<String>,

    /// Health check interval in seconds (default: 30)
    #[arg(long, default_value = "30")]
    pub health_interval: u64,

    /// Health check timeout in seconds (default: 5)
    #[arg(long, default_value = "5")]
    pub health_timeout: u64,

    /// Health check retries before unhealthy (default: 3)
    #[arg(long, default_value = "3")]
    pub health_retries: u32,

    /// Health check start period in seconds (default: 0)
    #[arg(long, default_value = "0")]
    pub health_start_period: u64,

    /// Limit PIDs inside the box (--pids-limit)
    #[arg(long)]
    pub pids_limit: Option<u64>,

    /// Pin to specific CPUs (e.g., "0,1,3" or "0-3")
    #[arg(long)]
    pub cpuset_cpus: Option<String>,

    /// Set ulimit (e.g., "nofile=1024:4096"), can be repeated
    #[arg(long = "ulimit")]
    pub ulimits: Vec<String>,

    /// CPU shares (relative weight, 2-262144)
    #[arg(long)]
    pub cpu_shares: Option<u64>,

    /// CPU quota in microseconds per cpu-period
    #[arg(long)]
    pub cpu_quota: Option<i64>,

    /// CPU period in microseconds (default: 100000)
    #[arg(long)]
    pub cpu_period: Option<u64>,

    /// Memory reservation/soft limit (e.g., "256m", "1g")
    #[arg(long)]
    pub memory_reservation: Option<String>,

    /// Memory+swap limit (e.g., "1g", "-1" for unlimited)
    #[arg(long)]
    pub memory_swap: Option<String>,

    /// Read environment variables from a file, can be repeated
    #[arg(long)]
    pub env_file: Vec<String>,

    /// Add a custom host-to-IP mapping (host:ip), can be repeated
    #[arg(long)]
    pub add_host: Vec<String>,

    /// Set target platform (e.g., "linux/amd64", "linux/arm64")
    #[arg(long)]
    pub platform: Option<String>,

    /// Run an init process (tini) as PID 1
    #[arg(long)]
    pub init: bool,

    /// Mount the root filesystem as read-only
    #[arg(long)]
    pub read_only: bool,

    /// Add a Linux capability, can be repeated
    #[arg(long)]
    pub cap_add: Vec<String>,

    /// Drop a Linux capability, can be repeated
    #[arg(long)]
    pub cap_drop: Vec<String>,

    /// Security options (e.g., "seccomp=unconfined"), can be repeated
    #[arg(long)]
    pub security_opt: Vec<String>,

    /// Give extended privileges to the box
    #[arg(long)]
    pub privileged: bool,

    /// Add a host device to the box (host_path[:guest_path[:perms]]), can be repeated
    #[arg(long)]
    pub device: Vec<String>,

    /// GPU devices to add (e.g., "all", "0,1")
    #[arg(long)]
    pub gpus: Option<String>,

    /// Size of /dev/shm (e.g., "64m", "1g")
    #[arg(long)]
    pub shm_size: Option<String>,

    /// Override the default signal to stop the box
    #[arg(long)]
    pub stop_signal: Option<String>,

    /// Timeout (in seconds) to stop the box before killing
    #[arg(long)]
    pub stop_timeout: Option<u64>,

    /// Disable any healthcheck defined in the image
    #[arg(long)]
    pub no_healthcheck: bool,

    /// Disable OOM Killer for the box
    #[arg(long)]
    pub oom_kill_disable: bool,

    /// Tune the host OOM score adjustment (-1000 to 1000)
    #[arg(long)]
    pub oom_score_adj: Option<i32>,
}

/// Parse KEY=VALUE pairs into a HashMap.
pub(crate) fn parse_env_vars(vars: &[String]) -> Result<HashMap<String, String>, String> {
    let mut map = HashMap::new();
    for var in vars {
        let (key, value) = var
            .split_once('=')
            .ok_or_else(|| format!("Invalid environment variable (expected KEY=VALUE): {var}"))?;
        map.insert(key.to_string(), value.to_string());
    }
    Ok(map)
}

/// Load environment variables from a file.
///
/// Each line should be KEY=VALUE. Empty lines and lines starting with '#' are skipped.
pub(crate) fn parse_env_file(
    path: &str,
) -> Result<HashMap<String, String>, Box<dyn std::error::Error>> {
    let content = std::fs::read_to_string(path)
        .map_err(|e| format!("Failed to read env file '{}': {}", path, e))?;
    let mut map = HashMap::new();
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        if let Some((key, value)) = trimmed.split_once('=') {
            map.insert(key.trim().to_string(), value.trim().to_string());
        } else {
            // KEY without value — set to empty string (Docker behavior)
            map.insert(trimmed.to_string(), String::new());
        }
    }
    Ok(map)
}

/// Parse a memory size string (e.g., "256m", "1g", "1073741824") into bytes.
pub(crate) fn parse_memory_bytes(s: &str) -> Result<u64, String> {
    let s = s.trim().to_lowercase();
    if s.is_empty() {
        return Err("empty value".to_string());
    }

    if let Ok(bytes) = s.parse::<u64>() {
        return Ok(bytes);
    }

    let (num_str, multiplier) = if s.ends_with("gb") || s.ends_with("g") {
        let num = s.trim_end_matches("gb").trim_end_matches('g');
        (num, 1024u64 * 1024 * 1024)
    } else if s.ends_with("mb") || s.ends_with("m") {
        let num = s.trim_end_matches("mb").trim_end_matches('m');
        (num, 1024u64 * 1024)
    } else if s.ends_with("kb") || s.ends_with("k") {
        let num = s.trim_end_matches("kb").trim_end_matches('k');
        (num, 1024u64)
    } else if s.ends_with('b') {
        let num = s.trim_end_matches('b');
        (num, 1u64)
    } else {
        return Err(format!("unrecognized memory format: {s}"));
    };

    let num: u64 = num_str
        .parse()
        .map_err(|_| format!("invalid number: {num_str}"))?;
    Ok(num * multiplier)
}

/// Build ResourceLimits from common box args.
pub(crate) fn build_resource_limits(
    args: &CommonBoxArgs,
) -> Result<ResourceLimits, Box<dyn std::error::Error>> {
    let memory_reservation = match &args.memory_reservation {
        Some(s) => {
            Some(parse_memory_bytes(s).map_err(|e| format!("Invalid --memory-reservation: {e}"))?)
        }
        None => None,
    };
    let memory_swap = match &args.memory_swap {
        Some(s) if s == "-1" => Some(-1i64),
        Some(s) => {
            Some(parse_memory_bytes(s).map_err(|e| format!("Invalid --memory-swap: {e}"))? as i64)
        }
        None => None,
    };

    Ok(ResourceLimits {
        pids_limit: args.pids_limit,
        cpuset_cpus: args.cpuset_cpus.clone(),
        ulimits: args.ulimits.clone(),
        cpu_shares: args.cpu_shares,
        cpu_quota: args.cpu_quota,
        cpu_period: args.cpu_period,
        memory_reservation,
        memory_swap,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- parse_env_vars tests ---

    #[test]
    fn test_parse_env_vars_valid() {
        let vars = vec!["FOO=bar".to_string(), "BAZ=qux".to_string()];
        let map = parse_env_vars(&vars).unwrap();
        assert_eq!(map.get("FOO").unwrap(), "bar");
        assert_eq!(map.get("BAZ").unwrap(), "qux");
    }

    #[test]
    fn test_parse_env_vars_empty() {
        let map = parse_env_vars(&[]).unwrap();
        assert!(map.is_empty());
    }

    #[test]
    fn test_parse_env_vars_value_with_equals() {
        let vars = vec!["KEY=val=ue".to_string()];
        let map = parse_env_vars(&vars).unwrap();
        assert_eq!(map.get("KEY").unwrap(), "val=ue");
    }

    #[test]
    fn test_parse_env_vars_invalid_no_equals() {
        let vars = vec!["INVALID".to_string()];
        let result = parse_env_vars(&vars);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("KEY=VALUE"));
    }

    #[test]
    fn test_parse_env_vars_empty_value() {
        let vars = vec!["KEY=".to_string()];
        let map = parse_env_vars(&vars).unwrap();
        assert_eq!(map.get("KEY").unwrap(), "");
    }

    // --- parse_memory_bytes tests ---

    #[test]
    fn test_parse_memory_bytes_raw_number() {
        assert_eq!(parse_memory_bytes("1073741824").unwrap(), 1073741824);
        assert_eq!(parse_memory_bytes("512").unwrap(), 512);
    }

    #[test]
    fn test_parse_memory_bytes_kilobytes() {
        assert_eq!(parse_memory_bytes("1k").unwrap(), 1024);
        assert_eq!(parse_memory_bytes("100k").unwrap(), 100 * 1024);
        assert_eq!(parse_memory_bytes("512kb").unwrap(), 512 * 1024);
        assert_eq!(parse_memory_bytes("2K").unwrap(), 2048);
    }

    #[test]
    fn test_parse_memory_bytes_megabytes() {
        assert_eq!(parse_memory_bytes("1m").unwrap(), 1024 * 1024);
        assert_eq!(parse_memory_bytes("512m").unwrap(), 512 * 1024 * 1024);
        assert_eq!(parse_memory_bytes("512mb").unwrap(), 512 * 1024 * 1024);
        assert_eq!(parse_memory_bytes("512M").unwrap(), 512 * 1024 * 1024);
        assert_eq!(parse_memory_bytes("2M").unwrap(), 2 * 1024 * 1024);
    }

    #[test]
    fn test_parse_memory_bytes_gigabytes() {
        assert_eq!(parse_memory_bytes("1g").unwrap(), 1024 * 1024 * 1024);
        assert_eq!(parse_memory_bytes("2g").unwrap(), 2 * 1024 * 1024 * 1024);
        assert_eq!(parse_memory_bytes("2gb").unwrap(), 2 * 1024 * 1024 * 1024);
        assert_eq!(parse_memory_bytes("4G").unwrap(), 4 * 1024 * 1024 * 1024);
    }

    #[test]
    fn test_parse_memory_bytes_bytes_suffix() {
        assert_eq!(parse_memory_bytes("4096b").unwrap(), 4096);
        assert_eq!(parse_memory_bytes("1024b").unwrap(), 1024);
        assert_eq!(parse_memory_bytes("512B").unwrap(), 512);
    }

    #[test]
    fn test_parse_memory_bytes_empty() {
        assert!(parse_memory_bytes("").is_err());
        assert!(parse_memory_bytes("   ").is_err());
    }

    #[test]
    fn test_parse_memory_bytes_invalid_format() {
        assert!(parse_memory_bytes("abc").is_err());
        assert!(parse_memory_bytes("12x").is_err());
        assert!(parse_memory_bytes("123x").is_err());
        assert!(parse_memory_bytes("m512").is_err());
    }

    #[test]
    fn test_parse_memory_bytes_invalid_number() {
        assert!(parse_memory_bytes("abcm").is_err());
        assert!(parse_memory_bytes("12.5g").is_err());
    }

    #[test]
    fn test_parse_memory_bytes_whitespace() {
        assert_eq!(parse_memory_bytes("  512m  ").unwrap(), 512 * 1024 * 1024);
        assert_eq!(parse_memory_bytes("\t1g\n").unwrap(), 1024 * 1024 * 1024);
    }

    // --- parse_env_file tests ---

    #[test]
    fn test_parse_env_file_basic() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("env");
        std::fs::write(&path, "FOO=bar\nBAZ=qux\n").unwrap();
        let map = parse_env_file(path.to_str().unwrap()).unwrap();
        assert_eq!(map.get("FOO").unwrap(), "bar");
        assert_eq!(map.get("BAZ").unwrap(), "qux");
    }

    #[test]
    fn test_parse_env_file_comments_and_blanks() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("env");
        std::fs::write(&path, "# comment\n\nKEY=val\n  \n# another\n").unwrap();
        let map = parse_env_file(path.to_str().unwrap()).unwrap();
        assert_eq!(map.len(), 1);
        assert_eq!(map.get("KEY").unwrap(), "val");
    }

    #[test]
    fn test_parse_env_file_key_without_value() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("env");
        std::fs::write(&path, "STANDALONE\n").unwrap();
        let map = parse_env_file(path.to_str().unwrap()).unwrap();
        assert_eq!(map.get("STANDALONE").unwrap(), "");
    }

    #[test]
    fn test_parse_env_file_value_with_equals() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("env");
        std::fs::write(&path, "CONN=postgres://host?opt=1\n").unwrap();
        let map = parse_env_file(path.to_str().unwrap()).unwrap();
        assert_eq!(map.get("CONN").unwrap(), "postgres://host?opt=1");
    }

    #[test]
    fn test_parse_env_file_missing_file() {
        let result = parse_env_file("/nonexistent/path/env");
        assert!(result.is_err());
    }

    #[test]
    fn test_parse_env_file_empty() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("env");
        std::fs::write(&path, "").unwrap();
        let map = parse_env_file(path.to_str().unwrap()).unwrap();
        assert!(map.is_empty());
    }

    // --- build_resource_limits tests ---

    /// Helper to create a CommonBoxArgs with defaults for testing.
    fn default_common_args() -> CommonBoxArgs {
        CommonBoxArgs {
            image: "test".to_string(),
            name: None,
            cpus: 2,
            memory: "512m".to_string(),
            volumes: vec![],
            env: vec![],
            publish: vec![],
            dns: vec![],
            entrypoint: None,
            hostname: None,
            user: None,
            workdir: None,
            restart: "no".to_string(),
            labels: vec![],
            tmpfs: vec![],
            network: None,
            health_cmd: None,
            health_interval: 30,
            health_timeout: 5,
            health_retries: 3,
            health_start_period: 0,
            pids_limit: None,
            cpuset_cpus: None,
            ulimits: vec![],
            cpu_shares: None,
            cpu_quota: None,
            cpu_period: None,
            memory_reservation: None,
            memory_swap: None,
            env_file: vec![],
            add_host: vec![],
            platform: None,
            init: false,
            read_only: false,
            cap_add: vec![],
            cap_drop: vec![],
            security_opt: vec![],
            privileged: false,
            device: vec![],
            gpus: None,
            shm_size: None,
            stop_signal: None,
            stop_timeout: None,
            no_healthcheck: false,
            oom_kill_disable: false,
            oom_score_adj: None,
        }
    }

    #[test]
    fn test_build_resource_limits_defaults() {
        let args = default_common_args();
        let limits = build_resource_limits(&args).unwrap();
        assert!(limits.pids_limit.is_none());
        assert!(limits.cpuset_cpus.is_none());
        assert!(limits.cpu_shares.is_none());
        assert!(limits.memory_reservation.is_none());
        assert!(limits.memory_swap.is_none());
    }

    #[test]
    fn test_build_resource_limits_with_values() {
        let mut args = default_common_args();
        args.pids_limit = Some(100);
        args.cpuset_cpus = Some("0-3".to_string());
        args.ulimits = vec!["nofile=1024:4096".to_string()];
        args.cpu_shares = Some(512);
        args.cpu_quota = Some(50000);
        args.cpu_period = Some(100000);
        args.memory_reservation = Some("256m".to_string());
        args.memory_swap = Some("-1".to_string());

        let limits = build_resource_limits(&args).unwrap();
        assert_eq!(limits.pids_limit, Some(100));
        assert_eq!(limits.cpuset_cpus, Some("0-3".to_string()));
        assert_eq!(limits.cpu_shares, Some(512));
        assert_eq!(limits.cpu_quota, Some(50000));
        assert_eq!(limits.cpu_period, Some(100000));
        assert_eq!(limits.memory_reservation, Some(256 * 1024 * 1024));
        assert_eq!(limits.memory_swap, Some(-1));
    }

    #[test]
    fn test_build_resource_limits_memory_swap_value() {
        let mut args = default_common_args();
        args.memory_swap = Some("1g".to_string());

        let limits = build_resource_limits(&args).unwrap();
        assert_eq!(limits.memory_swap, Some(1024 * 1024 * 1024));
    }
}
