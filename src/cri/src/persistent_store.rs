//! Unified persistent store for CRI sandbox and container state.
//!
//! Wraps SandboxStore and ContainerStore together so every mutation
//! can atomically snapshot both to disk via a StateStore.

use std::collections::HashMap;
use std::sync::Arc;

use crate::container::{Container, ContainerState, ContainerStore};
use crate::sandbox::{PodSandbox, SandboxState, SandboxStore};
use crate::state::{PersistedState, StateStore};

/// Unified store that persists sandbox + container state after every mutation.
pub struct PersistentCriStore {
    pub sandboxes: Arc<SandboxStore>,
    pub containers: Arc<ContainerStore>,
    state_store: Arc<dyn StateStore>,
}

impl PersistentCriStore {
    /// Create a new store backed by the given StateStore.
    pub fn new(state_store: Arc<dyn StateStore>) -> Self {
        Self {
            sandboxes: Arc::new(SandboxStore::new()),
            containers: Arc::new(ContainerStore::new()),
            state_store,
        }
    }

    /// Load previously persisted state and populate in-memory stores.
    pub async fn load(&self) -> std::io::Result<()> {
        let persisted = self.state_store.load()?;
        for sb in persisted.sandboxes {
            self.sandboxes.add(sb).await;
        }
        for c in persisted.containers {
            self.containers.add(c).await;
        }
        Ok(())
    }

    /// Snapshot current in-memory state to disk.
    async fn persist(&self) -> std::io::Result<()> {
        let state = PersistedState {
            sandboxes: self.sandboxes.list(None).await,
            containers: self.containers.list(None, None).await,
        };
        self.state_store.save(&state)
    }

    // ── Sandbox mutations (persist after each) ────────────────────────

    pub async fn add_sandbox(&self, sandbox: PodSandbox) {
        self.sandboxes.add(sandbox).await;
        if let Err(e) = self.persist().await {
            tracing::warn!(error = %e, "Failed to persist CRI state after add_sandbox");
        }
    }

    pub async fn update_sandbox_state(&self, id: &str, state: SandboxState) -> bool {
        let updated = self.sandboxes.update_state(id, state).await;
        if updated {
            if let Err(e) = self.persist().await {
                tracing::warn!(error = %e, "Failed to persist CRI state after update_sandbox_state");
            }
        }
        updated
    }

    pub async fn remove_sandbox(&self, id: &str) -> Option<PodSandbox> {
        let removed = self.sandboxes.remove(id).await;
        if removed.is_some() {
            if let Err(e) = self.persist().await {
                tracing::warn!(error = %e, "Failed to persist CRI state after remove_sandbox");
            }
        }
        removed
    }

    // ── Container mutations (persist after each) ──────────────────────

    pub async fn add_container(&self, container: Container) {
        self.containers.add(container).await;
        if let Err(e) = self.persist().await {
            tracing::warn!(error = %e, "Failed to persist CRI state after add_container");
        }
    }

    pub async fn mark_container_started(&self, id: &str, started_at: i64) -> bool {
        let updated = self.containers.mark_started(id, started_at).await;
        if updated {
            if let Err(e) = self.persist().await {
                tracing::warn!(error = %e, "Failed to persist CRI state after mark_container_started");
            }
        }
        updated
    }

    pub async fn mark_container_exited(&self, id: &str, finished_at: i64, exit_code: i32) -> bool {
        let updated = self
            .containers
            .mark_exited(id, finished_at, exit_code)
            .await;
        if updated {
            if let Err(e) = self.persist().await {
                tracing::warn!(error = %e, "Failed to persist CRI state after mark_container_exited");
            }
        }
        updated
    }

    pub async fn remove_container(&self, id: &str) -> Option<Container> {
        let removed = self.containers.remove(id).await;
        if removed.is_some() {
            if let Err(e) = self.persist().await {
                tracing::warn!(error = %e, "Failed to persist CRI state after remove_container");
            }
        }
        removed
    }

    pub async fn remove_containers_by_sandbox(&self, sandbox_id: &str) -> Vec<Container> {
        let removed = self.containers.remove_by_sandbox(sandbox_id).await;
        if !removed.is_empty() {
            if let Err(e) = self.persist().await {
                tracing::warn!(error = %e, "Failed to persist CRI state after remove_containers_by_sandbox");
            }
        }
        removed
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{JsonStateStore, NoopStateStore};

    fn sample_sandbox(id: &str) -> PodSandbox {
        PodSandbox {
            id: id.to_string(),
            name: format!("pod-{}", id),
            namespace: "default".to_string(),
            uid: format!("uid-{}", id),
            state: SandboxState::Ready,
            created_at: 1_000_000_000,
            labels: HashMap::new(),
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

    #[tokio::test]
    async fn test_add_sandbox_persists() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        let store = PersistentCriStore::new(Arc::new(JsonStateStore::new(&path)));

        store.add_sandbox(sample_sandbox("sb1")).await;

        // Reload from disk into a fresh store
        let store2 = PersistentCriStore::new(Arc::new(JsonStateStore::new(&path)));
        store2.load().await.unwrap();

        let sb = store2.sandboxes.get("sb1").await.unwrap();
        assert_eq!(sb.name, "pod-sb1");
        assert_eq!(sb.state, SandboxState::Ready);
    }

    #[tokio::test]
    async fn test_add_container_persists() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        let store = PersistentCriStore::new(Arc::new(JsonStateStore::new(&path)));

        store.add_sandbox(sample_sandbox("sb1")).await;
        store.add_container(sample_container("c1", "sb1")).await;

        let store2 = PersistentCriStore::new(Arc::new(JsonStateStore::new(&path)));
        store2.load().await.unwrap();

        let c = store2.containers.get("c1").await.unwrap();
        assert_eq!(c.sandbox_id, "sb1");
        assert_eq!(c.state, ContainerState::Running);
    }

    #[tokio::test]
    async fn test_update_sandbox_state_persists() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        let store = PersistentCriStore::new(Arc::new(JsonStateStore::new(&path)));

        store.add_sandbox(sample_sandbox("sb1")).await;
        store
            .update_sandbox_state("sb1", SandboxState::NotReady)
            .await;

        let store2 = PersistentCriStore::new(Arc::new(JsonStateStore::new(&path)));
        store2.load().await.unwrap();

        let sb = store2.sandboxes.get("sb1").await.unwrap();
        assert_eq!(sb.state, SandboxState::NotReady);
    }

