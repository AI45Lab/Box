//! Container state management.
//!
//! Maps CRI Container to sessions within an A3S Box microVM.

use std::collections::HashMap;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;

use a3s_box_core::exec::ExecRequest;

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

/// A CRI mount persisted with container metadata.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContainerMount {
    pub container_path: String,
    pub host_path: String,
    pub readonly: bool,
    pub selinux_relabel: bool,
    pub propagation: i32,
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
    /// Content digest resolved from the local image store during CreateContainer.
    #[serde(default)]
    pub resolved_image_digest: String,
    /// Local OCI image layout path resolved during CreateContainer.
    #[serde(default)]
    pub resolved_image_path: String,
    /// Container entrypoint override from CRI.
    #[serde(default)]
    pub command: Vec<String>,
    /// Container command arguments from CRI.
    #[serde(default)]
    pub args: Vec<String>,
    /// Container environment variables from CRI.
    #[serde(default)]
    pub env: Vec<(String, String)>,
    /// Container working directory from CRI.
    #[serde(default)]
    pub working_dir: String,
    /// User override from CRI Linux security context.
    #[serde(default)]
    pub user: Option<String>,
    /// Whether stdin should be attached.
    #[serde(default)]
    pub stdin: bool,
    /// Whether stdin is closed after the first attach.
    #[serde(default)]
    pub stdin_once: bool,
    /// Whether this container requires a TTY.
    #[serde(default)]
    pub tty: bool,
    /// CRI mounts accepted during CreateContainer.
    #[serde(default)]
    pub mounts: Vec<ContainerMount>,
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
    /// Whether the container was killed by the out-of-memory killer (its
    /// memory cgroup exceeded `memory.max`). Drives the `OOMKilled` exit reason.
    #[serde(default)]
    pub oom_killed: bool,
    /// Container labels.
    pub labels: HashMap<String, String>,
    /// Container annotations.
    pub annotations: HashMap<String, String>,
    /// Log file path.
    pub log_path: String,
    /// Host path of the prepared container rootfs.
    #[serde(default)]
    pub rootfs_path: String,
    /// Guest-visible path of the prepared container rootfs.
    #[serde(default)]
    pub rootfs_guest_path: String,
}

impl Container {
    /// Return the stable image identity reported through CRI status surfaces.
    pub fn status_image_ref(&self) -> &str {
        if self.resolved_image_digest.is_empty() {
            &self.image_ref
        } else {
            &self.resolved_image_digest
        }
    }

    /// Convert the stored CRI command configuration into a guest exec request.
    ///
    /// This is the execution payload `StartContainer` will use once independent
    /// CRI container workloads are wired to the guest process supervisor.
    pub fn to_exec_request(&self, timeout_ns: u64) -> Result<ExecRequest, String> {
        let mut cmd = self.command.clone();
        cmd.extend(self.args.clone());

        if cmd.is_empty() {
            return Err(
                "resolved container command is empty; set CRI command/args or use an image with ENTRYPOINT/CMD"
                    .to_string(),
            );
        }

        let env = self
            .env
            .iter()
            .map(|(key, value)| format!("{key}={value}"))
            .collect();

        Ok(ExecRequest {
            cmd,
            timeout_ns,
            env,
            working_dir: if self.working_dir.is_empty() {
                None
            } else {
                Some(self.working_dir.clone())
            },
            rootfs: if self.rootfs_guest_path.is_empty() {
                None
            } else {
                Some(self.rootfs_guest_path.clone())
            },
            stdin: None,
            stdin_streaming: self.stdin,
            user: self.user.clone(),
            streaming: false,
        })
    }
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

