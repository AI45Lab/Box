//! Docker-compatible name/ID resolution for box instances.
//!
//! Resolution order: exact name -> exact ID -> ID prefix (must be unique).

use crate::state::{BoxRecord, StateFile};

/// Resolve a query string to a single box record.
///
/// Matches in order:
/// 1. Exact name match
/// 2. Exact ID match
/// 3. Unique ID prefix match (on full ID or short ID)
pub fn resolve<'a>(state: &'a StateFile, query: &str) -> Result<&'a BoxRecord, ResolveError> {
    // 1. Exact name
    if let Some(record) = state.find_by_name(query) {
        return Ok(record);
    }

    // 2. Exact ID
    if let Some(record) = state.find_by_id(query) {
        return Ok(record);
    }

    // 3. ID prefix
    let matches = state.find_by_id_prefix(query);
    match matches.len() {
        0 => Err(ResolveError::NotFound(query.to_string())),
        1 => Ok(matches[0]),
        n => Err(ResolveError::Ambiguous {
            query: query.to_string(),
            count: n,
        }),
    }
}

/// Resolve a query string to a mutable box record.
pub fn resolve_mut<'a>(
    state: &'a mut StateFile,
    query: &str,
) -> Result<&'a mut BoxRecord, ResolveError> {
    // Find the ID first (immutable borrow)
    let id = {
        let record = resolve_immutable_lookup(state, query)?;
        record.id.clone()
    };

    // Now get the mutable reference
    state
        .find_by_id_mut(&id)
        .ok_or_else(|| ResolveError::NotFound(query.to_string()))
}

/// Helper for immutable lookup phase of resolve_mut.
fn resolve_immutable_lookup<'a>(
    state: &'a StateFile,
    query: &str,
) -> Result<&'a BoxRecord, ResolveError> {
    resolve(state, query)
}

/// Resolution errors.
#[derive(Debug, thiserror::Error)]
pub enum ResolveError {
    #[error("No such box: {0}")]
    NotFound(String),

    #[error("Ambiguous box reference \"{query}\" — matches {count} boxes")]
    Ambiguous { query: String, count: usize },
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
            image: "test:latest".to_string(),
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

    // --- Immutable resolve tests ---

    #[test]
    fn test_resolve_by_name() {
        let (_tmp, state) = setup_state(vec![make_record("abc-123", "my_box")]);
        let result = resolve(&state, "my_box").unwrap();
        assert_eq!(result.id, "abc-123");
    }

    #[test]
    fn test_resolve_by_exact_id() {
        let (_tmp, state) = setup_state(vec![make_record("abc-123", "my_box")]);
        let result = resolve(&state, "abc-123").unwrap();
        assert_eq!(result.name, "my_box");
    }

    #[test]
    fn test_resolve_by_id_prefix() {
        let (_tmp, state) = setup_state(vec![
            make_record("abc-123-xyz", "box1"),
            make_record("def-456-xyz", "box2"),
        ]);
        let result = resolve(&state, "abc").unwrap();
        assert_eq!(result.name, "box1");
    }

    #[test]
    fn test_resolve_ambiguous() {
        let (_tmp, state) = setup_state(vec![
            make_record("abc-123", "box1"),
            make_record("abc-456", "box2"),
        ]);
        let err = resolve(&state, "abc").unwrap_err();
        assert!(matches!(err, ResolveError::Ambiguous { .. }));
    }

    #[test]
    fn test_resolve_not_found() {
        let (_tmp, state) = setup_state(vec![make_record("abc-123", "my_box")]);
        let err = resolve(&state, "nonexistent").unwrap_err();
        assert!(matches!(err, ResolveError::NotFound(_)));
    }

    #[test]
    fn test_resolve_name_takes_priority() {
        let (_tmp, state) = setup_state(vec![
            make_record("abc-123", "abc"),
            make_record("abc-456", "other"),
        ]);
        let result = resolve(&state, "abc").unwrap();
        assert_eq!(result.id, "abc-123");
    }

    #[test]
    fn test_resolve_empty_state() {
        let (_tmp, state) = setup_state(vec![]);
        let err = resolve(&state, "anything").unwrap_err();
        assert!(matches!(err, ResolveError::NotFound(_)));
    }

    #[test]
    fn test_resolve_by_short_id_prefix() {
        let (_tmp, state) = setup_state(vec![make_record(
            "550e8400-e29b-41d4-a716-446655440000",
            "box1",
        )]);
        // short_id is "550e8400e29b" — match by its prefix
        let result = resolve(&state, "550e84").unwrap();
        assert_eq!(result.name, "box1");
    }

    #[test]
    fn test_resolve_id_exact_over_prefix() {
        // If exact ID match exists, it should win even if another record's prefix matches
        let (_tmp, state) = setup_state(vec![
            make_record("abc", "box_exact"),
            make_record("abc-456", "box_prefix"),
        ]);
        let result = resolve(&state, "abc").unwrap();
        // "abc" matches name of neither, but exact ID of first
        // Actually name resolution happens first. box_exact is not named "abc".
        // Wait — make_record gives name "box_exact", id "abc".
        // resolve("abc") → find_by_name("abc") = None → find_by_id("abc") = Some(box_exact)
        assert_eq!(result.name, "box_exact");
    }

    // --- Mutable resolve tests ---

    #[test]
    fn test_resolve_mut_by_name() {
        let (_tmp, mut state) = setup_state(vec![make_record("id-1", "my_box")]);
        let record = resolve_mut(&mut state, "my_box").unwrap();
        assert_eq!(record.id, "id-1");

        // Mutate and verify
        record.status = "stopped".to_string();
        assert_eq!(state.find_by_id("id-1").unwrap().status, "stopped");
    }

    #[test]
    fn test_resolve_mut_by_id() {
        let (_tmp, mut state) = setup_state(vec![make_record("id-1", "my_box")]);
        let record = resolve_mut(&mut state, "id-1").unwrap();
        record.cpus = 8;
        assert_eq!(state.find_by_id("id-1").unwrap().cpus, 8);
    }

    #[test]
    fn test_resolve_mut_not_found() {
        let (_tmp, mut state) = setup_state(vec![make_record("id-1", "my_box")]);
        let err = resolve_mut(&mut state, "nonexistent").unwrap_err();
        assert!(matches!(err, ResolveError::NotFound(_)));
    }

    #[test]
    fn test_resolve_mut_ambiguous() {
        let (_tmp, mut state) = setup_state(vec![
            make_record("abc-123", "box1"),
            make_record("abc-456", "box2"),
        ]);
        let err = resolve_mut(&mut state, "abc").unwrap_err();
        assert!(matches!(err, ResolveError::Ambiguous { .. }));
    }

    // --- Error display tests ---

    #[test]
    fn test_not_found_error_display() {
        let err = ResolveError::NotFound("my_box".to_string());
        assert_eq!(err.to_string(), "No such box: my_box");
    }

    #[test]
    fn test_ambiguous_error_display() {
        let err = ResolveError::Ambiguous {
            query: "abc".to_string(),
            count: 3,
        };
        let msg = err.to_string();
        assert!(msg.contains("abc"));
        assert!(msg.contains("3"));
    }
}