    #[tokio::test]
    async fn test_remove_sandbox_persists() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        let store = PersistentCriStore::new(Arc::new(JsonStateStore::new(&path)));

        store.add_sandbox(sample_sandbox("sb1")).await;
        store.add_sandbox(sample_sandbox("sb2")).await;
        store.remove_sandbox("sb1").await;

        let store2 = PersistentCriStore::new(Arc::new(JsonStateStore::new(&path)));
        store2.load().await.unwrap();

        assert!(store2.sandboxes.get("sb1").await.is_none());
        assert!(store2.sandboxes.get("sb2").await.is_some());
    }

    #[tokio::test]
    async fn test_mark_container_exited_persists() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        let store = PersistentCriStore::new(Arc::new(JsonStateStore::new(&path)));

        store.add_sandbox(sample_sandbox("sb1")).await;
        store.add_container(sample_container("c1", "sb1")).await;
        store.mark_container_exited("c1", 3_000_000_000, 42).await;

        let store2 = PersistentCriStore::new(Arc::new(JsonStateStore::new(&path)));
        store2.load().await.unwrap();

        let c = store2.containers.get("c1").await.unwrap();
        assert_eq!(c.state, ContainerState::Exited);
        assert_eq!(c.exit_code, 42);
        assert_eq!(c.finished_at, 3_000_000_000);
    }

    #[tokio::test]
    async fn test_remove_containers_by_sandbox_persists() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        let store = PersistentCriStore::new(Arc::new(JsonStateStore::new(&path)));

        store.add_sandbox(sample_sandbox("sb1")).await;
        store.add_sandbox(sample_sandbox("sb2")).await;
        store.add_container(sample_container("c1", "sb1")).await;
        store.add_container(sample_container("c2", "sb1")).await;
        store.add_container(sample_container("c3", "sb2")).await;
        store.remove_containers_by_sandbox("sb1").await;

        let store2 = PersistentCriStore::new(Arc::new(JsonStateStore::new(&path)));
        store2.load().await.unwrap();

        assert!(store2.containers.get("c1").await.is_none());
        assert!(store2.containers.get("c2").await.is_none());
        assert!(store2.containers.get("c3").await.is_some());
    }

    #[tokio::test]
    async fn test_crash_recovery_simulation() {
        // Simulate: store1 writes state, then "crashes" (dropped).
        // store2 loads from disk and sees the same state.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");

        {
            let store1 = PersistentCriStore::new(Arc::new(JsonStateStore::new(&path)));
            store1.add_sandbox(sample_sandbox("sb1")).await;
            store1.add_container(sample_container("c1", "sb1")).await;
            store1.mark_container_started("c1", 2_000_000_000).await;
            // store1 dropped here — simulates crash
        }

        let store2 = PersistentCriStore::new(Arc::new(JsonStateStore::new(&path)));
        store2.load().await.unwrap();

        let sb = store2.sandboxes.get("sb1").await.unwrap();
        assert_eq!(sb.state, SandboxState::Ready);

        let c = store2.containers.get("c1").await.unwrap();
        assert_eq!(c.state, ContainerState::Running);
        assert_eq!(c.started_at, 2_000_000_000);
    }

    #[tokio::test]
    async fn test_noop_store_no_persistence() {
        // With NoopStateStore, load always returns empty — no disk I/O
        let store = PersistentCriStore::new(Arc::new(NoopStateStore));
        store.add_sandbox(sample_sandbox("sb1")).await;

        // In-memory still works
        assert!(store.sandboxes.get("sb1").await.is_some());

        // But a fresh store with noop sees nothing
        let store2 = PersistentCriStore::new(Arc::new(NoopStateStore));
        store2.load().await.unwrap();
        assert!(store2.sandboxes.get("sb1").await.is_none());
    }

    #[tokio::test]
    async fn test_mark_container_started_persists() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        let store = PersistentCriStore::new(Arc::new(JsonStateStore::new(&path)));

        store.add_sandbox(sample_sandbox("sb1")).await;
        let mut c = sample_container("c1", "sb1");
        c.state = ContainerState::Created;
        c.started_at = 0;
        store.add_container(c).await;
        store.mark_container_started("c1", 5_000_000_000).await;

        let store2 = PersistentCriStore::new(Arc::new(JsonStateStore::new(&path)));
        store2.load().await.unwrap();

        let c = store2.containers.get("c1").await.unwrap();
        assert_eq!(c.state, ContainerState::Running);
        assert_eq!(c.started_at, 5_000_000_000);
    }
}
