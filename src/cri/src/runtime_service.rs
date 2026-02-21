//! CRI RuntimeService implementation.
//!
//! Maps CRI pod/container lifecycle to A3S Box VmManager instances.
//! - Pod Sandbox → Box instance (one microVM per pod)
//! - Container → Session within Box

use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::RwLock;
use tonic::{Request, Response, Status};

use a3s_box_core::event::EventEmitter;
use a3s_box_runtime::oci::{ImageStore, RegistryAuth};
use a3s_box_runtime::pool::WarmPool;
use a3s_box_runtime::vm::VmManager;

use crate::config_mapper::pod_sandbox_config_to_box_config;
use crate::container::{Container, ContainerState};
use crate::cri_api::runtime_service_server::RuntimeService;
use crate::cri_api::*;
use crate::error::box_error_to_status;
use crate::persistent_store::PersistentCriStore;
use crate::sandbox::{PodSandbox, SandboxState};
use crate::state::{default_state_path, JsonStateStore, NoopStateStore, StateStore};
use crate::streaming::{SessionKind, StreamingHandle, StreamingSession};

/// A3S Box implementation of the CRI RuntimeService.
pub struct BoxRuntimeService {
    store: Arc<PersistentCriStore>,
    /// Maps sandbox_id → VmManager for running VMs.
    vm_managers: Arc<RwLock<HashMap<String, VmManager>>>,
    /// Handle for registering CRI streaming sessions.
    streaming: StreamingHandle,
    /// Optional warm pool for instant VM acquisition.
    warm_pool: Option<Arc<RwLock<WarmPool>>>,
}

impl BoxRuntimeService {
    /// Create a new BoxRuntimeService with JSON-backed persistent state.
    pub fn new(
        _image_store: Arc<ImageStore>,
        _auth: RegistryAuth,
        streaming: StreamingHandle,
    ) -> Self {
        let state_store: Arc<dyn StateStore> = Arc::new(JsonStateStore::new(default_state_path()));
        Self::with_state_store(_image_store, _auth, streaming, state_store)
    }

    /// Create a BoxRuntimeService with a custom StateStore (used in tests).
    pub fn with_state_store(
        _image_store: Arc<ImageStore>,
        _auth: RegistryAuth,
        streaming: StreamingHandle,
        state_store: Arc<dyn StateStore>,
    ) -> Self {
        Self {
            store: Arc::new(PersistentCriStore::new(state_store)),
            vm_managers: Arc::new(RwLock::new(HashMap::new())),
            streaming,
            warm_pool: None,
        }
    }

    /// Attach a warm pool for instant VM acquisition on RunPodSandbox.
    pub fn with_warm_pool(mut self, pool: WarmPool) -> Self {
        self.warm_pool = Some(Arc::new(RwLock::new(pool)));
        self
    }

    /// Load persisted state from disk. Call once after construction.
    pub async fn load_state(&self) {
        if let Err(e) = self.store.load().await {
            tracing::warn!(error = %e, "Failed to load persisted CRI state — starting fresh");
        }
    }

    /// Acquire a VM: from warm pool if available, otherwise cold boot.
    async fn acquire_vm(
        &self,
        box_config: a3s_box_core::config::BoxConfig,
    ) -> Result<VmManager, Status> {
        if let Some(ref pool) = self.warm_pool {
            let pool = pool.read().await;
            match pool.acquire().await {
                Ok(vm) => {
                    tracing::debug!(box_id = %vm.box_id(), "Acquired VM from warm pool");
                    return Ok(vm);
                }
                Err(e) => {
                    tracing::warn!(error = %e, "Warm pool acquire failed, falling back to cold boot");
                }
            }
        }

        // Cold boot
        let event_emitter = EventEmitter::new(256);
        let mut vm = VmManager::new(box_config, event_emitter);
        vm.boot().await.map_err(box_error_to_status)?;
        Ok(vm)
    }

    /// Release a VM back to the warm pool, or destroy it if no pool.
    async fn release_vm(&self, vm: VmManager) {
        if let Some(ref pool) = self.warm_pool {
            let pool = pool.read().await;
            if let Err(e) = pool.release(vm).await {
                tracing::warn!(error = %e, "Failed to release VM to warm pool");
            }
        } else {
            let mut vm = vm;
            if let Err(e) = vm.destroy().await {
                tracing::warn!(error = %e, "Failed to destroy VM");
            }
        }
    }
}

#[tonic::async_trait]
impl RuntimeService for BoxRuntimeService {
    // ── Version ──────────────────────────────────────────────────────

    async fn version(
        &self,
        request: Request<VersionRequest>,
    ) -> Result<Response<VersionResponse>, Status> {
        let _req = request.into_inner();
        Ok(Response::new(VersionResponse {
            version: "0.1.0".to_string(),
            runtime_name: "a3s-box".to_string(),
            runtime_version: a3s_box_runtime::VERSION.to_string(),
            runtime_api_version: "v1".to_string(),
        }))
    }

    // ── Pod Sandbox ──────────────────────────────────────────────────

    async fn run_pod_sandbox(
        &self,
        request: Request<RunPodSandboxRequest>,
    ) -> Result<Response<RunPodSandboxResponse>, Status> {
        let req = request.into_inner();
        let config = req
            .config
            .ok_or_else(|| Status::invalid_argument("sandbox config required"))?;

        let metadata = config
            .metadata
            .as_ref()
            .ok_or_else(|| Status::invalid_argument("sandbox metadata required"))?;

        tracing::info!(
            name = %metadata.name,
            namespace = %metadata.namespace,
            "CRI RunPodSandbox"
        );

        // Convert CRI config to BoxConfig
        let box_config = pod_sandbox_config_to_box_config(&config).map_err(box_error_to_status)?;

        // Acquire VM: from warm pool if available, otherwise cold boot
        let vm = self.acquire_vm(box_config).await?;
        let sandbox_id = vm.box_id().to_string();

        // Store sandbox state
        let now_ns = chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0);
        let sandbox = PodSandbox {
            id: sandbox_id.clone(),
            name: metadata.name.clone(),
            namespace: metadata.namespace.clone(),
            uid: metadata.uid.clone(),
            state: SandboxState::Ready,
            created_at: now_ns,
            labels: config.labels.clone(),
            annotations: config.annotations.clone(),
            log_directory: config.log_directory.clone(),
            runtime_handler: req.runtime_handler,
        };

