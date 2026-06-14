//! `a3s-box rename` command — Rename a box.

use clap::Args;

use crate::resolve;
use crate::state::StateFile;

#[derive(Args)]
pub struct RenameArgs {
    /// Box name or ID
    pub r#box: String,

    /// New name for the box
    pub new_name: String,
}

pub async fn execute(args: RenameArgs) -> Result<(), Box<dyn std::error::Error>> {
    // Do the name-conflict check and the rename atomically under the state lock
    // (load-fresh + mutate + save) so a concurrent writer cannot be clobbered.
    let old_name = StateFile::modify(|state| -> Result<String, Box<dyn std::error::Error>> {
        if state.find_by_name(&args.new_name).is_some() {
            return Err(format!("Name \"{}\" is already in use", args.new_name).into());
        }
        let record = resolve::resolve_mut(state, &args.r#box)?;
        let old_name = record.name.clone();
        record.name = args.new_name.clone();
        Ok(old_name)
    })?;

    println!("Renamed {} → {}", old_name, args.new_name);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::BoxRecord;
    use std::collections::HashMap;
    use std::path::PathBuf;
    use tempfile::TempDir;

    fn make_record(id: &str, name: &str) -> BoxRecord {
        let short_id = BoxRecord::make_short_id(id);
        BoxRecord {
            id: id.to_string(),
            short_id,
            name: name.to_string(),
            image: "alpine:latest".to_string(),
            status: "created".to_string(),
            pid: None,
            cpus: 2,
            memory_mb: 512,
            volumes: vec![],
            env: HashMap::new(),
            cmd: vec![],
            entrypoint: None,
            box_dir: PathBuf::from("/tmp").join(id),
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
            healthcheck_disabled: false,
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

    fn setup_state(records: Vec<BoxRecord>) -> (TempDir, StateFile) {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("boxes.json");
        let mut sf = StateFile::load(&path).unwrap();
        for r in records {
            sf.add(r).unwrap();
        }
        (tmp, sf)
    }

    #[test]
    fn test_rename_success() {
        let (_tmp, mut state) = setup_state(vec![make_record("id-1", "old_name")]);

        // Simulate rename logic
        assert!(state.find_by_name("new_name").is_none());
        let record = resolve::resolve_mut(&mut state, "old_name").unwrap();
        record.name = "new_name".to_string();
        state.save().unwrap();

        assert!(state.find_by_name("new_name").is_some());
        assert!(state.find_by_name("old_name").is_none());
    }

    #[test]
    fn test_rename_conflict() {
        let (_tmp, state) = setup_state(vec![
            make_record("id-1", "box_a"),
            make_record("id-2", "box_b"),
        ]);

        // "box_b" already exists — rename should fail
        assert!(state.find_by_name("box_b").is_some());
    }

    #[test]
    fn test_rename_not_found() {
        let (_tmp, mut state) = setup_state(vec![make_record("id-1", "box_a")]);
        let result = resolve::resolve_mut(&mut state, "nonexistent");
        assert!(result.is_err());
    }

    #[test]
    fn test_rename_by_id() {
        let (_tmp, mut state) = setup_state(vec![make_record("abc-123", "old_name")]);

        let record = resolve::resolve_mut(&mut state, "abc-123").unwrap();
        record.name = "new_name".to_string();
        state.save().unwrap();

        assert_eq!(state.find_by_id("abc-123").unwrap().name, "new_name");
    }
}
