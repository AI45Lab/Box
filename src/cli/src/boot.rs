//! Shared boot logic for starting a box from a persisted `BoxRecord`.
//!
//! Used by `start`, `restart`, and `monitor` commands to avoid duplicating
//! the "reconstruct BoxConfig from BoxRecord → VmManager::boot()" pattern.

use a3s_box_core::config::{BoxConfig, ResourceConfig};
use a3s_box_core::event::EventEmitter;
use a3s_box_runtime::{prom::RuntimeMetrics, NetworkStore, VmManager, VolumeStore};
use std::path::PathBuf;

use crate::commands::common;
use crate::state::{BoxRecord, HealthCheck};

/// Result of a successful box boot.
pub struct BootResult {
    /// PID of the shim process.
    pub pid: Option<u32>,
    /// Host-side exec socket path selected by the runtime.
    pub exec_socket_path: Option<PathBuf>,
    /// Host-side PTY socket path selected by the runtime.
    pub pty_socket_path: Option<PathBuf>,
    /// Effective health check after applying image defaults.
    pub health_check: Option<HealthCheck>,
    /// Effective stop signal after applying image defaults.
    pub stop_signal: Option<String>,
    /// Anonymous volumes present after boot.
    pub anonymous_volumes: Vec<String>,
}

/// How a successful boot should update the restart counter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RestartCountUpdate {
    /// Reset the counter for a user-initiated `start`.
    Reset,
    /// Leave the counter unchanged for a user-initiated `restart`.
    Preserve,
    /// Increment the counter for monitor-driven automatic recovery.
    Increment,
}

struct BootResourceGuard {
    box_id: String,
    volume_names: Vec<String>,
    network_name: Option<String>,
    armed: bool,
}

impl BootResourceGuard {
    fn new(record: &BoxRecord) -> Self {
        Self {
            box_id: record.id.clone(),
            volume_names: record.volume_names.clone(),
            network_name: boot_network_name(record).map(str::to_string),
            armed: true,
        }
    }

    fn rollback(&mut self) {
        if !self.armed {
            return;
        }
        crate::cleanup::cleanup_box_resources(
            &self.box_id,
            &self.volume_names,
            self.network_name.as_deref(),
        );
        self.armed = false;
    }

    fn disarm(mut self) {
        self.armed = false;
    }
}

fn boot_network_name(record: &BoxRecord) -> Option<&str> {
    record
        .network_name
        .as_deref()
        .or(match &record.network_mode {
            a3s_box_core::NetworkMode::Bridge { network } => Some(network.as_str()),
            _ => None,
        })
}

/// Apply a successful boot result to a persisted record.
pub fn apply_boot_result(
    record: &mut BoxRecord,
    result: BootResult,
    restart_count_update: RestartCountUpdate,
) {
    record.status = "running".to_string();
    record.pid = result.pid;
    if let Some(exec_socket_path) = result.exec_socket_path {
        record.exec_socket_path = exec_socket_path;
    }
    record.health_check = result.health_check;
    record.health_status = if record.health_check.is_some() {
        "starting".to_string()
    } else {
        "none".to_string()
    };
    record.health_retries = 0;
    record.health_last_check = None;
    record.stop_signal = result.stop_signal;
    record.started_at = Some(chrono::Utc::now());
    record.stopped_by_user = false;
    record.exit_code = None;

    for volume_name in result.anonymous_volumes {
        if !record
            .anonymous_volumes
            .iter()
            .any(|existing| existing == &volume_name)
        {
            record.anonymous_volumes.push(volume_name);
        }
    }

    match restart_count_update {
        RestartCountUpdate::Reset => record.restart_count = 0,
        RestartCountUpdate::Preserve => {}
        RestartCountUpdate::Increment => record.restart_count += 1,
    }
}

fn ensure_boot_resources(
    record: &BoxRecord,
) -> Result<BootResourceGuard, Box<dyn std::error::Error>> {
    ensure_network_connected(record)?;
    let mut guard = BootResourceGuard::new(record);

    if !record.volume_names.is_empty() {
        let volume_store = VolumeStore::default_path()?;
        if let Err(error) = crate::commands::volume::attach_volumes_with_store(
            &volume_store,
            &record.volume_names,
            &record.id,
        ) {
            guard.rollback();
            return Err(error);
        }
    }

    Ok(guard)
}

