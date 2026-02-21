//! Persistent state store for CRI sandbox and container records.
//!
//! Persists state to disk so CRI server restarts do not orphan running VMs.
//! Writes are atomic: data is written to a `.tmp` file then renamed into place.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::container::Container;
use crate::sandbox::PodSandbox;

/// All CRI state serialized to disk.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct PersistedState {
    pub sandboxes: Vec<PodSandbox>,
    pub containers: Vec<Container>,
}

/// Trait for persisting and loading CRI state.
pub trait StateStore: Send + Sync {
    /// Persist the full current state atomically.
    fn save(&self, state: &PersistedState) -> std::io::Result<()>;

    /// Load previously persisted state. Returns empty state if none exists.
    fn load(&self) -> std::io::Result<PersistedState>;
}

/// JSON file-backed state store.
///
/// Writes are atomic: serialized JSON is written to `<path>.tmp` then
/// renamed to `<path>`, so a crash mid-write never corrupts the last
/// good snapshot.
pub struct JsonStateStore {
    path: PathBuf,
}

impl JsonStateStore {
    /// Create a store backed by `path`.
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }
}

impl StateStore for JsonStateStore {
    fn save(&self, state: &PersistedState) -> std::io::Result<()> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let tmp = self.path.with_extension("tmp");
        let json = serde_json::to_vec_pretty(state)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;

        std::fs::write(&tmp, &json)?;
        std::fs::rename(&tmp, &self.path)?;
        Ok(())
    }

    fn load(&self) -> std::io::Result<PersistedState> {
        if !self.path.exists() {
            return Ok(PersistedState::default());
        }

        let bytes = std::fs::read(&self.path)?;
        serde_json::from_slice(&bytes)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
    }
}

/// No-op state store for testing (in-memory only, never touches disk).
pub struct NoopStateStore;

impl StateStore for NoopStateStore {
    fn save(&self, _state: &PersistedState) -> std::io::Result<()> {
        Ok(())
    }

    fn load(&self) -> std::io::Result<PersistedState> {
        Ok(PersistedState::default())
    }
}

/// Resolve the default state file path: `~/.a3s/cri/state.json`.
pub fn default_state_path() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join(".a3s")
        .join("cri")
        .join("state.json")
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;
    use crate::container::{Container, ContainerState};
    use crate::sandbox::{PodSandbox, SandboxState};

    fn sample_sandbox(id: &str) -> PodSandbox {
        PodSandbox {
            id: id.to_string(),
            name: format!("pod-{}", id),
            namespace: "default".to_string(),
            uid: format!("uid-{}", id),
            state: SandboxState::Ready,
            created_at: 1_000_000_000,
            labels: HashMap::from([("app".to_string(), "test".to_string())]),
            annotations: HashMap::new(),
            log_directory: "/var/log/pods".to_string(),
            runtime_handler: "a3s".to_string(),
        }
    }

    fn sample_container(id: &str, sandbox_id: &str) -> Container {
        Container {
            id: id.to_string(),
            sandbox_id: sandbox_id.to_string(),
            name: format!("container-{}", id),
            image_ref: "alpine:latest".to_string(),
            state: ContainerState::Running,
            created_at: 1_000_000_000,
            started_at: 2_000_000_000,
            finished_at: 0,
            exit_code: 0,
            labels: HashMap::new(),
            annotations: HashMap::new(),
            log_path: String::new(),
        }
    }

    #[test]
    fn test_noop_store_save_load() {
        let store = NoopStateStore;
        let state = PersistedState {
            sandboxes: vec![sample_sandbox("sb1")],
            containers: vec![sample_container("c1", "sb1")],
        };
        store.save(&state).unwrap();
        let loaded = store.load().unwrap();
        // Noop always returns empty
        assert!(loaded.sandboxes.is_empty());
        assert!(loaded.containers.is_empty());
    }

    #[test]
    fn test_json_store_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        let store = JsonStateStore::new(&path);

        let state = PersistedState {
            sandboxes: vec![sample_sandbox("sb1"), sample_sandbox("sb2")],
            containers: vec![
                sample_container("c1", "sb1"),
                sample_container("c2", "sb1"),
                sample_container("c3", "sb2"),
            ],
        };

        store.save(&state).unwrap();
        assert!(path.exists());

        let loaded = store.load().unwrap();
        assert_eq!(loaded.sandboxes.len(), 2);
        assert_eq!(loaded.containers.len(), 3);

        let sb = loaded.sandboxes.iter().find(|s| s.id == "sb1").unwrap();
        assert_eq!(sb.name, "pod-sb1");
        assert_eq!(sb.state, SandboxState::Ready);

        let c = loaded.containers.iter().find(|c| c.id == "c3").unwrap();
        assert_eq!(c.sandbox_id, "sb2");
        assert_eq!(c.state, ContainerState::Running);
    }

    #[test]
    fn test_json_store_load_missing_file() {
        let dir = tempfile::tempdir().unwrap();
        let store = JsonStateStore::new(dir.path().join("nonexistent.json"));
        let loaded = store.load().unwrap();
        assert!(loaded.sandboxes.is_empty());
        assert!(loaded.containers.is_empty());
    }

    #[test]
    fn test_json_store_atomic_write() {
        // Verify no .tmp file is left after a successful save
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        let store = JsonStateStore::new(&path);

        store
            .save(&PersistedState {
                sandboxes: vec![sample_sandbox("sb1")],
                containers: vec![],
            })
            .unwrap();

        let tmp = path.with_extension("tmp");
        assert!(
            !tmp.exists(),
            ".tmp file should not exist after successful save"
        );
        assert!(path.exists());
    }

    #[test]
    fn test_json_store_overwrite() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        let store = JsonStateStore::new(&path);

        // Save initial state
        store
            .save(&PersistedState {
                sandboxes: vec![sample_sandbox("sb1")],
                containers: vec![],
            })
            .unwrap();

        // Overwrite with new state
        store
            .save(&PersistedState {
                sandboxes: vec![sample_sandbox("sb2"), sample_sandbox("sb3")],
                containers: vec![sample_container("c1", "sb2")],
            })
            .unwrap();

        let loaded = store.load().unwrap();
        assert_eq!(loaded.sandboxes.len(), 2);
        assert_eq!(loaded.containers.len(), 1);
        assert!(loaded.sandboxes.iter().all(|s| s.id != "sb1"));
    }

    #[test]
    fn test_json_store_creates_parent_dirs() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("dirs").join("state.json");
        let store = JsonStateStore::new(&path);

        store.save(&PersistedState::default()).unwrap();

        assert!(path.exists());
    }

    #[test]
    fn test_default_state_path_is_absolute() {
        let path = default_state_path();
        assert!(path.is_absolute());
        assert!(path.to_string_lossy().contains(".a3s"));
    }
}
