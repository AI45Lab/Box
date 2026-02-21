//! Pod sandbox state management.
//!
//! Maps CRI PodSandbox to A3S Box instances (one microVM per pod).

use std::collections::HashMap;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;

/// Sandbox lifecycle state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SandboxState {
    /// Sandbox is running and ready.
    Ready,
    /// Sandbox is not running.
    NotReady,
    /// Sandbox has been removed.
    Removed,
}

/// Represents a pod sandbox backed by a Box microVM.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PodSandbox {
    /// Unique sandbox identifier.
    pub id: String,
    /// Pod name.
    pub name: String,
    /// Kubernetes namespace.
    pub namespace: String,
    /// Pod UID.
    pub uid: String,
    /// Current state.
    pub state: SandboxState,
    /// Creation timestamp in nanoseconds.
    pub created_at: i64,
    /// Pod labels.
    pub labels: HashMap<String, String>,
    /// Pod annotations.
    pub annotations: HashMap<String, String>,
    /// Log directory path.
    pub log_directory: String,
    /// Runtime handler name.
    pub runtime_handler: String,
}

/// In-memory store for pod sandboxes.
pub struct SandboxStore {
    sandboxes: Arc<RwLock<HashMap<String, PodSandbox>>>,
}

impl SandboxStore {
    /// Create a new empty sandbox store.
    pub fn new() -> Self {
        Self {
            sandboxes: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Add a sandbox to the store.
    pub async fn add(&self, sandbox: PodSandbox) {
        let mut store = self.sandboxes.write().await;
        store.insert(sandbox.id.clone(), sandbox);
    }

    /// Get a sandbox by ID.
    pub async fn get(&self, id: &str) -> Option<PodSandbox> {
        let store = self.sandboxes.read().await;
        store.get(id).cloned()
    }

    /// Remove a sandbox by ID.
    pub async fn remove(&self, id: &str) -> Option<PodSandbox> {
        let mut store = self.sandboxes.write().await;
        store.remove(id)
    }

    /// List sandboxes, optionally filtered by labels.
    pub async fn list(&self, label_filter: Option<&HashMap<String, String>>) -> Vec<PodSandbox> {
        let store = self.sandboxes.read().await;
        store
            .values()
            .filter(|sb| {
                if let Some(filter) = label_filter {
                    filter.iter().all(|(k, v)| sb.labels.get(k) == Some(v))
                } else {
                    true
                }
            })
            .cloned()
            .collect()
    }

    /// Update the state of a sandbox.
    pub async fn update_state(&self, id: &str, state: SandboxState) -> bool {
        let mut store = self.sandboxes.write().await;
        if let Some(sb) = store.get_mut(id) {
            sb.state = state;
            true
        } else {
            false
        }
    }
}

impl Default for SandboxStore {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_sandbox(id: &str) -> PodSandbox {
        PodSandbox {
            id: id.to_string(),
            name: format!("pod-{}", id),
            namespace: "default".to_string(),
            uid: format!("uid-{}", id),
            state: SandboxState::Ready,
            created_at: 1000000000,
            labels: HashMap::from([("app".to_string(), "test".to_string())]),
            annotations: HashMap::new(),
            log_directory: "/var/log/pods".to_string(),
            runtime_handler: "a3s".to_string(),
        }
    }

    #[tokio::test]
    async fn test_add_and_get() {
        let store = SandboxStore::new();
        store.add(test_sandbox("sb1")).await;

        let sb = store.get("sb1").await.unwrap();
        assert_eq!(sb.name, "pod-sb1");
        assert_eq!(sb.state, SandboxState::Ready);
    }

    #[tokio::test]
    async fn test_get_nonexistent() {
        let store = SandboxStore::new();
        assert!(store.get("missing").await.is_none());
    }

    #[tokio::test]
    async fn test_remove() {
        let store = SandboxStore::new();
        store.add(test_sandbox("sb1")).await;

        let removed = store.remove("sb1").await;
        assert!(removed.is_some());
        assert!(store.get("sb1").await.is_none());
    }

    #[tokio::test]
    async fn test_list_all() {
        let store = SandboxStore::new();
        store.add(test_sandbox("sb1")).await;
        store.add(test_sandbox("sb2")).await;

        let all = store.list(None).await;
        assert_eq!(all.len(), 2);
    }

    #[tokio::test]
    async fn test_list_with_label_filter() {
        let store = SandboxStore::new();
        store.add(test_sandbox("sb1")).await;

        let mut sb2 = test_sandbox("sb2");
        sb2.labels.insert("app".to_string(), "other".to_string());
        store.add(sb2).await;

        let filter = HashMap::from([("app".to_string(), "test".to_string())]);
        let filtered = store.list(Some(&filter)).await;
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].id, "sb1");
    }

    #[tokio::test]
    async fn test_update_state() {
        let store = SandboxStore::new();
        store.add(test_sandbox("sb1")).await;

        assert!(store.update_state("sb1", SandboxState::NotReady).await);
        let sb = store.get("sb1").await.unwrap();
        assert_eq!(sb.state, SandboxState::NotReady);
    }

    #[tokio::test]
    async fn test_update_state_nonexistent() {
        let store = SandboxStore::new();
        assert!(!store.update_state("missing", SandboxState::NotReady).await);
    }
}
