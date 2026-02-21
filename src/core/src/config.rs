use crate::network::NetworkMode;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// TEE (Trusted Execution Environment) configuration.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TeeConfig {
    /// No TEE (standard VM)
    #[default]
    None,

    /// AMD SEV-SNP (Secure Encrypted Virtualization - Secure Nested Paging)
    SevSnp {
        /// Workload identifier for attestation
        workload_id: String,
        /// CPU generation: "milan" or "genoa"
        #[serde(default)]
        generation: SevSnpGeneration,
        /// Enable simulation mode (no hardware required, for development)
        #[serde(default)]
        simulate: bool,
    },

    /// Intel TDX (Trust Domain Extensions) — stub, not yet implemented at runtime.
    Tdx {
        /// Workload identifier for attestation
        workload_id: String,
        /// Enable simulation mode (no hardware required, for development)
        #[serde(default)]
        simulate: bool,
    },
}

/// AMD SEV-SNP CPU generation.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum SevSnpGeneration {
    /// AMD EPYC Milan (3rd gen)
    #[default]
    Milan,
    /// AMD EPYC Genoa (4th gen)
    Genoa,
}

impl SevSnpGeneration {
    /// Get the generation as a string for TEE config.
    pub fn as_str(&self) -> &'static str {
        match self {
            SevSnpGeneration::Milan => "milan",
            SevSnpGeneration::Genoa => "genoa",
        }
    }
}

/// Cache configuration for cold start optimization.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CacheConfig {
    /// Enable rootfs and layer caching (default: true)
    #[serde(default = "default_true")]
    pub enabled: bool,

    /// Cache directory (default: ~/.a3s/cache)
    pub cache_dir: Option<PathBuf>,

    /// Maximum number of cached rootfs entries (default: 10)
    #[serde(default = "default_max_rootfs_entries")]
    pub max_rootfs_entries: usize,

    /// Maximum total cache size in bytes (default: 10 GB)
    #[serde(default = "default_max_cache_bytes")]
    pub max_cache_bytes: u64,
}

fn default_true() -> bool {
    true
}

fn default_max_rootfs_entries() -> usize {
    10
}

fn default_max_cache_bytes() -> u64 {
    10 * 1024 * 1024 * 1024 // 10 GB
}

impl Default for CacheConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            cache_dir: None,
            max_rootfs_entries: 10,
            max_cache_bytes: 10 * 1024 * 1024 * 1024,
        }
    }
}

/// Warm pool configuration for pre-booted VMs.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PoolConfig {
    /// Enable warm pool (default: false)
    #[serde(default)]
    pub enabled: bool,

    /// Minimum number of pre-warmed idle VMs to maintain
    #[serde(default = "default_min_idle")]
    pub min_idle: usize,

    /// Maximum number of VMs in the pool (idle + in-use)
    #[serde(default = "default_max_pool_size")]
    pub max_size: usize,

    /// Time-to-live for idle VMs in seconds (0 = unlimited)
    #[serde(default = "default_idle_ttl")]
    pub idle_ttl_secs: u64,

    /// Autoscaling policy for dynamic min_idle adjustment
    #[serde(default)]
    pub scaling: ScalingPolicy,
}

/// Autoscaling policy for dynamic warm pool sizing.
///
/// When enabled, the pool monitors acquire hit/miss rates over a sliding
/// window and adjusts `min_idle` up or down to match demand pressure.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScalingPolicy {
    /// Enable autoscaling (default: false)
    #[serde(default)]
    pub enabled: bool,

    /// Miss rate threshold to trigger scale-up (default: 0.3 = 30%)
    #[serde(default = "default_scale_up_threshold")]
    pub scale_up_threshold: f64,

    /// Miss rate threshold to trigger scale-down (default: 0.05 = 5%)
    #[serde(default = "default_scale_down_threshold")]
    pub scale_down_threshold: f64,

    /// Upper bound for dynamic min_idle (default: 0 = use max_size)
    #[serde(default)]
    pub max_min_idle: usize,

    /// Seconds between scaling decisions (default: 60)
    #[serde(default = "default_cooldown_secs")]
    pub cooldown_secs: u64,

    /// Observation window for miss rate calculation in seconds (default: 120)
    #[serde(default = "default_window_secs")]
    pub window_secs: u64,
}

