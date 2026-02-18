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
use a3s_box_runtime::vm::VmManager;

use crate::config_mapper::pod_sandbox_config_to_box_config;
use crate::container::{Container, ContainerState, ContainerStore};
use crate::cri_api::runtime_service_server::RuntimeService;
use crate::cri_api::*;
use crate::error::box_error_to_status;
use crate::sandbox::{PodSandbox, SandboxState, SandboxStore};
use crate::streaming::{SessionKind, StreamingHandle, StreamingSession};

/// A3S Box implementation of the CRI RuntimeService.
pub struct BoxRuntimeService {
    sandbox_store: Arc<SandboxStore>,
    container_store: Arc<ContainerStore>,
    /// Maps sandbox_id → VmManager for running VMs.
    vm_managers: Arc<RwLock<HashMap<String, VmManager>>>,
    /// Handle for registering CRI streaming sessions.
    streaming: StreamingHandle,
}

impl BoxRuntimeService {
    /// Create a new BoxRuntimeService.
    pub fn new(
        _image_store: Arc<ImageStore>,
        _auth: RegistryAuth,
        streaming: StreamingHandle,
    ) -> Self {
        Self {
            sandbox_store: Arc::new(SandboxStore::new()),
            container_store: Arc::new(ContainerStore::new()),
            vm_managers: Arc::new(RwLock::new(HashMap::new())),
            streaming,
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

        // Create VmManager
        let event_emitter = EventEmitter::new(256);
        let mut vm = VmManager::new(box_config, event_emitter);
        let sandbox_id = vm.box_id().to_string();

        // Boot the VM
        vm.boot().await.map_err(box_error_to_status)?;

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

        self.sandbox_store.add(sandbox).await;
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
        let containers = self.container_store.list(Some(sandbox_id), None).await;
        let now_ns = chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0);
        for c in &containers {
            if c.state != ContainerState::Exited {
                self.container_store.mark_exited(&c.id, now_ns, 137).await;
            }
        }

        // Destroy the VM
        if let Some(mut vm) = self.vm_managers.write().await.remove(sandbox_id) {
            vm.destroy().await.map_err(box_error_to_status)?;
        }

        self.sandbox_store
            .update_state(sandbox_id, SandboxState::NotReady)
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

        // Ensure VM is stopped
        if let Some(mut vm) = self.vm_managers.write().await.remove(sandbox_id) {
            let _ = vm.destroy().await;
        }

        // Remove all containers
        self.container_store.remove_by_sandbox(sandbox_id).await;

        // Remove sandbox
        self.sandbox_store.remove(sandbox_id).await;

        Ok(Response::new(RemovePodSandboxResponse {}))
    }

    async fn pod_sandbox_status(
        &self,
        request: Request<PodSandboxStatusRequest>,
    ) -> Result<Response<PodSandboxStatusResponse>, Status> {
        let req = request.into_inner();
        let sandbox_id = &req.pod_sandbox_id;

        let sandbox = self
            .sandbox_store
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

        let sandboxes = self.sandbox_store.list(label_filter).await;

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
        self.sandbox_store
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

        self.container_store.add(container).await;

        Ok(Response::new(CreateContainerResponse { container_id }))
    }

    async fn start_container(
        &self,
        request: Request<StartContainerRequest>,
    ) -> Result<Response<StartContainerResponse>, Status> {
        let req = request.into_inner();
        let container_id = &req.container_id;

        let container = self
            .container_store
            .get(container_id)
            .await
            .ok_or_else(|| Status::not_found(format!("Container not found: {}", container_id)))?;

        tracing::info!(
            container_id = %container_id,
            sandbox_id = %container.sandbox_id,
            "CRI StartContainer"
        );

        let now_ns = chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0);
        self.container_store
            .mark_started(container_id, now_ns)
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
        self.container_store
            .mark_exited(container_id, now_ns, 0)
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

        self.container_store.remove(container_id).await;

        Ok(Response::new(RemoveContainerResponse {}))
    }

    async fn container_status(
        &self,
        request: Request<ContainerStatusRequest>,
    ) -> Result<Response<ContainerStatusResponse>, Status> {
        let req = request.into_inner();
        let container_id = &req.container_id;

        let container = self
            .container_store
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
            .container_store
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
            .container_store
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
            .container_store
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
            .container_store
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
        self.sandbox_store
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
            .container_store
            .get(container_id)
            .await
            .ok_or_else(|| Status::not_found(format!("Container not found: {}", container_id)))?;

        if let Some(ref linux) = req.linux {
            tracing::info!(
                container_id = %container_id,
                sandbox_id = %container.sandbox_id,
                cpu_quota = linux.cpu_quota,
                cpu_period = linux.cpu_period,
                memory_limit = linux.memory_limit_in_bytes,
                "CRI UpdateContainerResources (acknowledged, microVM resources are fixed at boot)"
            );
        } else {
            tracing::info!(
                container_id = %container_id,
                "CRI UpdateContainerResources (no linux resources specified)"
            );
        }

        // MicroVM resources (CPU, memory) are fixed at boot time and cannot be
        // dynamically resized. We acknowledge the request to maintain CRI compatibility
        // but log that the actual resources remain unchanged.
        Ok(Response::new(UpdateContainerResourcesResponse {}))
    }

    async fn reopen_container_log(
        &self,
        request: Request<ReopenContainerLogRequest>,
    ) -> Result<Response<ReopenContainerLogResponse>, Status> {
        let req = request.into_inner();
        let container_id = &req.container_id;

        let container = self
            .container_store
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
