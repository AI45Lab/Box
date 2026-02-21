//! Container state management.
//!
//! Maps CRI Container to sessions within an A3S Box microVM.

use std::collections::HashMap;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;

/// Container lifecycle state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ContainerState {
    /// Container has been created but not started.
    Created,
    /// Container is running.
    Running,
    /// Container has exited.
    Exited,
}

/// Represents a container (session) within a pod sandbox (Box).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Container {
    /// Unique container identifier.
    pub id: String,
    /// Parent sandbox identifier.
    pub sandbox_id: String,
    /// Container name.
    pub name: String,
    /// Image reference used to create this container.
    pub image_ref: String,
    /// Current state.
    pub state: ContainerState,
    /// Creation timestamp in nanoseconds.
    pub created_at: i64,
    /// Start timestamp in nanoseconds (0 if not started).
    pub started_at: i64,
    /// Finish timestamp in nanoseconds (0 if not finished).
    pub finished_at: i64,
    /// Exit code (0 if not exited).
    pub exit_code: i32,
    /// Container labels.
    pub labels: HashMap<String, String>,
    /// Container annotations.
    pub annotations: HashMap<String, String>,
    /// Log file path.
    pub log_path: String,
}

/// In-memory store for containers.
pub struct ContainerStore {
    containers: Arc<RwLock<HashMap<String, Container>>>,
}

impl ContainerStore {
    /// Create a new empty container store.
    pub fn new() -> Self {
        Self {
            containers: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Add a container to the store.
    pub async fn add(&self, container: Container) {
        let mut store = self.containers.write().await;
        store.insert(container.id.clone(), container);
    }

    /// Get a container by ID.
    pub async fn get(&self, id: &str) -> Option<Container> {
        let store = self.containers.read().await;
        store.get(id).cloned()
    }

    /// Remove a container by ID.
    pub async fn remove(&self, id: &str) -> Option<Container> {
        let mut store = self.containers.write().await;
        store.remove(id)
    }

    /// List containers, optionally filtered by sandbox ID and/or labels.
    pub async fn list(
        &self,
        sandbox_id: Option<&str>,
        label_filter: Option<&HashMap<String, String>>,
    ) -> Vec<Container> {
        let store = self.containers.read().await;
        store
            .values()
            .filter(|c| {
                if let Some(sid) = sandbox_id {
                    if c.sandbox_id != sid {
                        return false;
                    }
                }
                if let Some(filter) = label_filter {
                    if !filter.iter().all(|(k, v)| c.labels.get(k) == Some(v)) {
                        return false;
                    }
                }
                true
            })
            .cloned()
            .collect()
    }

    /// Update the state of a container.
    pub async fn update_state(&self, id: &str, state: ContainerState) -> bool {
        let mut store = self.containers.write().await;
        if let Some(c) = store.get_mut(id) {
            c.state = state;
            true
        } else {
            false
        }
    }

    /// Update container timestamps when started.
    pub async fn mark_started(&self, id: &str, started_at: i64) -> bool {
        let mut store = self.containers.write().await;
        if let Some(c) = store.get_mut(id) {
            c.state = ContainerState::Running;
            c.started_at = started_at;
            true
        } else {
            false
        }
    }

    /// Update container timestamps and exit code when exited.
    pub async fn mark_exited(&self, id: &str, finished_at: i64, exit_code: i32) -> bool {
        let mut store = self.containers.write().await;
        if let Some(c) = store.get_mut(id) {
            c.state = ContainerState::Exited;
            c.finished_at = finished_at;
            c.exit_code = exit_code;
            true
        } else {
            false
        }
    }

    /// Remove all containers belonging to a sandbox.
    pub async fn remove_by_sandbox(&self, sandbox_id: &str) -> Vec<Container> {
        let mut store = self.containers.write().await;
        let ids: Vec<String> = store
            .values()
            .filter(|c| c.sandbox_id == sandbox_id)
            .map(|c| c.id.clone())
            .collect();

        ids.iter().filter_map(|id| store.remove(id)).collect()
    }
}

impl Default for ContainerStore {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_container(id: &str, sandbox_id: &str) -> Container {
        Container {
            id: id.to_string(),
            sandbox_id: sandbox_id.to_string(),
            name: format!("container-{}", id),
            image_ref: "nginx:latest".to_string(),
            state: ContainerState::Created,
            created_at: 1000000000,
            started_at: 0,
            finished_at: 0,
            exit_code: 0,
            labels: HashMap::from([("app".to_string(), "test".to_string())]),
            annotations: HashMap::new(),
            log_path: format!("/var/log/pods/{}.log", id),
        }
    }

    #[tokio::test]
    async fn test_add_and_get() {
        let store = ContainerStore::new();
        store.add(test_container("c1", "sb1")).await;

        let c = store.get("c1").await.unwrap();
        assert_eq!(c.name, "container-c1");
        assert_eq!(c.state, ContainerState::Created);
    }

    #[tokio::test]
    async fn test_get_nonexistent() {
        let store = ContainerStore::new();
        assert!(store.get("missing").await.is_none());
    }

    #[tokio::test]
    async fn test_remove() {
        let store = ContainerStore::new();
        store.add(test_container("c1", "sb1")).await;

        let removed = store.remove("c1").await;
        assert!(removed.is_some());
        assert!(store.get("c1").await.is_none());
    }

    #[tokio::test]
    async fn test_list_by_sandbox() {
        let store = ContainerStore::new();
        store.add(test_container("c1", "sb1")).await;
        store.add(test_container("c2", "sb1")).await;
        store.add(test_container("c3", "sb2")).await;

        let sb1_containers = store.list(Some("sb1"), None).await;
        assert_eq!(sb1_containers.len(), 2);

        let sb2_containers = store.list(Some("sb2"), None).await;
        assert_eq!(sb2_containers.len(), 1);
    }

    #[tokio::test]
    async fn test_list_with_label_filter() {
        let store = ContainerStore::new();
        store.add(test_container("c1", "sb1")).await;

        let mut c2 = test_container("c2", "sb1");
        c2.labels.insert("app".to_string(), "other".to_string());
        store.add(c2).await;

        let filter = HashMap::from([("app".to_string(), "test".to_string())]);
        let filtered = store.list(None, Some(&filter)).await;
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].id, "c1");
    }

    #[tokio::test]
    async fn test_mark_started() {
        let store = ContainerStore::new();
        store.add(test_container("c1", "sb1")).await;

        assert!(store.mark_started("c1", 2000000000).await);
        let c = store.get("c1").await.unwrap();
        assert_eq!(c.state, ContainerState::Running);
        assert_eq!(c.started_at, 2000000000);
    }

    #[tokio::test]
    async fn test_mark_exited() {
        let store = ContainerStore::new();
        store.add(test_container("c1", "sb1")).await;
        store.mark_started("c1", 2000000000).await;

        assert!(store.mark_exited("c1", 3000000000, 0).await);
        let c = store.get("c1").await.unwrap();
        assert_eq!(c.state, ContainerState::Exited);
        assert_eq!(c.finished_at, 3000000000);
        assert_eq!(c.exit_code, 0);
    }

    #[tokio::test]
    async fn test_remove_by_sandbox() {
        let store = ContainerStore::new();
        store.add(test_container("c1", "sb1")).await;
        store.add(test_container("c2", "sb1")).await;
        store.add(test_container("c3", "sb2")).await;

        let removed = store.remove_by_sandbox("sb1").await;
        assert_eq!(removed.len(), 2);
        assert!(store.get("c1").await.is_none());
        assert!(store.get("c2").await.is_none());
        assert!(store.get("c3").await.is_some());
    }
}
