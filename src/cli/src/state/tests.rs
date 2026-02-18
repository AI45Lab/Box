//! Tests for state management.

use super::*;
use std::collections::HashMap;
use tempfile::TempDir;

fn test_state_path(tmp: &TempDir) -> PathBuf {
    tmp.path().join("boxes.json")
}

fn sample_record(id: &str, name: &str, status: &str) -> BoxRecord {
    let short_id = BoxRecord::make_short_id(id);
    BoxRecord {
        id: id.to_string(),
        short_id,
        name: name.to_string(),
        image: "alpine:latest".to_string(),
        status: status.to_string(),
        pid: if status == "running" {
            Some(99999)
        } else {
            None
        },
        cpus: 2,
        memory_mb: 512,
        volumes: vec![],
        env: HashMap::new(),
        cmd: vec![],
        entrypoint: None,
        box_dir: PathBuf::from("/tmp/boxes").join(id),
        exec_socket_path: PathBuf::from("/tmp/boxes")
            .join(id)
            .join("sockets")
            .join("exec.sock"),
        console_log: PathBuf::from("/tmp/boxes").join(id).join("console.log"),
        created_at: Utc::now(),
        started_at: if status == "running" {
            Some(Utc::now())
        } else {
            None
        },
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

// --- BoxRecord tests ---

#[test]
fn test_make_short_id() {
    let id = "550e8400-e29b-41d4-a716-446655440000";
    assert_eq!(BoxRecord::make_short_id(id), "550e8400e29b");
}

#[test]
fn test_make_short_id_no_dashes() {
    let id = "abcdef1234567890";
    assert_eq!(BoxRecord::make_short_id(id), "abcdef123456");
}

#[test]
fn test_make_short_id_short_input() {
    let id = "abc";
    assert_eq!(BoxRecord::make_short_id(id), "abc");
}

#[test]
fn test_make_short_id_empty() {
    assert_eq!(BoxRecord::make_short_id(""), "");
}

#[test]
fn test_box_record_serialization() {
    let record = sample_record("test-id-123", "my_box", "created");
    let json = serde_json::to_string(&record).unwrap();
    let parsed: BoxRecord = serde_json::from_str(&json).unwrap();

    assert_eq!(parsed.id, "test-id-123");
    assert_eq!(parsed.name, "my_box");
    assert_eq!(parsed.status, "created");
    assert_eq!(parsed.image, "alpine:latest");
    assert_eq!(parsed.cpus, 2);
    assert_eq!(parsed.memory_mb, 512);
    assert!(parsed.pid.is_none());
}

#[test]
fn test_box_record_serialization_with_env() {
    let mut record = sample_record("env-id", "env_box", "created");
    record.env.insert("FOO".to_string(), "bar".to_string());
    record.env.insert("BAZ".to_string(), "qux".to_string());
    record.volumes = vec!["/host:/guest".to_string()];
    record.cmd = vec!["sh".to_string(), "-c".to_string(), "echo hi".to_string()];

    let json = serde_json::to_string(&record).unwrap();
    let parsed: BoxRecord = serde_json::from_str(&json).unwrap();

    assert_eq!(parsed.env.get("FOO").unwrap(), "bar");
    assert_eq!(parsed.env.get("BAZ").unwrap(), "qux");
    assert_eq!(parsed.volumes, vec!["/host:/guest"]);
    assert_eq!(parsed.cmd, vec!["sh", "-c", "echo hi"]);
}

#[test]
fn test_box_record_serialization_running() {
    let record = sample_record("run-id", "runner", "running");
    let json = serde_json::to_string(&record).unwrap();
    let parsed: BoxRecord = serde_json::from_str(&json).unwrap();

    assert_eq!(parsed.status, "running");
    assert_eq!(parsed.pid, Some(99999));
    assert!(parsed.started_at.is_some());
}

// --- StateFile basic tests ---

#[test]
fn test_load_empty() {
    let tmp = TempDir::new().unwrap();
    let sf = StateFile::load(&test_state_path(&tmp)).unwrap();
    assert!(sf.records().is_empty());
}

#[test]
fn test_load_creates_parent_dir() {
    let tmp = TempDir::new().unwrap();
    let path = tmp.path().join("nested").join("dir").join("boxes.json");
    let sf = StateFile::load(&path).unwrap();
    assert!(sf.records().is_empty());
    assert!(path.parent().unwrap().exists());
}

#[test]
fn test_load_corrupt_json_returns_empty() {
    let tmp = TempDir::new().unwrap();
    let path = test_state_path(&tmp);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, "not valid json!!!").unwrap();

    let sf = StateFile::load(&path).unwrap();
    assert!(sf.records().is_empty());
}

#[test]
fn test_add_and_find() {
    let tmp = TempDir::new().unwrap();
    let mut sf = StateFile::load(&test_state_path(&tmp)).unwrap();

    let record = sample_record("abc-def-123", "test_box", "created");
    sf.add(record).unwrap();

    assert_eq!(sf.records().len(), 1);
    assert!(sf.find_by_id("abc-def-123").is_some());
    assert!(sf.find_by_name("test_box").is_some());
}

#[test]
fn test_add_multiple() {
    let tmp = TempDir::new().unwrap();
    let mut sf = StateFile::load(&test_state_path(&tmp)).unwrap();

    sf.add(sample_record("id1", "box1", "created")).unwrap();
    sf.add(sample_record("id2", "box2", "stopped")).unwrap();
    sf.add(sample_record("id3", "box3", "dead")).unwrap();

    assert_eq!(sf.records().len(), 3);
    assert!(sf.find_by_id("id1").is_some());
    assert!(sf.find_by_id("id2").is_some());
    assert!(sf.find_by_id("id3").is_some());
}

#[test]
fn test_find_by_name_not_found() {
    let tmp = TempDir::new().unwrap();
    let mut sf = StateFile::load(&test_state_path(&tmp)).unwrap();
    sf.add(sample_record("id1", "box1", "created")).unwrap();

    assert!(sf.find_by_name("nonexistent").is_none());
}

#[test]
fn test_find_by_id_not_found() {
    let tmp = TempDir::new().unwrap();
    let mut sf = StateFile::load(&test_state_path(&tmp)).unwrap();
    sf.add(sample_record("id1", "box1", "created")).unwrap();

    assert!(sf.find_by_id("wrong-id").is_none());
}

// --- Remove tests ---

#[test]
fn test_remove() {
    let tmp = TempDir::new().unwrap();
    let mut sf = StateFile::load(&test_state_path(&tmp)).unwrap();

    let record = sample_record("abc-def-123", "test_box", "created");
    sf.add(record).unwrap();

    assert!(sf.remove("abc-def-123").unwrap());
    assert!(sf.records().is_empty());
}

#[test]
fn test_remove_nonexistent() {
    let tmp = TempDir::new().unwrap();
    let mut sf = StateFile::load(&test_state_path(&tmp)).unwrap();
    assert!(!sf.remove("nonexistent").unwrap());
}

#[test]
fn test_remove_preserves_others() {
    let tmp = TempDir::new().unwrap();
    let mut sf = StateFile::load(&test_state_path(&tmp)).unwrap();

    sf.add(sample_record("id1", "box1", "created")).unwrap();
    sf.add(sample_record("id2", "box2", "created")).unwrap();
    sf.add(sample_record("id3", "box3", "created")).unwrap();

    assert!(sf.remove("id2").unwrap());
    assert_eq!(sf.records().len(), 2);
    assert!(sf.find_by_id("id1").is_some());
    assert!(sf.find_by_id("id2").is_none());
    assert!(sf.find_by_id("id3").is_some());
}

// --- Prefix search tests ---

#[test]
fn test_find_by_id_prefix() {
    let tmp = TempDir::new().unwrap();
    let mut sf = StateFile::load(&test_state_path(&tmp)).unwrap();

    sf.add(sample_record("abc-def-123", "box1", "created"))
        .unwrap();
    sf.add(sample_record("abc-def-456", "box2", "created"))
        .unwrap();
    sf.add(sample_record("xyz-000-111", "box3", "created"))
        .unwrap();

    assert_eq!(sf.find_by_id_prefix("abc").len(), 2);
    assert_eq!(sf.find_by_id_prefix("xyz").len(), 1);
    assert_eq!(sf.find_by_id_prefix("zzz").len(), 0);
}

#[test]
fn test_find_by_short_id_prefix() {
    let tmp = TempDir::new().unwrap();
    let mut sf = StateFile::load(&test_state_path(&tmp)).unwrap();

    // UUID format: "550e8400-e29b-41d4-a716-446655440000"
    // short_id:    "550e8400e29b"
    sf.add(sample_record(
        "550e8400-e29b-41d4-a716-446655440000",
        "box1",
        "created",
    ))
    .unwrap();

    // Search by short_id prefix
    let matches = sf.find_by_id_prefix("550e8400e");
    assert_eq!(matches.len(), 1);
    assert_eq!(matches[0].name, "box1");
}

// --- List filter tests ---

#[test]
fn test_list_filter() {
    let tmp = TempDir::new().unwrap();
    let mut sf = StateFile::load(&test_state_path(&tmp)).unwrap();

    sf.add(sample_record("id1", "box1", "created")).unwrap();
    let mut running = sample_record("id2", "box2", "running");
    running.pid = Some(99999);
    sf.add(running).unwrap();

    let all = sf.list(true);
    assert_eq!(all.len(), 2);
}

#[test]
fn test_list_all_statuses() {
    let tmp = TempDir::new().unwrap();
    let mut sf = StateFile::load(&test_state_path(&tmp)).unwrap();

    sf.add(sample_record("id1", "box1", "created")).unwrap();
    sf.add(sample_record("id2", "box2", "stopped")).unwrap();
    sf.add(sample_record("id3", "box3", "dead")).unwrap();

    // None are "running", so list(false) should return empty
    let running = sf.list(false);
    assert_eq!(running.len(), 0);

    // list(true) returns all
    let all = sf.list(true);
    assert_eq!(all.len(), 3);
}

// --- Persistence tests ---

#[test]
fn test_persistence() {
    let tmp = TempDir::new().unwrap();
    let path = test_state_path(&tmp);

    {
        let mut sf = StateFile::load(&path).unwrap();
        sf.add(sample_record("persist-id", "persist_box", "created"))
            .unwrap();
    }

    {
        let sf = StateFile::load(&path).unwrap();
        assert_eq!(sf.records().len(), 1);
        assert_eq!(sf.find_by_id("persist-id").unwrap().name, "persist_box");
    }
}

#[test]
fn test_persistence_multiple_records() {
    let tmp = TempDir::new().unwrap();
    let path = test_state_path(&tmp);

    {
        let mut sf = StateFile::load(&path).unwrap();
        sf.add(sample_record("id1", "box1", "created")).unwrap();
        sf.add(sample_record("id2", "box2", "stopped")).unwrap();
    }

    {
        let sf = StateFile::load(&path).unwrap();
        assert_eq!(sf.records().len(), 2);

        let rec1 = sf.find_by_id("id1").unwrap();
        assert_eq!(rec1.name, "box1");
        assert_eq!(rec1.status, "created");

        let rec2 = sf.find_by_id("id2").unwrap();
        assert_eq!(rec2.name, "box2");
        assert_eq!(rec2.status, "stopped");
    }
}

#[test]
fn test_persistence_after_remove() {
    let tmp = TempDir::new().unwrap();
    let path = test_state_path(&tmp);

    {
        let mut sf = StateFile::load(&path).unwrap();
        sf.add(sample_record("id1", "box1", "created")).unwrap();
        sf.add(sample_record("id2", "box2", "created")).unwrap();
        sf.remove("id1").unwrap();
    }

    {
        let sf = StateFile::load(&path).unwrap();
        assert_eq!(sf.records().len(), 1);
        assert!(sf.find_by_id("id1").is_none());
        assert!(sf.find_by_id("id2").is_some());
    }
}

// --- Reconcile tests ---

#[test]
fn test_reconcile_marks_dead_pid() {
    let tmp = TempDir::new().unwrap();
    let path = test_state_path(&tmp);

    // Create a state file with a "running" box with a dead PID
    {
        let mut sf = StateFile::load(&path).unwrap();
        let mut record = sample_record("dead-pid-id", "dead_pid_box", "created");
        // Manually set to running with an impossible PID
        record.status = "running".to_string();
        record.pid = Some(4294967); // Very unlikely to be a real process
        sf.records.push(record);
        sf.save().unwrap();
    }

    // Reload — reconcile should mark it as dead
    {
        let sf = StateFile::load(&path).unwrap();
        let record = sf.find_by_id("dead-pid-id").unwrap();
        assert_eq!(record.status, "dead");
        assert!(record.pid.is_none());
    }
}

#[test]
fn test_reconcile_running_without_pid() {
    let tmp = TempDir::new().unwrap();
    let path = test_state_path(&tmp);

    {
        let mut sf = StateFile::load(&path).unwrap();
        let mut record = sample_record("no-pid-id", "no_pid_box", "created");
        record.status = "running".to_string();
        record.pid = None; // Running but no PID
        sf.records.push(record);
        sf.save().unwrap();
    }

    {
        let sf = StateFile::load(&path).unwrap();
        let record = sf.find_by_id("no-pid-id").unwrap();
        assert_eq!(record.status, "dead");
    }
}

#[test]
fn test_reconcile_ignores_non_running() {
    let tmp = TempDir::new().unwrap();
    let path = test_state_path(&tmp);

    {
        let mut sf = StateFile::load(&path).unwrap();
        sf.add(sample_record("created-id", "created_box", "created"))
            .unwrap();
        sf.add(sample_record("stopped-id", "stopped_box", "stopped"))
            .unwrap();
    }

    {
        let sf = StateFile::load(&path).unwrap();
        assert_eq!(sf.find_by_id("created-id").unwrap().status, "created");
        assert_eq!(sf.find_by_id("stopped-id").unwrap().status, "stopped");
    }
}

// --- Atomic save tests ---

#[test]
fn test_atomic_save() {
    let tmp = TempDir::new().unwrap();
    let path = test_state_path(&tmp);

    let mut sf = StateFile::load(&path).unwrap();
    sf.add(sample_record("id1", "box1", "created")).unwrap();

    let tmp_path = path.with_extension("json.tmp");
    assert!(!tmp_path.exists());
    assert!(path.exists());
}

// --- Mutation tests ---

#[test]
fn test_find_by_id_mut() {
    let tmp = TempDir::new().unwrap();
    let mut sf = StateFile::load(&test_state_path(&tmp)).unwrap();

    sf.add(sample_record("mut-id", "mut_box", "created"))
        .unwrap();

    let record = sf.find_by_id_mut("mut-id").unwrap();
    record.status = "running".to_string();
    record.pid = Some(12345);

    assert_eq!(sf.find_by_id("mut-id").unwrap().status, "running");
}

#[test]
fn test_find_by_id_mut_not_found() {
    let tmp = TempDir::new().unwrap();
    let mut sf = StateFile::load(&test_state_path(&tmp)).unwrap();
    assert!(sf.find_by_id_mut("nonexistent").is_none());
}

#[test]
fn test_mutation_persists_after_save() {
    let tmp = TempDir::new().unwrap();
    let path = test_state_path(&tmp);

    {
        let mut sf = StateFile::load(&path).unwrap();
        sf.add(sample_record("mut-save-id", "mut_save", "created"))
            .unwrap();

        let record = sf.find_by_id_mut("mut-save-id").unwrap();
        record.status = "stopped".to_string();
        sf.save().unwrap();
    }

    {
        let sf = StateFile::load(&path).unwrap();
        assert_eq!(sf.find_by_id("mut-save-id").unwrap().status, "stopped");
    }
}

// --- Name generation tests ---

#[test]
fn test_generate_name() {
    let name = generate_name();
    assert!(name.contains('_'));
    let parts: Vec<&str> = name.split('_').collect();
    assert_eq!(parts.len(), 2);
    assert!(!parts[0].is_empty());
    assert!(!parts[1].is_empty());
}

#[test]
fn test_generate_name_uniqueness() {
    // Generate 50 names and check that at least some are unique
    // (with 32*32=1024 combinations, collisions in 50 are unlikely)
    let names: Vec<String> = (0..50).map(|_| generate_name()).collect();
    let unique: std::collections::HashSet<&String> = names.iter().collect();
    assert!(unique.len() > 1, "Expected unique names, got all identical");
}

#[test]
fn test_generate_name_uses_valid_words() {
    let name = generate_name();
    let parts: Vec<&str> = name.split('_').collect();

    assert!(
        policy::ADJECTIVES.contains(&parts[0]),
        "Adjective '{}' not in word list",
        parts[0]
    );
    assert!(
        policy::NOUNS.contains(&parts[1]),
        "Noun '{}' not in word list",
        parts[1]
    );
}

#[test]
fn test_word_lists_not_empty() {
    assert!(!policy::ADJECTIVES.is_empty());
    assert!(!policy::NOUNS.is_empty());
}

// --- Restart policy tests ---

#[test]
fn test_should_restart_no_policy() {
    let mut record = sample_record("id-1", "box1", "dead");
    record.restart_policy = "no".to_string();
    assert!(!policy::should_restart(&record));
}

#[test]
fn test_should_restart_always() {
    let mut record = sample_record("id-1", "box1", "dead");
    record.restart_policy = "always".to_string();
    assert!(policy::should_restart(&record));
}

#[test]
fn test_should_restart_always_even_if_stopped_by_user() {
    let mut record = sample_record("id-1", "box1", "dead");
    record.restart_policy = "always".to_string();
    record.stopped_by_user = true;
    assert!(policy::should_restart(&record));
}

#[test]
fn test_should_restart_on_failure_not_stopped_by_user() {
    let mut record = sample_record("id-1", "box1", "dead");
    record.restart_policy = "on-failure".to_string();
    record.stopped_by_user = false;
    assert!(policy::should_restart(&record));
}

#[test]
fn test_should_restart_on_failure_stopped_by_user() {
    let mut record = sample_record("id-1", "box1", "dead");
    record.restart_policy = "on-failure".to_string();
    record.stopped_by_user = true;
    assert!(!policy::should_restart(&record));
}

#[test]
fn test_should_restart_unless_stopped_not_stopped_by_user() {
    let mut record = sample_record("id-1", "box1", "dead");
    record.restart_policy = "unless-stopped".to_string();
    record.stopped_by_user = false;
    assert!(policy::should_restart(&record));
}

#[test]
fn test_should_restart_unless_stopped_stopped_by_user() {
    let mut record = sample_record("id-1", "box1", "dead");
    record.restart_policy = "unless-stopped".to_string();
    record.stopped_by_user = true;
    assert!(!policy::should_restart(&record));
}

#[test]
fn test_should_restart_unknown_policy() {
    let mut record = sample_record("id-1", "box1", "dead");
    record.restart_policy = "unknown".to_string();
    assert!(!policy::should_restart(&record));
}

#[test]
fn test_pending_restarts_empty() {
    let tmp = TempDir::new().unwrap();
    let sf = StateFile::load(&test_state_path(&tmp)).unwrap();
    assert!(sf.pending_restarts().is_empty());
}

#[test]
fn test_pending_restarts_dead_with_always() {
    let tmp = TempDir::new().unwrap();
    let mut sf = StateFile::load(&test_state_path(&tmp)).unwrap();
    let mut record = sample_record("id-1", "box1", "dead");
    record.restart_policy = "always".to_string();
    sf.add(record).unwrap();

    let restarts = sf.pending_restarts();
    assert_eq!(restarts.len(), 1);
    assert_eq!(restarts[0], "id-1");
}

#[test]
fn test_pending_restarts_dead_with_no_policy() {
    let tmp = TempDir::new().unwrap();
    let mut sf = StateFile::load(&test_state_path(&tmp)).unwrap();
    let mut record = sample_record("id-1", "box1", "dead");
    record.restart_policy = "no".to_string();
    sf.add(record).unwrap();

    assert!(sf.pending_restarts().is_empty());
}

#[test]
fn test_pending_restarts_stopped_not_included() {
    let tmp = TempDir::new().unwrap();
    let mut sf = StateFile::load(&test_state_path(&tmp)).unwrap();
    let mut record = sample_record("id-1", "box1", "stopped");
    record.restart_policy = "always".to_string();
    sf.add(record).unwrap();

    // "stopped" boxes are not pending restart — only "dead" ones
    assert!(sf.pending_restarts().is_empty());
}

// --- Health check config tests ---

#[test]
fn test_health_check_serialization() {
    let hc = HealthCheck {
        cmd: vec![
            "curl".to_string(),
            "-f".to_string(),
            "http://localhost/health".to_string(),
        ],
        interval_secs: 10,
        timeout_secs: 3,
        retries: 5,
        start_period_secs: 15,
    };
    let json = serde_json::to_string(&hc).unwrap();
    let parsed: HealthCheck = serde_json::from_str(&json).unwrap();
    assert_eq!(parsed.cmd, vec!["curl", "-f", "http://localhost/health"]);
    assert_eq!(parsed.interval_secs, 10);
    assert_eq!(parsed.timeout_secs, 3);
    assert_eq!(parsed.retries, 5);
    assert_eq!(parsed.start_period_secs, 15);
}

#[test]
fn test_health_check_defaults() {
    let json = r#"{"cmd":["true"]}"#;
    let parsed: HealthCheck = serde_json::from_str(json).unwrap();
    assert_eq!(parsed.cmd, vec!["true"]);
    assert_eq!(parsed.interval_secs, 30);
    assert_eq!(parsed.timeout_secs, 5);
    assert_eq!(parsed.retries, 3);
    assert_eq!(parsed.start_period_secs, 0);
}

#[test]
fn test_box_record_with_health_check() {
    let mut record = sample_record("id-1", "box1", "running");
    record.health_check = Some(HealthCheck {
        cmd: vec![
            "test".to_string(),
            "-f".to_string(),
            "/tmp/healthy".to_string(),
        ],
        interval_secs: 30,
        timeout_secs: 5,
        retries: 3,
        start_period_secs: 0,
    });
    record.health_status = "healthy".to_string();

    let json = serde_json::to_string(&record).unwrap();
    let parsed: BoxRecord = serde_json::from_str(&json).unwrap();
    assert!(parsed.health_check.is_some());
    assert_eq!(parsed.health_status, "healthy");
}

#[test]
fn test_box_record_backward_compat_no_health() {
    // Old records without health fields should deserialize with defaults
    let record = sample_record("id-1", "box1", "created");
    let json = serde_json::to_string(&record).unwrap();

    // Remove health fields to simulate old format
    let mut val: serde_json::Value = serde_json::from_str(&json).unwrap();
    val.as_object_mut().unwrap().remove("health_check");
    val.as_object_mut().unwrap().remove("health_status");
    val.as_object_mut().unwrap().remove("health_retries");
    val.as_object_mut().unwrap().remove("health_last_check");
    val.as_object_mut().unwrap().remove("stopped_by_user");
    val.as_object_mut().unwrap().remove("restart_count");
    val.as_object_mut().unwrap().remove("max_restart_count");
    val.as_object_mut().unwrap().remove("exit_code");

    let parsed: BoxRecord = serde_json::from_value(val).unwrap();
    assert!(parsed.health_check.is_none());
    assert_eq!(parsed.health_status, "none");
    assert_eq!(parsed.health_retries, 0);
    assert!(parsed.health_last_check.is_none());
    assert!(!parsed.stopped_by_user);
    assert_eq!(parsed.restart_count, 0);
    assert_eq!(parsed.max_restart_count, 0);
    assert!(parsed.exit_code.is_none());
}

// --- Restart policy validation tests ---

#[test]
fn test_validate_restart_policy_valid() {
    assert!(validate_restart_policy("no").is_ok());
    assert!(validate_restart_policy("always").is_ok());
    assert!(validate_restart_policy("on-failure").is_ok());
    assert!(validate_restart_policy("unless-stopped").is_ok());
    assert!(validate_restart_policy("on-failure:5").is_ok());
    assert!(validate_restart_policy("on-failure:0").is_ok());
    assert!(validate_restart_policy("on-failure:100").is_ok());
}

#[test]
fn test_validate_restart_policy_invalid() {
    assert!(validate_restart_policy("").is_err());
    assert!(validate_restart_policy("never").is_err());
    assert!(validate_restart_policy("on-failure:").is_err());
    assert!(validate_restart_policy("on-failure:abc").is_err());
    assert!(validate_restart_policy("on-failure:-1").is_err());
    assert!(validate_restart_policy("on-failure:1.5").is_err());
}

#[test]
fn test_parse_restart_policy_simple() {
    assert_eq!(parse_restart_policy("no").unwrap(), ("no".to_string(), 0));
    assert_eq!(
        parse_restart_policy("always").unwrap(),
        ("always".to_string(), 0)
    );
    assert_eq!(
        parse_restart_policy("on-failure").unwrap(),
        ("on-failure".to_string(), 0)
    );
    assert_eq!(
        parse_restart_policy("unless-stopped").unwrap(),
        ("unless-stopped".to_string(), 0)
    );
}

#[test]
fn test_parse_restart_policy_on_failure_with_max() {
    assert_eq!(
        parse_restart_policy("on-failure:5").unwrap(),
        ("on-failure".to_string(), 5)
    );
    assert_eq!(
        parse_restart_policy("on-failure:0").unwrap(),
        ("on-failure".to_string(), 0)
    );
    assert_eq!(
        parse_restart_policy("on-failure:100").unwrap(),
        ("on-failure".to_string(), 100)
    );
}

#[test]
fn test_parse_restart_policy_invalid() {
    assert!(parse_restart_policy("invalid").is_err());
    assert!(parse_restart_policy("on-failure:abc").is_err());
}

// --- Enhanced should_restart tests ---

#[test]
fn test_should_restart_on_failure_with_max_under_limit() {
    let mut record = sample_record("id-1", "box1", "dead");
    record.restart_policy = "on-failure:5".to_string();
    record.restart_count = 3;
    assert!(policy::should_restart(&record));
}

#[test]
fn test_should_restart_on_failure_with_max_at_limit() {
    let mut record = sample_record("id-1", "box1", "dead");
    record.restart_policy = "on-failure:5".to_string();
    record.restart_count = 5;
    assert!(!policy::should_restart(&record));
}

#[test]
fn test_should_restart_on_failure_with_max_over_limit() {
    let mut record = sample_record("id-1", "box1", "dead");
    record.restart_policy = "on-failure:3".to_string();
    record.restart_count = 10;
    assert!(!policy::should_restart(&record));
}

#[test]
fn test_should_restart_on_failure_with_max_stopped_by_user() {
    let mut record = sample_record("id-1", "box1", "dead");
    record.restart_policy = "on-failure:5".to_string();
    record.restart_count = 0;
    record.stopped_by_user = true;
    assert!(!policy::should_restart(&record));
}

#[test]
fn test_should_restart_on_failure_with_max_restart_count_field() {
    // Test the max_restart_count field on plain "on-failure" policy
    let mut record = sample_record("id-1", "box1", "dead");
    record.restart_policy = "on-failure".to_string();
    record.max_restart_count = 3;
    record.restart_count = 3;
    assert!(!policy::should_restart(&record));
}

#[test]
fn test_should_restart_on_failure_max_restart_count_zero_means_unlimited() {
    let mut record = sample_record("id-1", "box1", "dead");
    record.restart_policy = "on-failure".to_string();
    record.max_restart_count = 0;
    record.restart_count = 100;
    assert!(policy::should_restart(&record));
}

#[test]
fn test_should_restart_malformed_on_failure_colon() {
    let mut record = sample_record("id-1", "box1", "dead");
    record.restart_policy = "on-failure:abc".to_string();
    assert!(!policy::should_restart(&record));
}

#[test]
fn test_pending_restarts_respects_max_count() {
    let tmp = TempDir::new().unwrap();
    let mut sf = StateFile::load(&test_state_path(&tmp)).unwrap();

    let mut record = sample_record("id-1", "box1", "dead");
    record.restart_policy = "on-failure:2".to_string();
    record.restart_count = 2;
    sf.add(record).unwrap();

    // At limit — should NOT be pending
    assert!(sf.pending_restarts().is_empty());
}

#[test]
fn test_pending_restarts_under_max_count() {
    let tmp = TempDir::new().unwrap();
    let mut sf = StateFile::load(&test_state_path(&tmp)).unwrap();

    let mut record = sample_record("id-1", "box1", "dead");
    record.restart_policy = "on-failure:5".to_string();
    record.restart_count = 2;
    sf.add(record).unwrap();

    // Under limit — should be pending
    assert_eq!(sf.pending_restarts().len(), 1);
}

// --- Exit code field tests ---

#[test]
fn test_box_record_exit_code_serialization() {
    let mut record = sample_record("id-1", "box1", "dead");
    record.exit_code = Some(137);

    let json = serde_json::to_string(&record).unwrap();
    let parsed: BoxRecord = serde_json::from_str(&json).unwrap();
    assert_eq!(parsed.exit_code, Some(137));
}

#[test]
fn test_box_record_exit_code_none() {
    let record = sample_record("id-1", "box1", "running");
    assert!(record.exit_code.is_none());

    let json = serde_json::to_string(&record).unwrap();
    let parsed: BoxRecord = serde_json::from_str(&json).unwrap();
    assert!(parsed.exit_code.is_none());
}

#[test]
fn test_find_by_label() {
    let tmp = TempDir::new().unwrap();
    let path = test_state_path(&tmp);
    let mut state = file::StateFile::load(&path).unwrap();

    let mut r1 = sample_record("id-1", "web", "running");
    r1.labels
        .insert("com.a3s.compose.project".to_string(), "myapp".to_string());
    r1.labels
        .insert("com.a3s.compose.service".to_string(), "web".to_string());

    let mut r2 = sample_record("id-2", "db", "running");
    r2.labels
        .insert("com.a3s.compose.project".to_string(), "myapp".to_string());
    r2.labels
        .insert("com.a3s.compose.service".to_string(), "db".to_string());

    let r3 = sample_record("id-3", "other", "running");

    state.add(r1).unwrap();
    state.add(r2).unwrap();
    state.add(r3).unwrap();

    let results = state.find_by_label("com.a3s.compose.project", "myapp");
    assert_eq!(results.len(), 2);

    let results = state.find_by_label("com.a3s.compose.project", "other");
    assert!(results.is_empty());

    let results = state.find_by_label("com.a3s.compose.service", "web");
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].name, "web");
}