fn default_scale_up_threshold() -> f64 {
    0.3
}

fn default_scale_down_threshold() -> f64 {
    0.05
}

fn default_cooldown_secs() -> u64 {
    60
}

fn default_window_secs() -> u64 {
    120
}

impl Default for ScalingPolicy {
    fn default() -> Self {
        Self {
            enabled: false,
            scale_up_threshold: 0.3,
            scale_down_threshold: 0.05,
            max_min_idle: 0,
            cooldown_secs: 60,
            window_secs: 120,
        }
    }
}

fn default_min_idle() -> usize {
    1
}

fn default_max_pool_size() -> usize {
    5
}

fn default_idle_ttl() -> u64 {
    300 // 5 minutes
}

impl Default for PoolConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            min_idle: 1,
            max_size: 5,
            idle_ttl_secs: 300,
            scaling: ScalingPolicy::default(),
        }
    }
}

/// Resource limits for a box instance.
///
/// Tier 1 limits (rlimits, cpuset) work on all platforms.
/// Tier 2 limits (cgroup-based) are Linux-only and best-effort.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ResourceLimits {
    /// PID limit inside the guest (--pids-limit).
    /// Maps to RLIMIT_NPROC in guest rlimits.
    #[serde(default)]
    pub pids_limit: Option<u64>,

    /// CPU pinning: comma-separated CPU IDs (--cpuset-cpus "0,1,3").
    /// Applied via sched_setaffinity() on the shim process (Linux only).
    #[serde(default)]
    pub cpuset_cpus: Option<String>,

    /// Custom rlimits (--ulimit), format: "RESOURCE=SOFT:HARD".
    #[serde(default)]
    pub ulimits: Vec<String>,

    /// CPU shares (--cpu-shares), relative weight 2-262144.
    /// Applied via cgroup v2 cpu.weight (Linux only).
    #[serde(default)]
    pub cpu_shares: Option<u64>,

    /// CPU quota in microseconds per --cpu-period (--cpu-quota).
    /// Applied via cgroup v2 cpu.max (Linux only).
    #[serde(default)]
    pub cpu_quota: Option<i64>,

    /// CPU period in microseconds (--cpu-period, default 100000).
    /// Applied via cgroup v2 cpu.max (Linux only).
    #[serde(default)]
    pub cpu_period: Option<u64>,

    /// Memory reservation/soft limit in bytes (--memory-reservation).
    /// Applied via cgroup v2 memory.low (Linux only).
    #[serde(default)]
    pub memory_reservation: Option<u64>,

    /// Memory+swap limit in bytes (--memory-swap, -1 = unlimited).
    /// Applied via cgroup v2 memory.swap.max (Linux only).
    #[serde(default)]
    pub memory_swap: Option<i64>,
}

