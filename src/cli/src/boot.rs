//! Shared boot logic for starting a box from a persisted `BoxRecord`.
//!
//! Used by `start`, `restart`, and `monitor` commands to avoid duplicating
//! the "reconstruct BoxConfig from BoxRecord → VmManager::boot()" pattern.

use a3s_box_core::config::{AgentType, BoxConfig, ResourceConfig};
use a3s_box_core::event::EventEmitter;
use a3s_box_runtime::{prom::RuntimeMetrics, VmManager};

use crate::state::BoxRecord;

/// Result of a successful box boot.
pub struct BootResult {
    /// PID of the shim process.
    pub pid: Option<u32>,
}

/// Reconstruct a `BoxConfig` from a persisted `BoxRecord` and boot the VM.
///
/// On success, returns the new PID. The caller is responsible for updating
/// the `BoxRecord` state (status, pid, started_at, etc.) and saving.
pub async fn boot_from_record(
    record: &BoxRecord,
) -> Result<BootResult, Box<dyn std::error::Error>> {
    let config = config_from_record(record);
    let emitter = EventEmitter::new(256);
    let mut vm = VmManager::with_box_id(config, emitter, record.id.clone());

    // Activate Prometheus metrics collection
    vm.set_metrics(RuntimeMetrics::new());

    vm.boot().await?;

    // Create rootfs baseline snapshot for `diff` command (best-effort)
    let rootfs_dir = record.box_dir.join("rootfs");
    let snapshot_path = record.box_dir.join("rootfs_snapshot.json");
    if rootfs_dir.exists() && !snapshot_path.exists() {
        let _ = crate::commands::diff::create_snapshot(&rootfs_dir, &snapshot_path);
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
    Ok(BootResult { pid })
}

/// Build a `BoxConfig` from a `BoxRecord`.
///
/// Reconstructs the full configuration needed to boot a VM from the
/// persisted record fields.
fn config_from_record(record: &BoxRecord) -> BoxConfig {
    BoxConfig {
        agent: AgentType::OciRegistry {
            reference: record.image.clone(),
        },
        resources: ResourceConfig {
            vcpus: record.cpus,
            memory_mb: record.memory_mb,
            ..Default::default()
        },
        cmd: record.cmd.clone(),
        entrypoint_override: record.entrypoint.clone(),
        volumes: record.volumes.clone(),
        extra_env: record
            .env
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect(),
        port_map: record.port_map.clone(),
        network: record.network_mode.clone(),
        tmpfs: record.tmpfs.clone(),
        resource_limits: record.resource_limits.clone(),
        ..Default::default()
    }
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
        let config = config_from_record(&record);

        match &config.agent {
            AgentType::OciRegistry { reference } => {
                assert_eq!(reference, "alpine:latest");
            }
            _ => panic!("Expected OciRegistry agent type"),
        }
    }

    #[test]
    fn test_config_from_record_resources() {
        let record = sample_record();
        let config = config_from_record(&record);

        assert_eq!(config.resources.vcpus, 4);
        assert_eq!(config.resources.memory_mb, 2048);
    }

    #[test]
    fn test_config_from_record_cmd_and_entrypoint() {
        let record = sample_record();
        let config = config_from_record(&record);

        assert_eq!(config.cmd, vec!["sh", "-c", "echo hi"]);
        assert_eq!(
            config.entrypoint_override,
            Some(vec!["/bin/sh".to_string()])
        );
    }

    #[test]
    fn test_config_from_record_volumes() {
        let record = sample_record();
        let config = config_from_record(&record);

        assert_eq!(config.volumes, vec!["/host:/guest"]);
        assert_eq!(config.tmpfs, vec!["/tmp"]);
    }

    #[test]
    fn test_config_from_record_env() {
        let record = sample_record();
        let config = config_from_record(&record);

        assert!(config
            .extra_env
            .contains(&("FOO".to_string(), "bar".to_string())));
    }

    #[test]
    fn test_config_from_record_port_map() {
        let record = sample_record();
        let config = config_from_record(&record);

        assert_eq!(config.port_map, vec!["8080:80"]);
    }

    #[test]
    fn test_config_from_record_network_mode() {
        let record = sample_record();
        let config = config_from_record(&record);

        assert_eq!(config.network, a3s_box_core::NetworkMode::Tsi);
    }
}
