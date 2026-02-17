//! Shared test helpers for CLI command tests.

#[cfg(test)]
pub mod fixtures {
    use crate::state::{BoxRecord, StateFile};
    use std::collections::HashMap;
    use std::path::PathBuf;
    use tempfile::TempDir;

    /// Create a test BoxRecord with the given parameters.
    pub fn make_record(id: &str, name: &str, status: &str, pid: Option<u32>) -> BoxRecord {
        let short_id = BoxRecord::make_short_id(id);
        BoxRecord {
            id: id.to_string(),
            short_id,
            name: name.to_string(),
            image: "alpine:latest".to_string(),
            status: status.to_string(),
            pid,
            cpus: 2,
            memory_mb: 512,
            volumes: vec![],
            env: HashMap::new(),
            cmd: vec![],
            entrypoint: None,
            box_dir: PathBuf::from("/tmp").join(id),
            socket_path: PathBuf::from("/tmp").join(id).join("grpc.sock"),
            exec_socket_path: PathBuf::from("/tmp")
                .join(id)
                .join("sockets")
                .join("exec.sock"),
            console_log: PathBuf::from("/tmp").join(id).join("console.log"),
            created_at: chrono::Utc::now(),
            started_at: None,
            auto_remove: false,
            hostname: None,
            user: None,
            workdir: None,
            restart_policy: "no".to_string(),
            port_map: vec![],
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
            tmpfs: vec![],
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

    /// Create a temporary StateFile pre-loaded with the given records.
    pub fn setup_state(records: Vec<BoxRecord>) -> (TempDir, StateFile) {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("boxes.json");
        let mut sf = StateFile::load(&path).unwrap();
        for r in records {
            sf.add(r).unwrap();
        }
        (tmp, sf)
    }
}