fn ensure_network_connected(record: &BoxRecord) -> Result<(), Box<dyn std::error::Error>> {
    let Some(network_name) = boot_network_name(record) else {
        return Ok(());
    };

    let network_store = NetworkStore::default_path()?;
    ensure_network_connected_with_store(&network_store, record, network_name)
}

fn ensure_network_connected_with_store(
    network_store: &NetworkStore,
    record: &BoxRecord,
    network_name: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut network = network_store
        .get(network_name)?
        .ok_or_else(|| format!("network '{}' not found", network_name))?;
    crate::commands::network::validate_attachable_network(&network)
        .map_err(|e| -> Box<dyn std::error::Error> { e.into() })?;

    if network.endpoints.contains_key(&record.id) {
        return Ok(());
    }

    network
        .connect(&record.id, &record.name)
        .map_err(|error| format!("Failed to connect to network: {error}"))?;
    network_store.update(&network)?;
    Ok(())
}

/// Reconstruct a `BoxConfig` from a persisted `BoxRecord` and boot the VM.
///
/// On success, returns the new PID. The caller is responsible for updating
/// the `BoxRecord` state (status, pid, started_at, etc.) and saving.
pub async fn boot_from_record(
    record: &BoxRecord,
) -> Result<BootResult, Box<dyn std::error::Error>> {
    let config =
        config_from_record(record).map_err(|e| -> Box<dyn std::error::Error> { e.into() })?;
    let emitter = EventEmitter::new(256);
    let mut vm = VmManager::with_box_id(config, emitter, record.id.clone());

    // Activate Prometheus metrics collection
    vm.set_metrics(RuntimeMetrics::try_new()?);

    let mut resource_guard = ensure_boot_resources(record)?;
    if let Err(error) = vm.boot().await {
        resource_guard.rollback();
        return Err(error.into());
    }
    resource_guard.disarm();

    // Create rootfs baseline snapshot for `diff` command (best-effort).
    if let Err(error) = crate::commands::diff::create_box_baseline_snapshot(&record.box_dir) {
        tracing::warn!(
            box_id = %record.id,
            error = %error,
            "Failed to create rootfs diff baseline snapshot"
        );
    }

    // Spawn structured log processor (json-file driver writes container.json)
    let log_dir = record.box_dir.join("logs");
    let _ = std::fs::create_dir_all(&log_dir);
    let _log_handle = a3s_box_runtime::log::spawn_log_processor(
        record.console_log.clone(),
        log_dir,
        record.log_config.clone(),
    );

    let pid = vm.pid().await;
    let exec_socket_path = vm.exec_socket_path().map(PathBuf::from);
    let pty_socket_path = vm.pty_socket_path().map(PathBuf::from);
    let anonymous_volumes = vm.anonymous_volumes().to_vec();
    let image_health_check = vm
        .image_config()
        .and_then(|config| config.health_check.clone());
    let image_stop_signal = vm
        .image_config()
        .and_then(|config| config.stop_signal.clone());
    let health_check = if record.healthcheck_disabled {
        None
    } else {
        record.health_check.clone().or_else(|| {
            image_health_check
                .as_ref()
                .and_then(common::health_check_from_oci)
        })
    };
    let stop_signal =
        common::effective_stop_signal(record.stop_signal.as_deref(), image_stop_signal.as_deref());

    // Spawn health checker if configured (self-terminates when box stops)
    if let Some(ref hc) = health_check {
        crate::health::spawn_health_checker(
            record.id.clone(),
            exec_socket_path
                .clone()
                .unwrap_or_else(|| record.exec_socket_path.clone()),
            hc.clone(),
        );
    }

    Ok(BootResult {
        pid,
        exec_socket_path,
        pty_socket_path,
        health_check,
        stop_signal,
        anonymous_volumes,
    })
}