        self.store.add_sandbox(sandbox).await;
        self.vm_managers
            .write()
            .await
            .insert(sandbox_id.clone(), vm);

        Ok(Response::new(RunPodSandboxResponse {
            pod_sandbox_id: sandbox_id,
        }))
    }

    async fn stop_pod_sandbox(
        &self,
        request: Request<StopPodSandboxRequest>,
    ) -> Result<Response<StopPodSandboxResponse>, Status> {
        let req = request.into_inner();
        let sandbox_id = &req.pod_sandbox_id;

        tracing::info!(sandbox_id = %sandbox_id, "CRI StopPodSandbox");

        // Stop all containers in this sandbox
        let containers = self.store.containers.list(Some(sandbox_id), None).await;
        let now_ns = chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0);
        for c in &containers {
            if c.state != ContainerState::Exited {
                self.store.mark_container_exited(&c.id, now_ns, 137).await;
            }
        }

        // Destroy the VM
        if let Some(mut vm) = self.vm_managers.write().await.remove(sandbox_id) {
            vm.destroy().await.map_err(box_error_to_status)?;
        }

        self.store
            .update_sandbox_state(sandbox_id, SandboxState::NotReady)
            .await;

        Ok(Response::new(StopPodSandboxResponse {}))
    }

    async fn remove_pod_sandbox(
        &self,
        request: Request<RemovePodSandboxRequest>,
    ) -> Result<Response<RemovePodSandboxResponse>, Status> {
        let req = request.into_inner();
        let sandbox_id = &req.pod_sandbox_id;

        tracing::info!(sandbox_id = %sandbox_id, "CRI RemovePodSandbox");

        // Release VM back to warm pool (or destroy if no pool)
        if let Some(vm) = self.vm_managers.write().await.remove(sandbox_id) {
            self.release_vm(vm).await;
        }

        // Remove all containers
        self.store.remove_containers_by_sandbox(sandbox_id).await;

        // Remove sandbox
        self.store.remove_sandbox(sandbox_id).await;

        Ok(Response::new(RemovePodSandboxResponse {}))
    }

    async fn pod_sandbox_status(
        &self,
        request: Request<PodSandboxStatusRequest>,
    ) -> Result<Response<PodSandboxStatusResponse>, Status> {
        let req = request.into_inner();
        let sandbox_id = &req.pod_sandbox_id;

        let sandbox = self
            .store
            .sandboxes
            .get(sandbox_id)
            .await
            .ok_or_else(|| Status::not_found(format!("Sandbox not found: {}", sandbox_id)))?;

        let state = match sandbox.state {
            SandboxState::Ready => PodSandboxState::SandboxReady,
            SandboxState::NotReady | SandboxState::Removed => PodSandboxState::SandboxNotready,
        };

        let status = PodSandboxStatus {
            id: sandbox.id.clone(),
            metadata: Some(PodSandboxMetadata {
                name: sandbox.name.clone(),
                uid: sandbox.uid.clone(),
                namespace: sandbox.namespace.clone(),
                attempt: 0,
            }),
            state: state.into(),
            created_at: sandbox.created_at,
            network: Some(PodSandboxNetworkStatus {
                ip: String::new(),
                additional_ips: vec![],
            }),
            linux: None,
            labels: sandbox.labels.clone(),
            annotations: sandbox.annotations.clone(),
            runtime_handler: sandbox.runtime_handler.clone(),
        };

        Ok(Response::new(PodSandboxStatusResponse {
            status: Some(status),
            info: Default::default(),
        }))
    }

    async fn list_pod_sandbox(
        &self,
        request: Request<ListPodSandboxRequest>,
    ) -> Result<Response<ListPodSandboxResponse>, Status> {
        let req = request.into_inner();

        let label_filter = req
            .filter
            .as_ref()
            .map(|f| &f.label_selector)
            .filter(|m| !m.is_empty());

        let sandboxes = self.store.sandboxes.list(label_filter).await;

        let items: Vec<crate::cri_api::PodSandbox> = sandboxes
            .into_iter()
            .filter(|sb| {
                if let Some(ref filter) = req.filter {
                    // Filter by ID
                    if !filter.id.is_empty() && sb.id != filter.id {
                        return false;
                    }
                    // Filter by state
                    let sb_state = match sb.state {
                        SandboxState::Ready => PodSandboxState::SandboxReady as i32,
                        _ => PodSandboxState::SandboxNotready as i32,
                    };
                    if filter.state != 0 && filter.state != sb_state {
                        return false;
                    }
                }
                true
            })
            .map(|sb| {
                let state = match sb.state {
                    SandboxState::Ready => PodSandboxState::SandboxReady,
                    _ => PodSandboxState::SandboxNotready,
                };
                crate::cri_api::PodSandbox {
                    id: sb.id,
                    metadata: Some(PodSandboxMetadata {
                        name: sb.name,
                        uid: sb.uid,
                        namespace: sb.namespace,
                        attempt: 0,
                    }),
                    state: state.into(),
                    created_at: sb.created_at,
                    labels: sb.labels,
                    annotations: sb.annotations,
                    runtime_handler: sb.runtime_handler,
                }
            })
            .collect();

        Ok(Response::new(ListPodSandboxResponse { items }))
    }

    // ── Container ────────────────────────────────────────────────────

    async fn create_container(
        &self,
        request: Request<CreateContainerRequest>,
    ) -> Result<Response<CreateContainerResponse>, Status> {
        let req = request.into_inner();
        let sandbox_id = &req.pod_sandbox_id;

        // Verify sandbox exists
        self.store
            .sandboxes
            .get(sandbox_id)
            .await
            .ok_or_else(|| Status::not_found(format!("Sandbox not found: {}", sandbox_id)))?;

        let config = req
            .config
            .ok_or_else(|| Status::invalid_argument("container config required"))?;

        let metadata = config
            .metadata
            .as_ref()
            .ok_or_else(|| Status::invalid_argument("container metadata required"))?;

        let image_ref = config
            .image
            .as_ref()
            .map(|i| i.image.clone())
            .unwrap_or_default();

        tracing::info!(
            sandbox_id = %sandbox_id,
            name = %metadata.name,
            image = %image_ref,
            "CRI CreateContainer"
        );

        let container_id = uuid::Uuid::new_v4().to_string();
        let now_ns = chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0);

        let container = Container {
            id: container_id.clone(),
            sandbox_id: sandbox_id.to_string(),
            name: metadata.name.clone(),
            image_ref,
            state: ContainerState::Created,
            created_at: now_ns,
            started_at: 0,
            finished_at: 0,
            exit_code: 0,
            labels: config.labels.clone(),
            annotations: config.annotations.clone(),
            log_path: config.log_path,
        };

        self.store.add_container(container).await;

        Ok(Response::new(CreateContainerResponse { container_id }))
    }

    async fn start_container(
        &self,
        request: Request<StartContainerRequest>,
    ) -> Result<Response<StartContainerResponse>, Status> {
        let req = request.into_inner();
        let container_id = &req.container_id;

        let container = self
            .store
            .containers
            .get(container_id)
            .await
            .ok_or_else(|| Status::not_found(format!("Container not found: {}", container_id)))?;

        tracing::info!(
            container_id = %container_id,
            sandbox_id = %container.sandbox_id,
            "CRI StartContainer"
        );

        let now_ns = chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0);
        self.store
            .mark_container_started(container_id, now_ns)
            .await;

        Ok(Response::new(StartContainerResponse {}))
    }

    async fn stop_container(
        &self,
        request: Request<StopContainerRequest>,
    ) -> Result<Response<StopContainerResponse>, Status> {
        let req = request.into_inner();
        let container_id = &req.container_id;

        tracing::info!(container_id = %container_id, "CRI StopContainer");

        let now_ns = chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0);
        self.store
            .mark_container_exited(container_id, now_ns, 0)
            .await;

        Ok(Response::new(StopContainerResponse {}))
    }

    async fn remove_container(
        &self,
        request: Request<RemoveContainerRequest>,
    ) -> Result<Response<RemoveContainerResponse>, Status> {
        let req = request.into_inner();
        let container_id = &req.container_id;

        tracing::info!(container_id = %container_id, "CRI RemoveContainer");

        self.store.remove_container(container_id).await;

        Ok(Response::new(RemoveContainerResponse {}))
    }

    async fn container_status(
        &self,
        request: Request<ContainerStatusRequest>,
    ) -> Result<Response<ContainerStatusResponse>, Status> {
        let req = request.into_inner();
        let container_id = &req.container_id;

        let container = self
            .store
            .containers
            .get(container_id)
            .await
            .ok_or_else(|| Status::not_found(format!("Container not found: {}", container_id)))?;

        let state = match container.state {
            ContainerState::Created => crate::cri_api::ContainerState::ContainerCreated,
            ContainerState::Running => crate::cri_api::ContainerState::ContainerRunning,
            ContainerState::Exited => crate::cri_api::ContainerState::ContainerExited,
        };

        let status = ContainerStatus {
            id: container.id.clone(),
            metadata: Some(ContainerMetadata {
                name: container.name.clone(),
                attempt: 0,
            }),
            state: state.into(),
            created_at: container.created_at,
            started_at: container.started_at,
            finished_at: container.finished_at,
            exit_code: container.exit_code,
            image: Some(ImageSpec {
                image: container.image_ref.clone(),
                annotations: Default::default(),
            }),
            image_ref: container.image_ref.clone(),
            reason: String::new(),
            message: String::new(),
            labels: container.labels.clone(),
            annotations: container.annotations.clone(),
            mounts: vec![],
            log_path: container.log_path.clone(),
        };

        Ok(Response::new(ContainerStatusResponse {
            status: Some(status),
            info: Default::default(),
        }))
    }

    async fn list_containers(
        &self,
        request: Request<ListContainersRequest>,
    ) -> Result<Response<ListContainersResponse>, Status> {
        let req = request.into_inner();

        let sandbox_filter = req
            .filter
            .as_ref()
            .map(|f| f.pod_sandbox_id.as_str())
            .filter(|s| !s.is_empty());

        let label_filter = req
            .filter
            .as_ref()
            .map(|f| &f.label_selector)
            .filter(|m| !m.is_empty());

        let containers = self
            .store
            .containers
            .list(sandbox_filter, label_filter)
            .await;

        let items: Vec<crate::cri_api::Container> = containers
            .into_iter()
            .filter(|c| {
                if let Some(ref filter) = req.filter {
                    if !filter.id.is_empty() && c.id != filter.id {
                        return false;
                    }
                    if let Some(ref state_val) = filter.state {
                        let c_state = match c.state {
                            ContainerState::Created => {
                                crate::cri_api::ContainerState::ContainerCreated as i32
                            }
                            ContainerState::Running => {
                                crate::cri_api::ContainerState::ContainerRunning as i32
                            }
                            ContainerState::Exited => {
                                crate::cri_api::ContainerState::ContainerExited as i32
                            }
                        };
                        if state_val.state != c_state {
                            return false;
                        }
                    }
                }
                true
            })
            .map(|c| {
                let state = match c.state {
                    ContainerState::Created => crate::cri_api::ContainerState::ContainerCreated,
                    ContainerState::Running => crate::cri_api::ContainerState::ContainerRunning,
                    ContainerState::Exited => crate::cri_api::ContainerState::ContainerExited,
                };
                crate::cri_api::Container {
                    id: c.id,
                    pod_sandbox_id: c.sandbox_id,
                    metadata: Some(ContainerMetadata {
                        name: c.name,
                        attempt: 0,
                    }),
                    image: Some(ImageSpec {
                        image: c.image_ref.clone(),
                        annotations: Default::default(),
                    }),
                    image_ref: c.image_ref,
                    state: state.into(),
                    created_at: c.created_at,
                    labels: c.labels,
                    annotations: c.annotations,
                }
            })
            .collect();

        Ok(Response::new(ListContainersResponse { containers: items }))
    }

    // ── Status ───────────────────────────────────────────────────────

    async fn status(
        &self,
        _request: Request<StatusRequest>,
    ) -> Result<Response<StatusResponse>, Status> {
        let conditions = vec![
            RuntimeCondition {
                r#type: "RuntimeReady".to_string(),
                status: true,
                reason: String::new(),
                message: String::new(),
            },
            RuntimeCondition {
                r#type: "NetworkReady".to_string(),
                status: true,
                reason: String::new(),
                message: String::new(),
            },
        ];

        Ok(Response::new(StatusResponse {
            status: Some(RuntimeStatus { conditions }),
            info: Default::default(),
        }))
    }

    async fn update_runtime_config(
        &self,
        _request: Request<UpdateRuntimeConfigRequest>,
    ) -> Result<Response<UpdateRuntimeConfigResponse>, Status> {
        // Accept but ignore runtime config updates for now
        Ok(Response::new(UpdateRuntimeConfigResponse {}))
    }

    // ── Exec / Attach / PortForward ────────────────────────────────

    async fn exec_sync(
        &self,
        request: Request<ExecSyncRequest>,
    ) -> Result<Response<ExecSyncResponse>, Status> {
        let req = request.into_inner();
        let container_id = &req.container_id;

        tracing::info!(
            container_id = %container_id,
            cmd = ?req.cmd,
            "CRI ExecSync"
        );

        // Look up the container to find its sandbox
        let container = self
            .store
            .containers
            .get(container_id)
            .await
            .ok_or_else(|| Status::not_found(format!("Container not found: {}", container_id)))?;

        // Get the VmManager for this sandbox
        let vm_managers = self.vm_managers.read().await;
        let vm = vm_managers.get(&container.sandbox_id).ok_or_else(|| {
            Status::not_found(format!("Sandbox not found: {}", container.sandbox_id))
        })?;

        // Execute the command via the exec client
        let timeout_ns = if req.timeout > 0 {
            req.timeout as u64 * 1_000_000_000
        } else {
            a3s_box_core::exec::DEFAULT_EXEC_TIMEOUT_NS
        };

        let output = vm
            .exec_command(req.cmd, timeout_ns)
            .await
            .map_err(box_error_to_status)?;

        Ok(Response::new(ExecSyncResponse {
            stdout: output.stdout,
            stderr: output.stderr,
            exit_code: output.exit_code,
        }))
    }

    async fn exec(&self, request: Request<ExecRequest>) -> Result<Response<ExecResponse>, Status> {
        let req = request.into_inner();
        let container_id = &req.container_id;

        tracing::info!(
            container_id = %container_id,
            cmd = ?req.cmd,
            tty = req.tty,
            "CRI Exec"
        );

        // Look up the container to find its sandbox
        let container = self
            .store
            .containers
            .get(container_id)
            .await
            .ok_or_else(|| Status::not_found(format!("Container not found: {}", container_id)))?;

        // Get the VmManager for this sandbox
        let vm_managers = self.vm_managers.read().await;
        let vm = vm_managers.get(&container.sandbox_id).ok_or_else(|| {
            Status::not_found(format!("Sandbox not found: {}", container.sandbox_id))
        })?;

        let exec_socket = vm
            .exec_socket_path()
            .ok_or_else(|| Status::unavailable("VM exec socket not ready"))?
            .to_string_lossy()
            .to_string();
        let pty_socket = vm
            .pty_socket_path()
            .ok_or_else(|| Status::unavailable("VM PTY socket not ready"))?
            .to_string_lossy()
            .to_string();

        let session = StreamingSession {
            kind: SessionKind::Exec,
            sandbox_id: container.sandbox_id.clone(),
            cmd: req.cmd,
            tty: req.tty,
            stdin: req.stdin,
            ports: vec![],
            exec_socket_path: exec_socket,
            pty_socket_path: pty_socket,
        };

        let url = self.streaming.register(session).await;
        Ok(Response::new(ExecResponse { url }))
    }

    async fn attach(
        &self,
        request: Request<AttachRequest>,
    ) -> Result<Response<AttachResponse>, Status> {
        let req = request.into_inner();
        let container_id = &req.container_id;

        tracing::info!(
            container_id = %container_id,
            tty = req.tty,
            "CRI Attach"
        );

        let container = self
            .store
            .containers
            .get(container_id)
            .await
            .ok_or_else(|| Status::not_found(format!("Container not found: {}", container_id)))?;

        let vm_managers = self.vm_managers.read().await;
        let vm = vm_managers.get(&container.sandbox_id).ok_or_else(|| {
            Status::not_found(format!("Sandbox not found: {}", container.sandbox_id))
        })?;

        let exec_socket = vm
            .exec_socket_path()
            .ok_or_else(|| Status::unavailable("VM exec socket not ready"))?
            .to_string_lossy()
            .to_string();
        let pty_socket = vm
            .pty_socket_path()
            .ok_or_else(|| Status::unavailable("VM PTY socket not ready"))?
            .to_string_lossy()
            .to_string();

        let session = StreamingSession {
            kind: SessionKind::Attach,
            sandbox_id: container.sandbox_id.clone(),
            cmd: vec![],
            tty: req.tty,
            stdin: req.stdin,
            ports: vec![],
            exec_socket_path: exec_socket,
            pty_socket_path: pty_socket,
        };

        let url = self.streaming.register(session).await;
        Ok(Response::new(AttachResponse { url }))
    }

    async fn port_forward(
        &self,
        request: Request<PortForwardRequest>,
    ) -> Result<Response<PortForwardResponse>, Status> {
        let req = request.into_inner();
        let sandbox_id = &req.pod_sandbox_id;

        tracing::info!(
            sandbox_id = %sandbox_id,
            ports = ?req.port,
            "CRI PortForward"
        );

        // Verify sandbox exists
        self.store
            .sandboxes
            .get(sandbox_id)
            .await
            .ok_or_else(|| Status::not_found(format!("Sandbox not found: {}", sandbox_id)))?;

        let vm_managers = self.vm_managers.read().await;
        let vm = vm_managers.get(sandbox_id).ok_or_else(|| {
            Status::not_found(format!("VM not found for sandbox: {}", sandbox_id))
        })?;

        let exec_socket = vm
            .exec_socket_path()
            .ok_or_else(|| Status::unavailable("VM exec socket not ready"))?
            .to_string_lossy()
            .to_string();
        let pty_socket = vm
            .pty_socket_path()
            .ok_or_else(|| Status::unavailable("VM PTY socket not ready"))?
            .to_string_lossy()
            .to_string();

        let session = StreamingSession {
            kind: SessionKind::PortForward,
            sandbox_id: sandbox_id.to_string(),
            cmd: vec![],
            tty: false,
            stdin: false,
            ports: req.port,
            exec_socket_path: exec_socket,
            pty_socket_path: pty_socket,
        };

        let url = self.streaming.register(session).await;
        Ok(Response::new(PortForwardResponse { url }))
    }

    async fn update_container_resources(
        &self,
        request: Request<UpdateContainerResourcesRequest>,
    ) -> Result<Response<UpdateContainerResourcesResponse>, Status> {
        let req = request.into_inner();
        let container_id = &req.container_id;

        // Verify container exists
        let container = self
            .store
            .containers
            .get(container_id)
            .await
            .ok_or_else(|| Status::not_found(format!("Container not found: {}", container_id)))?;

        let Some(ref linux) = req.linux else {
            tracing::info!(
                container_id = %container_id,
                "CRI UpdateContainerResources (no linux resources specified)"
            );
            return Ok(Response::new(UpdateContainerResourcesResponse {}));
        };

        // Build a ResourceUpdate from the CRI request.
        // memory_limit_in_bytes maps to Tier 1 (immutable) — reject if set.
        // cpu_quota, cpu_period, cpu_shares map to Tier 2 (cgroup) — apply via exec.
        let mut update = a3s_box_runtime::resize::ResourceUpdate::default();

        // Tier 1: memory_limit is a hard VM limit, cannot change after boot
        if linux.memory_limit_in_bytes > 0 {
            return Err(Status::unimplemented(
                "Cannot change memory limit on a running microVM: libkrun does not support \
                 memory ballooning. Recreate the pod with the desired memory size.",
            ));
        }

        // Tier 2: cgroup-based limits — apply via guest exec
        if linux.cpu_quota != 0 {
            update.limits.cpu_quota = Some(linux.cpu_quota);
        }
        if linux.cpu_period != 0 {
            update.limits.cpu_period = Some(linux.cpu_period as u64);
        }
        if linux.cpu_shares != 0 {
            update.limits.cpu_shares = Some(linux.cpu_shares as u64);
        }
        if !linux.cpuset_cpus.is_empty() {
            update.limits.cpuset_cpus = Some(linux.cpuset_cpus.clone());
        }
        if !linux.cpuset_mems.is_empty() {
            // cpuset_mems is not directly supported, log and ignore
            tracing::info!(
                container_id = %container_id,
                cpuset_mems = %linux.cpuset_mems,
                "CRI cpuset_mems ignored (not supported in microVM)"
            );
        }

        if !update.has_tier2_changes() {
            tracing::info!(
                container_id = %container_id,
                "CRI UpdateContainerResources: no applicable Tier 2 changes"
            );
            return Ok(Response::new(UpdateContainerResourcesResponse {}));
        }

        // Find the VM manager for this container's sandbox
        let managers = self.vm_managers.read().await;
        let vm = managers.get(&container.sandbox_id).ok_or_else(|| {
            Status::failed_precondition(format!(
                "Sandbox {} not running (VM not found)",
                container.sandbox_id
            ))
        })?;

        tracing::info!(
            container_id = %container_id,
            sandbox_id = %container.sandbox_id,
            cpu_quota = linux.cpu_quota,
            cpu_period = linux.cpu_period,
            cpu_shares = linux.cpu_shares,
            "CRI UpdateContainerResources: applying Tier 2 cgroup changes"
        );

        let result = vm
            .update_resources(&update)
            .await
            .map_err(|e| Status::internal(format!("Failed to apply resource update: {}", e)))?;

        if !result.rejected.is_empty() {
            let failures: Vec<String> = result
                .rejected
                .iter()
                .map(|(cmd, reason)| format!("{}: {}", cmd, reason))
                .collect();
            tracing::warn!(
                container_id = %container_id,
                failures = ?failures,
                "Some cgroup updates failed inside guest"
            );
        }

        Ok(Response::new(UpdateContainerResourcesResponse {}))
    }

    async fn reopen_container_log(
        &self,
        request: Request<ReopenContainerLogRequest>,
    ) -> Result<Response<ReopenContainerLogResponse>, Status> {
        let req = request.into_inner();
        let container_id = &req.container_id;

        let container = self
            .store
            .containers
            .get(container_id)
            .await
            .ok_or_else(|| Status::not_found(format!("Container not found: {}", container_id)))?;

        tracing::info!(
            container_id = %container_id,
            log_path = %container.log_path,
            "CRI ReopenContainerLog"
        );

        // If the container has a log path, signal log rotation by truncating
        // the existing log file. The guest agent will continue writing to it.
        if !container.log_path.is_empty() {
            let log_path = std::path::Path::new(&container.log_path);
            if log_path.exists() {
                if let Err(e) = std::fs::OpenOptions::new()
                    .write(true)
                    .truncate(true)
                    .open(log_path)
                {
                    tracing::warn!(
                        container_id = %container_id,
                        error = %e,
                        "Failed to truncate container log"
                    );
                }
            }
        }

        Ok(Response::new(ReopenContainerLogResponse {}))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::SocketAddr;

    use crate::streaming::StreamingServer;

    /// Create a BoxRuntimeService for testing.
    /// Uses NoopStateStore (no disk I/O) and a dummy StreamingHandle.
    fn make_test_service() -> BoxRuntimeService {
        let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
        let streaming_server = StreamingServer::new(addr);
        let handle = streaming_server.handle();

        BoxRuntimeService {
            store: Arc::new(PersistentCriStore::new(Arc::new(NoopStateStore))),
            vm_managers: Arc::new(RwLock::new(HashMap::new())),
            streaming: handle,
            warm_pool: None,
        }
    }

    fn test_sandbox(id: &str) -> PodSandbox {
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

    fn test_container(id: &str, sandbox_id: &str) -> Container {
        Container {
            id: id.to_string(),
            sandbox_id: sandbox_id.to_string(),
            name: format!("container-{}", id),
            image_ref: "nginx:latest".to_string(),
            state: ContainerState::Created,
            created_at: 1_000_000_000,
            started_at: 0,
            finished_at: 0,
            exit_code: 0,
            labels: HashMap::from([("app".to_string(), "test".to_string())]),
            annotations: HashMap::new(),
            log_path: String::new(),
        }
    }

    // ── Version ──────────────────────────────────────────────────────

    #[tokio::test]
    async fn test_version() {
        let svc = make_test_service();
        let resp = svc
            .version(Request::new(VersionRequest {
                version: "0.1.0".to_string(),
            }))
            .await
            .unwrap()
            .into_inner();

        assert_eq!(resp.runtime_name, "a3s-box");
        assert_eq!(resp.runtime_api_version, "v1");
        assert!(!resp.runtime_version.is_empty());
    }

    // ── Status ───────────────────────────────────────────────────────

    #[tokio::test]
    async fn test_status() {
        let svc = make_test_service();
        let resp = svc
            .status(Request::new(StatusRequest { verbose: false }))
            .await
            .unwrap()
            .into_inner();

        let status = resp.status.unwrap();
        assert_eq!(status.conditions.len(), 2);
        assert!(status
            .conditions
            .iter()
            .any(|c| c.r#type == "RuntimeReady" && c.status));
        assert!(status
            .conditions
            .iter()
            .any(|c| c.r#type == "NetworkReady" && c.status));
    }

    // ── UpdateRuntimeConfig ──────────────────────────────────────────

    #[tokio::test]
    async fn test_update_runtime_config() {
        let svc = make_test_service();
        let result = svc
            .update_runtime_config(Request::new(UpdateRuntimeConfigRequest {
                runtime_config: None,
            }))
            .await;
        assert!(result.is_ok());
    }

    // ── Pod Sandbox Status / List ────────────────────────────────────

    #[tokio::test]
    async fn test_pod_sandbox_status_not_found() {
        let svc = make_test_service();
        let result = svc
            .pod_sandbox_status(Request::new(PodSandboxStatusRequest {
                pod_sandbox_id: "nonexistent".to_string(),
                verbose: false,
            }))
            .await;
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().code(), tonic::Code::NotFound);
    }

    #[tokio::test]
    async fn test_pod_sandbox_status_found() {
        let svc = make_test_service();
        svc.store.sandboxes.add(test_sandbox("sb-1")).await;

        let resp = svc
            .pod_sandbox_status(Request::new(PodSandboxStatusRequest {
                pod_sandbox_id: "sb-1".to_string(),
                verbose: false,
            }))
            .await
            .unwrap()
            .into_inner();

        let status = resp.status.unwrap();
        assert_eq!(status.id, "sb-1");
        assert_eq!(status.state(), PodSandboxState::SandboxReady);
        let meta = status.metadata.unwrap();
        assert_eq!(meta.name, "pod-sb-1");
        assert_eq!(meta.namespace, "default");
    }

    #[tokio::test]
    async fn test_list_pod_sandbox_empty() {
        let svc = make_test_service();
        let resp = svc
            .list_pod_sandbox(Request::new(ListPodSandboxRequest { filter: None }))
            .await
            .unwrap()
            .into_inner();
        assert!(resp.items.is_empty());
    }

    #[tokio::test]
    async fn test_list_pod_sandbox_with_entries() {
        let svc = make_test_service();
        svc.store.sandboxes.add(test_sandbox("sb-1")).await;
        svc.store.sandboxes.add(test_sandbox("sb-2")).await;

        let resp = svc
            .list_pod_sandbox(Request::new(ListPodSandboxRequest { filter: None }))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(resp.items.len(), 2);
    }

    #[tokio::test]
    async fn test_list_pod_sandbox_filter_by_id() {
        let svc = make_test_service();
        svc.store.sandboxes.add(test_sandbox("sb-1")).await;
        svc.store.sandboxes.add(test_sandbox("sb-2")).await;

        let resp = svc
            .list_pod_sandbox(Request::new(ListPodSandboxRequest {
                filter: Some(PodSandboxFilter {
                    id: "sb-1".to_string(),
                    state: 0,
                    label_selector: HashMap::new(),
                }),
            }))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(resp.items.len(), 1);
        assert_eq!(resp.items[0].id, "sb-1");
    }

    // ── Container CRUD ───────────────────────────────────────────────

    #[tokio::test]
    async fn test_create_container_sandbox_not_found() {
        let svc = make_test_service();
        let result = svc
            .create_container(Request::new(CreateContainerRequest {
                pod_sandbox_id: "nonexistent".to_string(),
                config: Some(ContainerConfig {
                    metadata: Some(ContainerMetadata {
                        name: "test".to_string(),
                        attempt: 0,
                    }),
                    image: Some(ImageSpec {
                        image: "nginx:latest".to_string(),
                        annotations: HashMap::new(),
                    }),
                    ..Default::default()
                }),
                sandbox_config: None,
            }))
            .await;
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().code(), tonic::Code::NotFound);
    }

    #[tokio::test]
    async fn test_create_container_missing_config() {
        let svc = make_test_service();
        svc.store.sandboxes.add(test_sandbox("sb-1")).await;

        let result = svc
            .create_container(Request::new(CreateContainerRequest {
                pod_sandbox_id: "sb-1".to_string(),
                config: None,
                sandbox_config: None,
            }))
            .await;
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().code(), tonic::Code::InvalidArgument);
    }

    #[tokio::test]
    async fn test_create_container_missing_metadata() {
        let svc = make_test_service();
        svc.store.sandboxes.add(test_sandbox("sb-1")).await;

        let result = svc
            .create_container(Request::new(CreateContainerRequest {
                pod_sandbox_id: "sb-1".to_string(),
                config: Some(ContainerConfig {
                    metadata: None,
                    ..Default::default()
                }),
                sandbox_config: None,
            }))
            .await;
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().code(), tonic::Code::InvalidArgument);
    }

    #[tokio::test]
    async fn test_create_container_success() {
        let svc = make_test_service();
        svc.store.sandboxes.add(test_sandbox("sb-1")).await;

        let resp = svc
            .create_container(Request::new(CreateContainerRequest {
                pod_sandbox_id: "sb-1".to_string(),
                config: Some(ContainerConfig {
                    metadata: Some(ContainerMetadata {
                        name: "my-container".to_string(),
                        attempt: 0,
                    }),
                    image: Some(ImageSpec {
                        image: "nginx:latest".to_string(),
                        annotations: HashMap::new(),
                    }),
                    ..Default::default()
                }),
                sandbox_config: None,
            }))
            .await
            .unwrap()
            .into_inner();

        assert!(!resp.container_id.is_empty());

        // Verify container is in the store
        let c = svc.store.containers.get(&resp.container_id).await.unwrap();
        assert_eq!(c.name, "my-container");
        assert_eq!(c.sandbox_id, "sb-1");
        assert_eq!(c.state, ContainerState::Created);
    }

    #[tokio::test]
    async fn test_start_container_not_found() {
        let svc = make_test_service();
        let result = svc
            .start_container(Request::new(StartContainerRequest {
                container_id: "nonexistent".to_string(),
            }))
            .await;
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().code(), tonic::Code::NotFound);
    }

    #[tokio::test]
    async fn test_start_container_success() {
        let svc = make_test_service();
        svc.store
            .containers
            .add(test_container("c-1", "sb-1"))
            .await;

        svc.start_container(Request::new(StartContainerRequest {
            container_id: "c-1".to_string(),
        }))
        .await
        .unwrap();

        let c = svc.store.containers.get("c-1").await.unwrap();
        assert_eq!(c.state, ContainerState::Running);
        assert!(c.started_at > 0);
    }

    #[tokio::test]
    async fn test_stop_container() {
        let svc = make_test_service();
        svc.store
            .containers
            .add(test_container("c-1", "sb-1"))
            .await;
        svc.store
            .containers
            .mark_started("c-1", 2_000_000_000)
            .await;

        svc.stop_container(Request::new(StopContainerRequest {
            container_id: "c-1".to_string(),
            timeout: 0,
        }))
        .await
        .unwrap();

        let c = svc.store.containers.get("c-1").await.unwrap();
        assert_eq!(c.state, ContainerState::Exited);
        assert!(c.finished_at > 0);
        assert_eq!(c.exit_code, 0);
    }

    #[tokio::test]
    async fn test_remove_container() {
        let svc = make_test_service();
        svc.store
            .containers
            .add(test_container("c-1", "sb-1"))
            .await;

        svc.remove_container(Request::new(RemoveContainerRequest {
            container_id: "c-1".to_string(),
        }))
        .await
        .unwrap();

        assert!(svc.store.containers.get("c-1").await.is_none());
    }

    // ── Container Status ─────────────────────────────────────────────

    #[tokio::test]
    async fn test_container_status_not_found() {
        let svc = make_test_service();
        let result = svc
            .container_status(Request::new(ContainerStatusRequest {
                container_id: "nonexistent".to_string(),
                verbose: false,
            }))
            .await;
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().code(), tonic::Code::NotFound);
    }

    #[tokio::test]
    async fn test_container_status_created() {
        let svc = make_test_service();
        svc.store
            .containers
            .add(test_container("c-1", "sb-1"))
            .await;

        let resp = svc
            .container_status(Request::new(ContainerStatusRequest {
                container_id: "c-1".to_string(),
                verbose: false,
            }))
            .await
            .unwrap()
            .into_inner();

        let status = resp.status.unwrap();
        assert_eq!(status.id, "c-1");
        assert_eq!(
            status.state(),
            crate::cri_api::ContainerState::ContainerCreated
        );
        assert_eq!(status.image_ref, "nginx:latest");
    }

    #[tokio::test]
    async fn test_container_status_running() {
        let svc = make_test_service();
        svc.store
            .containers
            .add(test_container("c-1", "sb-1"))
            .await;
        svc.store
            .containers
            .mark_started("c-1", 2_000_000_000)
            .await;

        let resp = svc
            .container_status(Request::new(ContainerStatusRequest {
                container_id: "c-1".to_string(),
                verbose: false,
            }))
            .await
            .unwrap()
            .into_inner();

        let status = resp.status.unwrap();
        assert_eq!(
            status.state(),
            crate::cri_api::ContainerState::ContainerRunning
        );
        assert_eq!(status.started_at, 2_000_000_000);
    }

    // ── List Containers ──────────────────────────────────────────────

    #[tokio::test]
    async fn test_list_containers_empty() {
        let svc = make_test_service();
        let resp = svc
            .list_containers(Request::new(ListContainersRequest { filter: None }))
            .await
            .unwrap()
            .into_inner();
        assert!(resp.containers.is_empty());
    }

    #[tokio::test]
    async fn test_list_containers_filter_by_sandbox() {
        let svc = make_test_service();
        svc.store
            .containers
            .add(test_container("c-1", "sb-1"))
            .await;
        svc.store
            .containers
            .add(test_container("c-2", "sb-1"))
            .await;
        svc.store
            .containers
            .add(test_container("c-3", "sb-2"))
            .await;

        let resp = svc
            .list_containers(Request::new(ListContainersRequest {
                filter: Some(ContainerFilter {
                    id: String::new(),
                    state: None,
                    pod_sandbox_id: "sb-1".to_string(),
                    label_selector: HashMap::new(),
                }),
            }))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(resp.containers.len(), 2);
    }

    #[tokio::test]
    async fn test_list_containers_filter_by_id() {
        let svc = make_test_service();
        svc.store
            .containers
            .add(test_container("c-1", "sb-1"))
            .await;
        svc.store
            .containers
            .add(test_container("c-2", "sb-1"))
            .await;

        let resp = svc
            .list_containers(Request::new(ListContainersRequest {
                filter: Some(ContainerFilter {
                    id: "c-1".to_string(),
                    state: None,
                    pod_sandbox_id: String::new(),
                    label_selector: HashMap::new(),
                }),
            }))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(resp.containers.len(), 1);
        assert_eq!(resp.containers[0].id, "c-1");
    }

    // ── UpdateContainerResources ─────────────────────────────────────

    #[tokio::test]
    async fn test_update_container_resources_not_found() {
        let svc = make_test_service();
        let result = svc
            .update_container_resources(Request::new(UpdateContainerResourcesRequest {
                container_id: "nonexistent".to_string(),
                linux: None,
                annotations: HashMap::new(),
            }))
            .await;
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().code(), tonic::Code::NotFound);
    }

    #[tokio::test]
    async fn test_update_container_resources_no_linux() {
        let svc = make_test_service();
        svc.store
            .containers
            .add(test_container("c-1", "sb-1"))
            .await;

        let result = svc
            .update_container_resources(Request::new(UpdateContainerResourcesRequest {
                container_id: "c-1".to_string(),
                linux: None,
                annotations: HashMap::new(),
            }))
            .await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_update_container_resources_linux_rejected() {
        let svc = make_test_service();
        svc.store
            .containers
            .add(test_container("c-1", "sb-1"))
            .await;

        let result = svc
            .update_container_resources(Request::new(UpdateContainerResourcesRequest {
                container_id: "c-1".to_string(),
                linux: Some(LinuxContainerResources {
                    cpu_quota: 100_000,
                    memory_limit_in_bytes: 1024 * 1024 * 512,
                    ..Default::default()
                }),
                annotations: HashMap::new(),
            }))
            .await;
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().code(), tonic::Code::Unimplemented);
    }

    // ── ReopenContainerLog ───────────────────────────────────────────

    #[tokio::test]
    async fn test_reopen_container_log_not_found() {
        let svc = make_test_service();
        let result = svc
            .reopen_container_log(Request::new(ReopenContainerLogRequest {
                container_id: "nonexistent".to_string(),
            }))
            .await;
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().code(), tonic::Code::NotFound);
    }

    #[tokio::test]
    async fn test_reopen_container_log_empty_path() {
        let svc = make_test_service();
        svc.store
            .containers
            .add(test_container("c-1", "sb-1"))
            .await;

        // Should succeed even with empty log path (no-op)
        let result = svc
            .reopen_container_log(Request::new(ReopenContainerLogRequest {
                container_id: "c-1".to_string(),
            }))
            .await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_reopen_container_log_truncates_file() {
        let svc = make_test_service();

        let dir = tempfile::tempdir().unwrap();
        let log_path = dir.path().join("container.log");
        std::fs::write(&log_path, "some log content here").unwrap();

        let mut c = test_container("c-1", "sb-1");
        c.log_path = log_path.to_string_lossy().to_string();
        svc.store.containers.add(c).await;

        svc.reopen_container_log(Request::new(ReopenContainerLogRequest {
            container_id: "c-1".to_string(),
        }))
        .await
        .unwrap();

        // File should be truncated
        let content = std::fs::read_to_string(&log_path).unwrap();
        assert!(content.is_empty());
    }

    // ── Stop/Remove Pod Sandbox (store-only, no VM) ──────────────────

    #[tokio::test]
    async fn test_stop_pod_sandbox_no_vm() {
        let svc = make_test_service();
        svc.store.sandboxes.add(test_sandbox("sb-1")).await;
        svc.store
            .containers
            .add(test_container("c-1", "sb-1"))
            .await;
        svc.store
            .containers
            .mark_started("c-1", 2_000_000_000)
            .await;

        svc.stop_pod_sandbox(Request::new(StopPodSandboxRequest {
            pod_sandbox_id: "sb-1".to_string(),
        }))
        .await
        .unwrap();

        // Sandbox should be NotReady
        let sb = svc.store.sandboxes.get("sb-1").await.unwrap();
        assert_eq!(sb.state, SandboxState::NotReady);

        // Container should be Exited
        let c = svc.store.containers.get("c-1").await.unwrap();
        assert_eq!(c.state, ContainerState::Exited);
    }

    #[tokio::test]
    async fn test_remove_pod_sandbox_no_vm() {
        let svc = make_test_service();
        svc.store.sandboxes.add(test_sandbox("sb-1")).await;
        svc.store
            .containers
            .add(test_container("c-1", "sb-1"))
            .await;

        svc.remove_pod_sandbox(Request::new(RemovePodSandboxRequest {
            pod_sandbox_id: "sb-1".to_string(),
        }))
        .await
        .unwrap();

        // Sandbox and containers should be gone
        assert!(svc.store.sandboxes.get("sb-1").await.is_none());
        assert!(svc.store.containers.get("c-1").await.is_none());
    }

    // ── Exec/Attach/PortForward error paths ──────────────────────────

    #[tokio::test]
    async fn test_exec_sync_container_not_found() {
        let svc = make_test_service();
        let result = svc
            .exec_sync(Request::new(ExecSyncRequest {
                container_id: "nonexistent".to_string(),
                cmd: vec!["ls".to_string()],
                timeout: 0,
            }))
            .await;
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().code(), tonic::Code::NotFound);
    }

    #[tokio::test]
    async fn test_exec_sync_sandbox_not_found() {
        let svc = make_test_service();
        // Container exists but no VM for its sandbox
        svc.store
            .containers
            .add(test_container("c-1", "sb-missing"))
            .await;

        let result = svc
            .exec_sync(Request::new(ExecSyncRequest {
                container_id: "c-1".to_string(),
                cmd: vec!["ls".to_string()],
                timeout: 0,
            }))
            .await;
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().code(), tonic::Code::NotFound);
    }

    #[tokio::test]
    async fn test_exec_container_not_found() {
        let svc = make_test_service();
        let result = svc
            .exec(Request::new(ExecRequest {
                container_id: "nonexistent".to_string(),
                cmd: vec!["sh".to_string()],
                tty: false,
                stdin: false,
                stdout: true,
                stderr: true,
            }))
            .await;
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().code(), tonic::Code::NotFound);
    }

    #[tokio::test]
    async fn test_attach_container_not_found() {
        let svc = make_test_service();
        let result = svc
            .attach(Request::new(AttachRequest {
                container_id: "nonexistent".to_string(),
                stdin: false,
                tty: false,
                stdout: true,
                stderr: true,
            }))
            .await;
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().code(), tonic::Code::NotFound);
    }

    #[tokio::test]
    async fn test_port_forward_sandbox_not_found() {
        let svc = make_test_service();
        let result = svc
            .port_forward(Request::new(PortForwardRequest {
                pod_sandbox_id: "nonexistent".to_string(),
                port: vec![8080],
            }))
            .await;
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().code(), tonic::Code::NotFound);
    }

    // ── Warm Pool ────────────────────────────────────────────────────

    #[test]
    fn test_service_without_warm_pool_has_none() {
        let svc = make_test_service();
        assert!(svc.warm_pool.is_none());
    }

    #[tokio::test]
    async fn test_with_warm_pool_attaches_pool() {
        use a3s_box_core::config::{BoxConfig, PoolConfig};
        use a3s_box_core::event::EventEmitter;
        use a3s_box_runtime::pool::WarmPool;

        let pool_config = PoolConfig {
            enabled: true,
            min_idle: 0, // no pre-boot in tests
            max_size: 2,
            idle_ttl_secs: 300,
            ..Default::default()
        };

        let result =
            WarmPool::start(pool_config, BoxConfig::default(), EventEmitter::new(64)).await;

        if let Ok(mut pool) = result {
            let addr: std::net::SocketAddr = "127.0.0.1:0".parse().unwrap();
            let streaming_server = crate::streaming::StreamingServer::new(addr);
            let handle = streaming_server.handle();

            let svc = BoxRuntimeService {
                store: Arc::new(PersistentCriStore::new(Arc::new(NoopStateStore))),
                vm_managers: Arc::new(RwLock::new(HashMap::new())),
                streaming: handle,
                warm_pool: None,
            }
            .with_warm_pool(pool);

            assert!(svc.warm_pool.is_some());
            // Drain pool to clean up
            if let Some(p) = svc.warm_pool {
                let mut pool = p.write().await;
                let _ = pool.drain().await;
            }
        }
        // If WarmPool::start fails (no shim), test is skipped — acceptable in unit test env
    }

    #[tokio::test]
    async fn test_acquire_vm_without_pool_fails_without_shim() {
        // Without a warm pool, acquire_vm cold-boots — which fails in unit test env
        let svc = make_test_service();
        let config = a3s_box_core::config::BoxConfig::default();
        let result = svc.acquire_vm(config).await;
        // Expected: error because no shim binary available
        assert!(result.is_err());
    }
}