/// Box configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BoxConfig {
    /// OCI image reference (e.g., "nginx:alpine", "ghcr.io/org/app:latest")
    #[serde(default)]
    pub image: String,

    /// Workspace directory (mounted to /workspace inside the VM)
    pub workspace: PathBuf,

    /// Resource limits
    pub resources: ResourceConfig,

    /// Log level
    pub log_level: LogLevel,

    /// Enable gRPC debug logging
    pub debug_grpc: bool,

    /// TEE (Trusted Execution Environment) configuration
    #[serde(default)]
    pub tee: TeeConfig,

    /// Command override (replaces OCI CMD when set)
    #[serde(default)]
    pub cmd: Vec<String>,

    /// Entrypoint override (replaces OCI ENTRYPOINT when set)
    #[serde(default)]
    pub entrypoint_override: Option<Vec<String>>,

    /// Extra volume mounts (host_path:guest_path or host_path:guest_path:ro)
    #[serde(default)]
    pub volumes: Vec<String>,

    /// Extra environment variables for the entrypoint
    #[serde(default)]
    pub extra_env: Vec<(String, String)>,

    /// Cache configuration for cold start optimization
    #[serde(default)]
    pub cache: CacheConfig,

    /// Warm pool configuration for pre-booted VMs
    #[serde(default)]
    pub pool: PoolConfig,

    /// Port mappings: "host_port:guest_port" (e.g., "8080:80")
    /// Maps host ports to guest ports via TSI (Transparent Socket Impersonation).
    #[serde(default)]
    pub port_map: Vec<String>,

    /// Custom DNS servers (e.g., "1.1.1.1").
    /// If empty, reads from host /etc/resolv.conf, falling back to 8.8.8.8.
    #[serde(default)]
    pub dns: Vec<String>,

    /// Network mode: TSI (default), bridge (passt-based), or none.
    #[serde(default)]
    pub network: NetworkMode,

    /// tmpfs mounts (ephemeral in-guest filesystems).
    /// Format: "/path" or "/path:size=100m"
    #[serde(default)]
    pub tmpfs: Vec<String>,

    /// Resource limits (PID limits, CPU pinning, ulimits, cgroup controls).
    #[serde(default)]
    pub resource_limits: ResourceLimits,

    /// Linux capabilities to add (e.g., "NET_ADMIN", "SYS_PTRACE")
    #[serde(default)]
    pub cap_add: Vec<String>,

    /// Linux capabilities to drop (e.g., "ALL", "NET_RAW")
    #[serde(default)]
    pub cap_drop: Vec<String>,

    /// Security options (e.g., "seccomp=unconfined", "no-new-privileges")
    #[serde(default)]
    pub security_opt: Vec<String>,

    /// Run in privileged mode (disables all security restrictions)
    #[serde(default)]
    pub privileged: bool,

    /// Mount the container rootfs as read-only.
    ///
    /// Volume mounts (-v host:guest) remain writable by default.
    /// Requires guest init to be present in the rootfs image.
    #[serde(default)]
    pub read_only: bool,

    /// Optional sidecar process to run alongside the main container inside the VM.
    ///
    /// The sidecar is launched before the main container entrypoint and runs
    /// as a co-process inside the same MicroVM. Intended for security proxies
    /// such as SafeClaw that intercept and classify agent traffic.
    #[serde(default)]
    pub sidecar: Option<SidecarConfig>,
}

impl Default for BoxConfig {
    fn default() -> Self {
        Self {
            image: String::new(),
            // Empty path signals the runtime to create a per-box workspace
            // under ~/.a3s/boxes/<box_id>/workspace/ at boot time.
            workspace: PathBuf::new(),
            resources: ResourceConfig::default(),
            log_level: LogLevel::Info,
            debug_grpc: false,
            tee: TeeConfig::default(),
            cmd: vec![],
            entrypoint_override: None,
            volumes: vec![],
            extra_env: vec![],
            cache: CacheConfig::default(),
            pool: PoolConfig::default(),
            port_map: vec![],
            dns: vec![],
            network: NetworkMode::default(),
            tmpfs: vec![],
            resource_limits: ResourceLimits::default(),
            cap_add: vec![],
            cap_drop: vec![],
            security_opt: vec![],
            privileged: false,
            read_only: false,
            sidecar: None,
        }
    }
}

/// Sidecar process configuration.
///
/// A sidecar runs as a co-process inside the same MicroVM alongside the main
/// container. It is launched before the main entrypoint and communicates with
/// the host via a dedicated vsock port.
///
/// Primary use case: SafeClaw security proxy that intercepts and classifies
/// agent traffic before it reaches the LLM.
///
/// # Data flow
///
/// ```text
/// Agent → SafeClaw (vsock 4092) → classified/sanitized → LLM
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SidecarConfig {
    /// OCI image reference for the sidecar (e.g., "ghcr.io/a3s-lab/safeclaw:latest")
    pub image: String,

    /// Vsock port the sidecar listens on for host-side control (default: 4092)
    #[serde(default = "default_sidecar_vsock_port")]
    pub vsock_port: u32,

    /// Extra environment variables for the sidecar process
    #[serde(default)]
    pub env: Vec<(String, String)>,
}

fn default_sidecar_vsock_port() -> u32 {
    4092
}