/// Build a `BoxConfig` from a `BoxRecord`.
///
/// Reconstructs the full configuration needed to boot a VM from the
/// persisted record fields.
fn config_from_record(record: &BoxRecord) -> Result<BoxConfig, String> {
    // Translate shm_size to a tmpfs entry, reusing the BOX_TMPFS_* guest init mechanism.
    let mut tmpfs = record.tmpfs.clone();
    if let Some(size_bytes) = record.shm_size {
        tmpfs.push(format!("/dev/shm:size={}", size_bytes));
    }
    let user = common::normalize_user_option(record.user.as_deref())?;
    common::validate_workdir_option(record.workdir.as_deref())?;
    let port_map = common::normalize_port_maps(&record.port_map)?;
    if let Some(hostname) = record.hostname.as_deref() {
        a3s_box_core::dns::validate_hostname(hostname)
            .map_err(|e| format!("Invalid persisted hostname: {e}"))?;
    }
    a3s_box_core::dns::parse_add_host_entries(&record.add_host)
        .map_err(|e| format!("Invalid persisted add-host entry: {e}"))?;

    Ok(BoxConfig {
        image: record.image.clone(),
        resources: ResourceConfig {
            vcpus: record.cpus,
            memory_mb: record.memory_mb,
            ..Default::default()
        },
        cmd: record.cmd.clone(),
        entrypoint_override: record.entrypoint.clone(),
        user,
        workdir: record.workdir.clone(),
        hostname: record.hostname.clone(),
        volumes: record.volumes.clone(),
        extra_env: record
            .env
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect(),
        port_map,
        add_hosts: record.add_host.clone(),
        network: record.network_mode.clone(),
        tmpfs,
        resource_limits: record.resource_limits.clone(),
        read_only: record.read_only,
        cap_add: record.cap_add.clone(),
        cap_drop: record.cap_drop.clone(),
        security_opt: record.security_opt.clone(),
        privileged: record.privileged,
        ..Default::default()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::path::PathBuf;

    fn sample_record() -> BoxRecord {
        let id = "test-boot-id".to_string();
        let short_id = BoxRecord::make_short_id(&id);
        BoxRecord {
            id: id.clone(),
            short_id,
            name: "test_box".to_string(),
            image: "alpine:latest".to_string(),
            status: "stopped".to_string(),
            pid: None,
            cpus: 4,
            memory_mb: 2048,
            volumes: vec!["/host:/guest".to_string()],
            env: {
                let mut m = HashMap::new();
                m.insert("FOO".to_string(), "bar".to_string());
                m
            },
            cmd: vec!["sh".to_string(), "-c".to_string(), "echo hi".to_string()],
            entrypoint: Some(vec!["/bin/sh".to_string()]),
            box_dir: PathBuf::from("/tmp/boxes").join(&id),
            exec_socket_path: PathBuf::from("/tmp/boxes")
                .join(&id)
                .join("sockets")
                .join("exec.sock"),
            console_log: PathBuf::from("/tmp/boxes").join(&id).join("console.log"),
            created_at: chrono::Utc::now(),
            started_at: None,
            auto_remove: false,
            hostname: Some("myhost".to_string()),
            user: Some("root".to_string()),
            workdir: Some("/app".to_string()),
            restart_policy: "always".to_string(),
            port_map: vec!["8080:80".to_string()],
            labels: HashMap::new(),
            stopped_by_user: false,
            restart_count: 0,
            max_restart_count: 0,
            exit_code: None,
            health_check: None,
            healthcheck_disabled: false,
            health_status: "none".to_string(),
            health_retries: 0,
            health_last_check: None,
            network_mode: a3s_box_core::NetworkMode::default(),
            network_name: None,
            volume_names: vec![],
            tmpfs: vec!["/tmp".to_string()],
            anonymous_volumes: vec![],
            resource_limits: a3s_box_core::config::ResourceLimits::default(),
            log_config: a3s_box_core::log::LogConfig::default(),
            add_host: vec![],
            platform: None,
            init: false,
            read_only: false,
            cap_add: vec![],
            cap_drop: vec![],
            security_opt: vec![],
            privileged: false,
            devices: vec![],
            gpus: None,
            shm_size: None,
            stop_signal: None,
            stop_timeout: None,
            oom_kill_disable: false,
            oom_score_adj: None,
        }
    }

    #[test]
    fn test_config_from_record_image() {
        let record = sample_record();
        let config = config_from_record(&record).unwrap();
        assert_eq!(config.image, "alpine:latest");
    }

    #[test]
    fn test_config_from_record_resources() {
        let record = sample_record();
        let config = config_from_record(&record).unwrap();

        assert_eq!(config.resources.vcpus, 4);
        assert_eq!(config.resources.memory_mb, 2048);
    }

    #[test]
    fn test_config_from_record_cmd_and_entrypoint() {
        let record = sample_record();
        let config = config_from_record(&record).unwrap();

        assert_eq!(config.cmd, vec!["sh", "-c", "echo hi"]);
        assert_eq!(
            config.entrypoint_override,
            Some(vec!["/bin/sh".to_string()])
        );
    }

    #[test]
    fn test_config_from_record_volumes() {
        let record = sample_record();
        let config = config_from_record(&record).unwrap();

        assert_eq!(config.volumes, vec!["/host:/guest"]);
        assert_eq!(config.tmpfs, vec!["/tmp"]);
    }

    #[test]
    fn test_config_from_record_shm_size_appends_tmpfs() {
        let mut record = sample_record();
        record.shm_size = Some(64 * 1024 * 1024); // 64 MiB
        let config = config_from_record(&record).unwrap();

        assert!(config.tmpfs.contains(&"/tmp".to_string()));
        assert!(config.tmpfs.iter().any(|t| t == "/dev/shm:size=67108864"));
    }

    #[test]
    fn test_config_from_record_shm_size_none() {
        let record = sample_record();
        let config = config_from_record(&record).unwrap();

        // No /dev/shm entry when shm_size is None
        assert!(!config.tmpfs.iter().any(|t| t.contains("/dev/shm")));
    }

    #[test]
    fn test_config_from_record_read_only() {
        let mut record = sample_record();
        record.read_only = true;
        let config = config_from_record(&record).unwrap();
        assert!(config.read_only);
    }

    #[test]
    fn test_config_from_record_security_options() {
        let mut record = sample_record();
        record.cap_add = vec!["NET_ADMIN".to_string()];
        record.cap_drop = vec!["NET_RAW".to_string()];
        record.security_opt = vec!["seccomp=unconfined".to_string()];
        record.privileged = true;

        let config = config_from_record(&record).unwrap();

        assert_eq!(config.cap_add, vec!["NET_ADMIN"]);
        assert_eq!(config.cap_drop, vec!["NET_RAW"]);
        assert_eq!(config.security_opt, vec!["seccomp=unconfined"]);
        assert!(config.privileged);
    }

    #[test]
    fn test_config_from_record_user_and_workdir() {
        let record = sample_record();
        let config = config_from_record(&record).unwrap();

        assert_eq!(config.user.as_deref(), Some("0"));
        assert_eq!(config.workdir.as_deref(), Some("/app"));
    }

    #[test]
    fn test_config_from_record_hostname_and_add_hosts() {
        let mut record = sample_record();
        record.hostname = Some("web".to_string());
        record.add_host = vec!["db.local:10.88.0.10".to_string()];

        let config = config_from_record(&record).unwrap();

        assert_eq!(config.hostname.as_deref(), Some("web"));
        assert_eq!(config.add_hosts, vec!["db.local:10.88.0.10"]);
    }

    #[test]
    fn test_config_from_record_rejects_invalid_add_host() {
        let mut record = sample_record();
        record.add_host = vec!["db.local:not-an-ip".to_string()];

        let err = config_from_record(&record).unwrap_err();

        assert!(err.contains("Invalid persisted add-host"));
    }

    #[test]
    fn test_config_from_record_accepts_named_user() {
        // Named users are forwarded as-is and resolved in the guest.
        let mut record = sample_record();
        record.user = Some("node".to_string());

        let config = config_from_record(&record).unwrap();
        assert_eq!(config.user.as_deref(), Some("node"));
    }

    #[test]
    fn test_config_from_record_env() {
        let record = sample_record();
        let config = config_from_record(&record).unwrap();

        assert!(config
            .extra_env
            .contains(&("FOO".to_string(), "bar".to_string())));
    }

    #[test]
    fn test_config_from_record_port_map() {
        let record = sample_record();
        let config = config_from_record(&record).unwrap();

        assert_eq!(config.port_map, vec!["8080:80"]);
    }

    #[test]
    fn test_config_from_record_normalizes_tcp_port_suffix() {
        let mut record = sample_record();
        record.port_map = vec!["8080:80/tcp".to_string()];

        let config = config_from_record(&record).unwrap();

        assert_eq!(config.port_map, vec!["8080:80"]);
    }

    #[test]
    fn test_config_from_record_rejects_udp_port_map() {
        let mut record = sample_record();
        record.port_map = vec!["8080:80/udp".to_string()];

        let err = config_from_record(&record).unwrap_err();

        assert!(err.contains("only TCP is supported"));
    }

    #[test]
    fn test_config_from_record_network_mode() {
        let record = sample_record();
        let config = config_from_record(&record).unwrap();

        assert_eq!(config.network, a3s_box_core::NetworkMode::Tsi);
    }

    #[test]
    fn test_boot_network_name_falls_back_to_network_mode() {
        let mut record = sample_record();
        record.network_mode = a3s_box_core::NetworkMode::Bridge {
            network: "dev".to_string(),
        };
        record.network_name = None;

        assert_eq!(boot_network_name(&record), Some("dev"));
    }

    #[test]
    fn test_ensure_network_connected_with_store_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let network_store = NetworkStore::new(dir.path().join("networks.json"));
        network_store
            .create(a3s_box_core::network::NetworkConfig::new("dev", "10.88.0.0/24").unwrap())
            .unwrap();
        let mut record = sample_record();
        record.network_mode = a3s_box_core::NetworkMode::Bridge {
            network: "dev".to_string(),
        };

        ensure_network_connected_with_store(&network_store, &record, "dev").unwrap();
        ensure_network_connected_with_store(&network_store, &record, "dev").unwrap();

        let network = network_store.get("dev").unwrap().unwrap();
        assert_eq!(network.endpoints.len(), 1);
        assert!(network.endpoints.contains_key(&record.id));
    }

    fn sample_boot_result() -> BootResult {
        BootResult {
            pid: Some(1234),
            exec_socket_path: Some(PathBuf::from("/tmp/new-exec.sock")),
            pty_socket_path: Some(PathBuf::from("/tmp/new-pty.sock")),
            health_check: Some(HealthCheck {
                cmd: vec!["true".to_string()],
                interval_secs: 30,
                timeout_secs: 5,
                retries: 3,
                start_period_secs: 0,
            }),
            stop_signal: Some("SIGINT".to_string()),
            anonymous_volumes: vec!["old-anon".to_string(), "new-anon".to_string()],
        }
    }

    #[test]
    fn test_apply_boot_result_updates_running_state_and_merges_anonymous_volumes() {
        let mut record = sample_record();
        record.status = "dead".to_string();
        record.restart_count = 7;
        record.stopped_by_user = true;
        record.exit_code = Some(137);
        record.health_status = "unhealthy".to_string();
        record.health_retries = 2;
        record.health_last_check = Some(chrono::Utc::now());
        record.anonymous_volumes = vec!["old-anon".to_string()];

        apply_boot_result(&mut record, sample_boot_result(), RestartCountUpdate::Reset);

        assert_eq!(record.status, "running");
        assert_eq!(record.pid, Some(1234));
        assert_eq!(record.exec_socket_path, PathBuf::from("/tmp/new-exec.sock"));
        assert_eq!(record.health_status, "starting");
        assert_eq!(record.health_retries, 0);
        assert!(record.health_last_check.is_none());
        assert_eq!(record.stop_signal.as_deref(), Some("SIGINT"));
        assert!(record.started_at.is_some());
        assert!(!record.stopped_by_user);
        assert!(record.exit_code.is_none());
        assert_eq!(record.restart_count, 0);
        assert_eq!(
            record.anonymous_volumes,
            vec!["old-anon".to_string(), "new-anon".to_string()]
        );
    }

    #[test]
    fn test_apply_boot_result_preserves_manual_restart_count() {
        let mut record = sample_record();
        record.restart_count = 4;
        let mut result = sample_boot_result();
        result.health_check = None;

        apply_boot_result(&mut record, result, RestartCountUpdate::Preserve);

        assert_eq!(record.restart_count, 4);
        assert_eq!(record.health_status, "none");
    }

    #[test]
    fn test_apply_boot_result_increments_monitor_restart_count() {
        let mut record = sample_record();
        record.restart_count = 4;

        apply_boot_result(
            &mut record,
            sample_boot_result(),
            RestartCountUpdate::Increment,
        );

        assert_eq!(record.restart_count, 5);
    }
}