    /// Mark a container started only if it is still in the created state.
    pub async fn mark_started_if_created(&self, id: &str, started_at: i64) -> bool {
        let mut store = self.containers.write().await;
        if let Some(c) = store.get_mut(id) {
            if c.state != ContainerState::Created {
                return false;
            }

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

    /// Mark a container exited only if it is still running.
    pub async fn mark_exited_if_running(
        &self,
        id: &str,
        finished_at: i64,
        exit_code: i32,
        oom_killed: bool,
    ) -> bool {
        let mut store = self.containers.write().await;
        if let Some(c) = store.get_mut(id) {
            if c.state != ContainerState::Running {
                return false;
            }

            c.state = ContainerState::Exited;
            c.finished_at = finished_at;
            c.exit_code = exit_code;
            c.oom_killed = oom_killed;
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
            resolved_image_digest: "sha256:test".to_string(),
            resolved_image_path: "/".to_string(),
            command: vec!["nginx".to_string()],
            args: vec!["-g".to_string(), "daemon off;".to_string()],
            env: vec![("ENV".to_string(), "test".to_string())],
            working_dir: "/".to_string(),
            user: Some("1000:1001".to_string()),
            stdin: false,
            stdin_once: false,
            tty: false,
            mounts: vec![],
            state: ContainerState::Created,
            created_at: 1000000000,
            started_at: 0,
            finished_at: 0,
            exit_code: 0,
            oom_killed: false,
            labels: HashMap::from([("app".to_string(), "test".to_string())]),
            annotations: HashMap::new(),
            log_path: format!("/var/log/pods/{}.log", id),
            rootfs_path: "/".to_string(),
            rootfs_guest_path: format!("/run/a3s/cri/container-rootfs/{sandbox_id}/{id}/rootfs"),
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
    async fn test_mark_started_if_created() {
        let store = ContainerStore::new();
        store.add(test_container("c1", "sb1")).await;

        assert!(store.mark_started_if_created("c1", 2000000000).await);
        let c = store.get("c1").await.unwrap();
        assert_eq!(c.state, ContainerState::Running);
        assert_eq!(c.started_at, 2000000000);
    }

    #[tokio::test]
    async fn test_mark_started_if_created_rejects_non_created_state() {
        let store = ContainerStore::new();
        store.add(test_container("c1", "sb1")).await;
        store.mark_exited("c1", 3000000000, 7).await;

        assert!(!store.mark_started_if_created("c1", 2000000000).await);
        let c = store.get("c1").await.unwrap();
        assert_eq!(c.state, ContainerState::Exited);
        assert_eq!(c.started_at, 0);
        assert_eq!(c.finished_at, 3000000000);
        assert_eq!(c.exit_code, 7);
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
    async fn test_mark_exited_if_running() {
        let store = ContainerStore::new();
        store.add(test_container("c1", "sb1")).await;
        store.mark_started("c1", 2000000000).await;

        assert!(
            store
                .mark_exited_if_running("c1", 3000000000, 42, false)
                .await
        );
        let c = store.get("c1").await.unwrap();
        assert_eq!(c.state, ContainerState::Exited);
        assert_eq!(c.finished_at, 3000000000);
        assert_eq!(c.exit_code, 42);
    }

    #[tokio::test]
    async fn test_mark_exited_if_running_rejects_non_running_state() {
        let store = ContainerStore::new();
        store.add(test_container("c1", "sb1")).await;

        assert!(
            !store
                .mark_exited_if_running("c1", 3000000000, 42, false)
                .await
        );
        let c = store.get("c1").await.unwrap();
        assert_eq!(c.state, ContainerState::Created);
        assert_eq!(c.finished_at, 0);
        assert_eq!(c.exit_code, 0);
    }

    #[tokio::test]
    async fn test_mark_exited_if_running_preserves_existing_exit() {
        let store = ContainerStore::new();
        store.add(test_container("c1", "sb1")).await;
        store.mark_started("c1", 2000000000).await;
        store.mark_exited("c1", 3000000000, 7).await;

        assert!(
            !store
                .mark_exited_if_running("c1", 4000000000, 42, false)
                .await
        );
        let c = store.get("c1").await.unwrap();
        assert_eq!(c.state, ContainerState::Exited);
        assert_eq!(c.finished_at, 3000000000);
        assert_eq!(c.exit_code, 7);
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

    #[test]
    fn test_to_exec_request() {
        let c = test_container("c1", "sb1");
        let req = c.to_exec_request(1_000).unwrap();

        assert_eq!(
            req.cmd,
            vec![
                "nginx".to_string(),
                "-g".to_string(),
                "daemon off;".to_string()
            ]
        );
        assert_eq!(req.timeout_ns, 1_000);
        assert_eq!(req.env, vec!["ENV=test".to_string()]);
        assert_eq!(req.working_dir, Some("/".to_string()));
        assert_eq!(
            req.rootfs,
            Some("/run/a3s/cri/container-rootfs/sb1/c1/rootfs".to_string())
        );
        assert_eq!(req.user, Some("1000:1001".to_string()));
        assert!(req.stdin.is_none());
        assert!(!req.streaming);
    }

    #[test]
    fn test_to_exec_request_rejects_empty_command() {
        let mut c = test_container("c1", "sb1");
        c.command.clear();
        c.args.clear();

        let err = c.to_exec_request(0).unwrap_err();
        assert!(err.contains("resolved container command is empty"));
    }

    #[test]
    fn test_container_deserializes_legacy_image_metadata_defaults() {
        let json = r#"{
            "id": "c1",
            "sandbox_id": "sb1",
            "name": "legacy",
            "image_ref": "nginx:latest",
            "command": ["nginx"],
            "args": [],
            "env": [],
            "working_dir": "/",
            "user": null,
            "stdin": false,
            "stdin_once": false,
            "tty": false,
            "state": "Created",
            "created_at": 1000000000,
            "started_at": 0,
            "finished_at": 0,
            "exit_code": 0,
            "labels": {},
            "annotations": {},
            "log_path": ""
        }"#;

        let container: Container = serde_json::from_str(json).unwrap();
        assert_eq!(container.resolved_image_digest, "");
        assert_eq!(container.resolved_image_path, "");
        assert_eq!(container.rootfs_path, "");
        assert_eq!(container.rootfs_guest_path, "");
        assert_eq!(container.status_image_ref(), "nginx:latest");
    }
}