impl Default for SidecarConfig {
    fn default() -> Self {
        Self {
            image: String::new(),
            vsock_port: default_sidecar_vsock_port(),
            env: vec![],
        }
    }
}

/// Resource configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResourceConfig {
    /// Number of virtual CPUs
    pub vcpus: u32,

    /// Memory in MB
    pub memory_mb: u32,

    /// Disk space in MB
    pub disk_mb: u32,

    /// Box lifetime timeout in seconds (0 = unlimited)
    pub timeout: u64,
}

impl Default for ResourceConfig {
    fn default() -> Self {
        Self {
            vcpus: 2,
            memory_mb: 1024,
            disk_mb: 4096,
            timeout: 3600, // 1 hour
        }
    }
}

/// Log level
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub enum LogLevel {
    Debug,
    Info,
    Warn,
    Error,
}

impl From<LogLevel> for tracing::Level {
    fn from(level: LogLevel) -> Self {
        match level {
            LogLevel::Debug => tracing::Level::DEBUG,
            LogLevel::Info => tracing::Level::INFO,
            LogLevel::Warn => tracing::Level::WARN,
            LogLevel::Error => tracing::Level::ERROR,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_box_config_default() {
        let config = BoxConfig::default();

        assert!(config.image.is_empty());
        // Empty workspace signals the runtime to use a per-box directory at boot time.
        assert!(config.workspace.as_os_str().is_empty());
        assert_eq!(config.resources.vcpus, 2);
        assert!(!config.debug_grpc);
        assert!(!config.read_only);
    }

    #[test]
    fn test_box_config_read_only_default_false() {
        let config = BoxConfig::default();
        assert!(!config.read_only);
    }

    #[test]
    fn test_box_config_read_only_serde() {
        // read_only defaults to false when absent from JSON
        let json = r#"{"image":"test","workspace":"","resources":{"vcpus":2,"memory_mb":512,"disk_mb":4096,"timeout":3600},"log_level":"Info","debug_grpc":false}"#;
        let config: BoxConfig = serde_json::from_str(json).unwrap();
        assert!(!config.read_only);

        // read_only=true roundtrips correctly
        let config = BoxConfig {
            read_only: true,
            ..Default::default()
        };
        let json = serde_json::to_string(&config).unwrap();
        let deserialized: BoxConfig = serde_json::from_str(&json).unwrap();
        assert!(deserialized.read_only);
    }

    #[test]
    fn test_resource_config_default() {
        let config = ResourceConfig::default();

        assert_eq!(config.vcpus, 2);
        assert_eq!(config.memory_mb, 1024);
        assert_eq!(config.disk_mb, 4096);
        assert_eq!(config.timeout, 3600);
    }

    #[test]
    fn test_resource_config_custom() {
        let config = ResourceConfig {
            vcpus: 4,
            memory_mb: 2048,
            disk_mb: 8192,
            timeout: 7200,
        };

        assert_eq!(config.vcpus, 4);
        assert_eq!(config.memory_mb, 2048);
        assert_eq!(config.disk_mb, 8192);
        assert_eq!(config.timeout, 7200);
    }

    #[test]
    fn test_log_level_conversion() {
        assert_eq!(tracing::Level::from(LogLevel::Debug), tracing::Level::DEBUG);
        assert_eq!(tracing::Level::from(LogLevel::Info), tracing::Level::INFO);
        assert_eq!(tracing::Level::from(LogLevel::Warn), tracing::Level::WARN);
        assert_eq!(tracing::Level::from(LogLevel::Error), tracing::Level::ERROR);
    }

    #[test]
    fn test_box_config_serialization() {
        let config = BoxConfig::default();
        let json = serde_json::to_string(&config).unwrap();

        assert!(json.contains("workspace"));
        assert!(json.contains("resources"));
    }

    #[test]
    fn test_box_config_deserialization() {
        let json = r#"{
            "image": "nginx:alpine",
            "workspace": "/tmp/workspace",
            "resources": {
                "vcpus": 4,
                "memory_mb": 2048,
                "disk_mb": 8192,
                "timeout": 1800
            },
            "log_level": "Debug",
            "debug_grpc": true
        }"#;

        let config: BoxConfig = serde_json::from_str(json).unwrap();
        assert_eq!(config.image, "nginx:alpine");
        assert_eq!(config.workspace.to_str().unwrap(), "/tmp/workspace");
        assert_eq!(config.resources.vcpus, 4);
        assert!(config.debug_grpc);
    }

    #[test]
    fn test_resource_config_serialization() {
        let config = ResourceConfig {
            vcpus: 8,
            memory_mb: 4096,
            disk_mb: 16384,
            timeout: 0,
        };

        let json = serde_json::to_string(&config).unwrap();
        let parsed: ResourceConfig = serde_json::from_str(&json).unwrap();

        assert_eq!(parsed.vcpus, 8);
        assert_eq!(parsed.memory_mb, 4096);
        assert_eq!(parsed.timeout, 0); // Unlimited
    }

    #[test]
    fn test_log_level_serialization() {
        let levels = vec![
            LogLevel::Debug,
            LogLevel::Info,
            LogLevel::Warn,
            LogLevel::Error,
        ];

        for level in levels {
            let json = serde_json::to_string(&level).unwrap();
            let parsed: LogLevel = serde_json::from_str(&json).unwrap();
            assert_eq!(tracing::Level::from(parsed), tracing::Level::from(level));
        }
    }

    #[test]
    fn test_config_clone() {
        let config = BoxConfig::default();
        let cloned = config.clone();

        assert_eq!(config.workspace, cloned.workspace);
        assert_eq!(config.resources.vcpus, cloned.resources.vcpus);
    }

    #[test]
    fn test_config_debug() {
        let config = BoxConfig::default();
        let debug_str = format!("{:?}", config);

        assert!(debug_str.contains("BoxConfig"));
        assert!(debug_str.contains("workspace"));
    }

    #[test]
    fn test_tee_config_default() {
        let tee = TeeConfig::default();
        assert_eq!(tee, TeeConfig::None);
    }

    #[test]
    fn test_tee_config_sev_snp() {
        let tee = TeeConfig::SevSnp {
            workload_id: "test-agent".to_string(),
            generation: SevSnpGeneration::Milan,
            simulate: false,
        };

        match tee {
            TeeConfig::SevSnp {
                workload_id,
                generation,
                simulate,
            } => {
                assert_eq!(workload_id, "test-agent");
                assert_eq!(generation, SevSnpGeneration::Milan);
                assert!(!simulate);
            }
            _ => panic!("Expected SevSnp variant"),
        }
    }

    #[test]
    fn test_sev_snp_generation_as_str() {
        assert_eq!(SevSnpGeneration::Milan.as_str(), "milan");
        assert_eq!(SevSnpGeneration::Genoa.as_str(), "genoa");
    }

    #[test]
    fn test_sev_snp_generation_default() {
        let gen = SevSnpGeneration::default();
        assert_eq!(gen, SevSnpGeneration::Milan);
    }

    #[test]
    fn test_tee_config_serialization() {
        let tee = TeeConfig::SevSnp {
            workload_id: "my-workload".to_string(),
            generation: SevSnpGeneration::Genoa,
            simulate: false,
        };

        let json = serde_json::to_string(&tee).unwrap();
        let parsed: TeeConfig = serde_json::from_str(&json).unwrap();

        assert_eq!(parsed, tee);
    }

    #[test]
    fn test_tee_config_none_serialization() {
        let tee = TeeConfig::None;
        let json = serde_json::to_string(&tee).unwrap();
        let parsed: TeeConfig = serde_json::from_str(&json).unwrap();

        assert_eq!(parsed, TeeConfig::None);
    }

    #[test]
    fn test_tee_config_tdx() {
        let tee = TeeConfig::Tdx {
            workload_id: "tdx-workload".to_string(),
            simulate: false,
        };
        let json = serde_json::to_string(&tee).unwrap();
        let parsed: TeeConfig = serde_json::from_str(&json).unwrap();
        match parsed {
            TeeConfig::Tdx {
                workload_id,
                simulate,
            } => {
                assert_eq!(workload_id, "tdx-workload");
                assert!(!simulate);
            }
            _ => panic!("Expected Tdx variant"),
        }
    }

    #[test]
    fn test_tee_config_tdx_simulate() {
        let tee = TeeConfig::Tdx {
            workload_id: "test".to_string(),
            simulate: true,
        };
        let json = serde_json::to_string(&tee).unwrap();
        let parsed: TeeConfig = serde_json::from_str(&json).unwrap();
        match parsed {
            TeeConfig::Tdx { simulate, .. } => assert!(simulate),
            _ => panic!("Expected Tdx variant"),
        }
    }

    #[test]
    fn test_box_config_with_tee() {
        let config = BoxConfig {
            tee: TeeConfig::SevSnp {
                workload_id: "secure-agent".to_string(),
                generation: SevSnpGeneration::Milan,
                simulate: false,
            },
            ..Default::default()
        };

        let json = serde_json::to_string(&config).unwrap();
        let parsed: BoxConfig = serde_json::from_str(&json).unwrap();

        match parsed.tee {
            TeeConfig::SevSnp {
                workload_id,
                generation,
                simulate,
            } => {
                assert_eq!(workload_id, "secure-agent");
                assert_eq!(generation, SevSnpGeneration::Milan);
                assert!(!simulate);
            }
            _ => panic!("Expected SevSnp TEE config"),
        }
    }

    #[test]
    fn test_box_config_default_has_no_tee() {
        let config = BoxConfig::default();
        assert_eq!(config.tee, TeeConfig::None);
    }

    // --- CacheConfig tests ---

    #[test]
    fn test_cache_config_default() {
        let config = CacheConfig::default();
        assert!(config.enabled);
        assert!(config.cache_dir.is_none());
        assert_eq!(config.max_rootfs_entries, 10);
        assert_eq!(config.max_cache_bytes, 10 * 1024 * 1024 * 1024);
    }

    #[test]
    fn test_cache_config_serialization() {
        let config = CacheConfig {
            enabled: false,
            cache_dir: Some(PathBuf::from("/tmp/cache")),
            max_rootfs_entries: 5,
            max_cache_bytes: 1024 * 1024 * 1024,
        };

        let json = serde_json::to_string(&config).unwrap();
        let parsed: CacheConfig = serde_json::from_str(&json).unwrap();

        assert!(!parsed.enabled);
        assert_eq!(parsed.cache_dir, Some(PathBuf::from("/tmp/cache")));
        assert_eq!(parsed.max_rootfs_entries, 5);
        assert_eq!(parsed.max_cache_bytes, 1024 * 1024 * 1024);
    }

    #[test]
    fn test_cache_config_deserialization_defaults() {
        let json = "{}";
        let config: CacheConfig = serde_json::from_str(json).unwrap();

        assert!(config.enabled);
        assert!(config.cache_dir.is_none());
        assert_eq!(config.max_rootfs_entries, 10);
        assert_eq!(config.max_cache_bytes, 10 * 1024 * 1024 * 1024);
    }

    // --- PoolConfig tests ---

    #[test]
    fn test_pool_config_default() {
        let config = PoolConfig::default();
        assert!(!config.enabled);
        assert_eq!(config.min_idle, 1);
        assert_eq!(config.max_size, 5);
        assert_eq!(config.idle_ttl_secs, 300);
    }

    #[test]
    fn test_pool_config_serialization() {
        let config = PoolConfig {
            enabled: true,
            min_idle: 3,
            max_size: 10,
            idle_ttl_secs: 600,
            ..Default::default()
        };

        let json = serde_json::to_string(&config).unwrap();
        let parsed: PoolConfig = serde_json::from_str(&json).unwrap();

        assert!(parsed.enabled);
        assert_eq!(parsed.min_idle, 3);
        assert_eq!(parsed.max_size, 10);
        assert_eq!(parsed.idle_ttl_secs, 600);
    }

    #[test]
    fn test_pool_config_deserialization_defaults() {
        let json = "{}";
        let config: PoolConfig = serde_json::from_str(json).unwrap();

        assert!(!config.enabled);
        assert_eq!(config.min_idle, 1);
        assert_eq!(config.max_size, 5);
        assert_eq!(config.idle_ttl_secs, 300);
    }

    // --- BoxConfig with new fields ---

    #[test]
    fn test_box_config_default_has_cache_and_pool() {
        let config = BoxConfig::default();
        assert!(config.cache.enabled);
        assert!(!config.pool.enabled);
    }

    #[test]
    fn test_box_config_with_cache_serialization() {
        let config = BoxConfig {
            cache: CacheConfig {
                enabled: false,
                cache_dir: Some(PathBuf::from("/custom/cache")),
                max_rootfs_entries: 20,
                max_cache_bytes: 5 * 1024 * 1024 * 1024,
            },
            ..Default::default()
        };

        let json = serde_json::to_string(&config).unwrap();
        let parsed: BoxConfig = serde_json::from_str(&json).unwrap();

        assert!(!parsed.cache.enabled);
        assert_eq!(parsed.cache.cache_dir, Some(PathBuf::from("/custom/cache")));
        assert_eq!(parsed.cache.max_rootfs_entries, 20);
    }

    #[test]
    fn test_box_config_with_pool_serialization() {
        let config = BoxConfig {
            pool: PoolConfig {
                enabled: true,
                min_idle: 2,
                max_size: 8,
                idle_ttl_secs: 120,
                ..Default::default()
            },
            ..Default::default()
        };

        let json = serde_json::to_string(&config).unwrap();
        let parsed: BoxConfig = serde_json::from_str(&json).unwrap();

        assert!(parsed.pool.enabled);
        assert_eq!(parsed.pool.min_idle, 2);
        assert_eq!(parsed.pool.max_size, 8);
        assert_eq!(parsed.pool.idle_ttl_secs, 120);
    }

    #[test]
    fn test_box_config_backward_compatible_deserialization() {
        // JSON without cache/pool fields should still deserialize with defaults
        let json = r#"{
            "workspace": "/tmp/workspace",
            "resources": {
                "vcpus": 2,
                "memory_mb": 1024,
                "disk_mb": 4096,
                "timeout": 3600
            },
            "log_level": "Info",
            "debug_grpc": false
        }"#;

        let config: BoxConfig = serde_json::from_str(json).unwrap();
        assert!(config.cache.enabled);
        assert!(!config.pool.enabled);
    }

    // --- ResourceLimits tests ---

    #[test]
    fn test_resource_limits_default() {
        let limits = ResourceLimits::default();
        assert!(limits.pids_limit.is_none());
        assert!(limits.cpuset_cpus.is_none());
        assert!(limits.ulimits.is_empty());
        assert!(limits.cpu_shares.is_none());
        assert!(limits.cpu_quota.is_none());
        assert!(limits.cpu_period.is_none());
        assert!(limits.memory_reservation.is_none());
        assert!(limits.memory_swap.is_none());
    }

    #[test]
    fn test_resource_limits_serialization() {
        let limits = ResourceLimits {
            pids_limit: Some(100),
            cpuset_cpus: Some("0,1".to_string()),
            ulimits: vec!["nofile=1024:4096".to_string()],
            cpu_shares: Some(512),
            cpu_quota: Some(50000),
            cpu_period: Some(100000),
            memory_reservation: Some(256 * 1024 * 1024),
            memory_swap: Some(1024 * 1024 * 1024),
        };

        let json = serde_json::to_string(&limits).unwrap();
        let parsed: ResourceLimits = serde_json::from_str(&json).unwrap();

        assert_eq!(parsed.pids_limit, Some(100));
        assert_eq!(parsed.cpuset_cpus, Some("0,1".to_string()));
        assert_eq!(parsed.ulimits, vec!["nofile=1024:4096"]);
        assert_eq!(parsed.cpu_shares, Some(512));
        assert_eq!(parsed.cpu_quota, Some(50000));
        assert_eq!(parsed.cpu_period, Some(100000));
        assert_eq!(parsed.memory_reservation, Some(256 * 1024 * 1024));
        assert_eq!(parsed.memory_swap, Some(1024 * 1024 * 1024));
    }

    #[test]
    fn test_resource_limits_deserialization_defaults() {
        let json = "{}";
        let limits: ResourceLimits = serde_json::from_str(json).unwrap();
        assert!(limits.pids_limit.is_none());
        assert!(limits.ulimits.is_empty());
    }

    #[test]
    fn test_resource_limits_memory_swap_unlimited() {
        let limits = ResourceLimits {
            memory_swap: Some(-1),
            ..Default::default()
        };

        let json = serde_json::to_string(&limits).unwrap();
        let parsed: ResourceLimits = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.memory_swap, Some(-1));
    }

    #[test]
    fn test_box_config_with_resource_limits() {
        let config = BoxConfig {
            resource_limits: ResourceLimits {
                pids_limit: Some(256),
                cpu_shares: Some(1024),
                ..Default::default()
            },
            ..Default::default()
        };

        let json = serde_json::to_string(&config).unwrap();
        let parsed: BoxConfig = serde_json::from_str(&json).unwrap();

        assert_eq!(parsed.resource_limits.pids_limit, Some(256));
        assert_eq!(parsed.resource_limits.cpu_shares, Some(1024));
    }

    #[test]
    fn test_box_config_backward_compat_no_resource_limits() {
        // Old configs without resource_limits should deserialize with defaults
        let json = r#"{
            "workspace": "/tmp/workspace",
            "resources": {
                "vcpus": 2,
                "memory_mb": 1024,
                "disk_mb": 4096,
                "timeout": 3600
            },
            "log_level": "Info",
            "debug_grpc": false
        }"#;

        let config: BoxConfig = serde_json::from_str(json).unwrap();
        assert!(config.resource_limits.pids_limit.is_none());
        assert!(config.resource_limits.ulimits.is_empty());
    }

    // ── SidecarConfig tests ───────────────────────────────────────────

    #[test]
    fn test_sidecar_config_default() {
        let s = SidecarConfig::default();
        assert!(s.image.is_empty());
        assert_eq!(s.vsock_port, 4092);
        assert!(s.env.is_empty());
    }

    #[test]
    fn test_sidecar_config_roundtrip() {
        let s = SidecarConfig {
            image: "ghcr.io/a3s-lab/safeclaw:latest".to_string(),
            vsock_port: 4092,
            env: vec![
                ("LOG_LEVEL".to_string(), "debug".to_string()),
                ("MODE".to_string(), "proxy".to_string()),
            ],
        };
        let json = serde_json::to_string(&s).unwrap();
        let parsed: SidecarConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.image, "ghcr.io/a3s-lab/safeclaw:latest");
        assert_eq!(parsed.vsock_port, 4092);
        assert_eq!(parsed.env.len(), 2);
        assert_eq!(
            parsed.env[0],
            ("LOG_LEVEL".to_string(), "debug".to_string())
        );
    }

    #[test]
    fn test_sidecar_config_default_vsock_port_from_json() {
        let json = r#"{"image":"safeclaw:latest"}"#;
        let s: SidecarConfig = serde_json::from_str(json).unwrap();
        assert_eq!(s.vsock_port, 4092);
        assert!(s.env.is_empty());
    }

    #[test]
    fn test_box_config_default_has_no_sidecar() {
        let config = BoxConfig::default();
        assert!(config.sidecar.is_none());
    }

    #[test]
    fn test_box_config_with_sidecar_roundtrip() {
        let mut config = BoxConfig::default();
        config.sidecar = Some(SidecarConfig {
            image: "safeclaw:latest".to_string(),
            vsock_port: 4092,
            env: vec![],
        });
        let json = serde_json::to_string(&config).unwrap();
        let parsed: BoxConfig = serde_json::from_str(&json).unwrap();
        let sidecar = parsed.sidecar.unwrap();
        assert_eq!(sidecar.image, "safeclaw:latest");
        assert_eq!(sidecar.vsock_port, 4092);
    }

    #[test]
    fn test_box_config_without_sidecar_deserializes_as_none() {
        // Old configs without sidecar field should deserialize with sidecar=None
        let config = BoxConfig::default();
        let json = serde_json::to_string(&config).unwrap();
        let parsed: BoxConfig = serde_json::from_str(&json).unwrap();
        assert!(parsed.sidecar.is_none());
    }
}
