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
use a3s_box_core::{NetworkEndpoint, NetworkMode};
use a3s_box_runtime::network::NetworkStore;
use a3s_box_runtime::oci::{ImageStore, OciImage, OciImageConfig, RegistryAuth};
use a3s_box_runtime::pool::WarmPool;
use a3s_box_runtime::vm::VmManager;

use crate::config_mapper::pod_sandbox_config_to_box_config_with_defaults;
use crate::container::{Container, ContainerState};
use crate::cri_api::runtime_service_server::RuntimeService;
use crate::cri_api::*;
use crate::error::box_error_to_status;
use crate::persistent_store::PersistentCriStore;
use crate::sandbox::{PodSandbox, SandboxState};
#[cfg(test)]
use crate::state::NoopStateStore;
use crate::state::{default_state_path, JsonStateStore, StateStore};
use crate::streaming::{SessionKind, StreamingHandle, StreamingSession};

/// A3S Box implementation of the CRI RuntimeService.
pub struct BoxRuntimeService {
    store: Arc<PersistentCriStore>,
    /// Local OCI image store used for resolving image config defaults.
    image_store: Arc<ImageStore>,
    /// Runtime network configuration last received from kubelet.
    runtime_network: Arc<RwLock<RuntimeNetworkState>>,
    /// Default sandbox/agent image used when pods omit the A3S annotation.
    default_sandbox_image: Option<String>,
    /// Default A3S bridge network used when pods omit the A3S annotation.
    default_sandbox_network: Option<String>,
    /// Maps sandbox_id → VmManager for running VMs.
    vm_managers: Arc<RwLock<HashMap<String, VmManager>>>,
    /// Handle for registering CRI streaming sessions.
    streaming: StreamingHandle,
    /// Optional warm pool for instant VM acquisition.
    warm_pool: Option<Arc<RwLock<WarmPool>>>,
}

#[derive(Clone, Debug, Default)]
struct RuntimeNetworkState {
    pod_cidr: String,
}

impl BoxRuntimeService {
    /// Create a new BoxRuntimeService with JSON-backed persistent state.
    pub fn new(
        image_store: Arc<ImageStore>,
        _auth: RegistryAuth,
        streaming: StreamingHandle,
    ) -> Self {
        let state_store: Arc<dyn StateStore> = Arc::new(JsonStateStore::new(default_state_path()));
        Self::with_state_store(image_store, _auth, streaming, state_store)
    }

    /// Create a BoxRuntimeService with a custom StateStore (used in tests).
    pub fn with_state_store(
        image_store: Arc<ImageStore>,
        _auth: RegistryAuth,
        streaming: StreamingHandle,
        state_store: Arc<dyn StateStore>,
    ) -> Self {
        Self::with_persistent_store(
            image_store,
            _auth,
            streaming,
            Arc::new(PersistentCriStore::new(state_store)),
        )
    }

    /// Create a BoxRuntimeService with a shared persistent CRI store.
    pub fn with_persistent_store(
        image_store: Arc<ImageStore>,
        _auth: RegistryAuth,
        streaming: StreamingHandle,
        store: Arc<PersistentCriStore>,
    ) -> Self {
        Self {
            store,
            image_store,
            runtime_network: Arc::new(RwLock::new(RuntimeNetworkState::default())),
            default_sandbox_image: None,
            default_sandbox_network: None,
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

    /// Set the runtime default sandbox/agent image for pods that do not carry
    /// the A3S-specific image annotation.
    pub fn with_default_sandbox_image(mut self, image: Option<String>) -> Self {
        self.default_sandbox_image = image.filter(|value| !value.trim().is_empty());
        self
    }

    /// Set the runtime default A3S bridge network for pods that do not carry
    /// the A3S-specific network annotation.
    pub fn with_default_sandbox_network(mut self, network: Option<String>) -> Self {
        self.default_sandbox_network = network.filter(|value| !value.trim().is_empty());
        self
    }

    /// Load persisted state from disk. Call once after construction.
    pub async fn load_state(&self) {
        if let Err(e) = self.store.load().await {
            tracing::warn!(error = %e, "Failed to load persisted CRI state — starting fresh");
            return;
        }

        self.reconcile_loaded_state().await;
    }

    /// Reconcile persisted state after a CRI process restart.
    ///
    /// VmManager instances are in-memory only. After restart there are no
    /// manageable VMs, so persisted Ready/Running records must not advertise
    /// live workloads to kubelet.
    async fn reconcile_loaded_state(&self) {
        let ready_sandboxes = self
            .store
            .sandboxes
            .list(None)
            .await
            .into_iter()
            .filter(|sandbox| sandbox.state == SandboxState::Ready)
            .map(|sandbox| sandbox.id)
            .collect::<Vec<_>>();
        for sandbox_id in ready_sandboxes {
            self.store
                .update_sandbox_state(&sandbox_id, SandboxState::NotReady)
                .await;
        }

        let finished_at = chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0);
        let running_containers = self
            .store
            .containers
            .list(None, None)
            .await
            .into_iter()
            .filter(|container| container.state == ContainerState::Running)
            .map(|container| container.id)
            .collect::<Vec<_>>();
        for container_id in running_containers {
            self.store
                .mark_container_exited(&container_id, finished_at, 255)
                .await;
        }
    }

    /// Acquire a VM: from warm pool if available, otherwise cold boot.
    async fn acquire_vm(
        &self,
        box_config: a3s_box_core::config::BoxConfig,
        box_name: &str,
    ) -> Result<(VmManager, Option<NetworkEndpoint>), Status> {
        let bridge_network = match &box_config.network {
            NetworkMode::Bridge { network } => Some(network.clone()),
            _ => None,
        };

        if bridge_network.is_none() {
            if let Some(ref pool) = self.warm_pool {
                let pool = pool.read().await;
                match pool.acquire().await {
                    Ok(vm) => {
                        tracing::debug!(box_id = %vm.box_id(), "Acquired VM from warm pool");
                        return Ok((vm, None));
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "Warm pool acquire failed, falling back to cold boot");
                    }
                }
            }
        }

        // Cold boot
        let event_emitter = EventEmitter::new(256);
        let mut vm = VmManager::new(box_config, event_emitter);
        let network_endpoint = if let Some(network_name) = bridge_network.as_deref() {
            Some(connect_cri_network_endpoint(
                network_name,
                vm.box_id(),
                box_name,
            )?)
        } else {
            None
        };

        if let Err(error) = vm.boot().await {
            if let Some(network_name) = bridge_network.as_deref() {
                disconnect_cri_network_endpoint(network_name, vm.box_id());
            }
            return Err(box_error_to_status(error));
        }

        Ok((vm, network_endpoint))
    }

    /// Load OCI image configuration from the local image store.
    async fn image_config(&self, image_ref: &str) -> Result<Option<OciImageConfig>, Status> {
        if image_ref.is_empty() {
            return Ok(None);
        }

        let Some(stored) = self.image_store.find(image_ref).await else {
            return Ok(None);
        };

        let image = OciImage::from_path(&stored.path).map_err(box_error_to_status)?;
        Ok(Some(image.config().clone()))
    }

    async fn runtime_status_info(&self) -> HashMap<String, String> {
        let sandboxes = self.store.sandboxes.list(None).await;
        let containers = self.store.containers.list(None, None).await;
        let images = self.image_store.list().await;
        let image_bytes = self.image_store.total_size().await;
        let running_vms = self.vm_managers.read().await.len();
        let network = self.runtime_network.read().await.clone();

        let sandbox_ready = sandboxes
            .iter()
            .filter(|sandbox| sandbox.state == SandboxState::Ready)
            .count();
        let container_created = containers
            .iter()
            .filter(|container| container.state == ContainerState::Created)
            .count();
        let container_running = containers
            .iter()
            .filter(|container| container.state == ContainerState::Running)
            .count();
        let container_exited = containers
            .iter()
            .filter(|container| container.state == ContainerState::Exited)
            .count();

        HashMap::from([
            ("a3s.runtime.name".to_string(), "a3s-box".to_string()),
            (
                "a3s.runtime.version".to_string(),
                a3s_box_runtime::VERSION.to_string(),
            ),
            ("a3s.sandbox.total".to_string(), sandboxes.len().to_string()),
            ("a3s.sandbox.ready".to_string(), sandbox_ready.to_string()),
            (
                "a3s.container.total".to_string(),
                containers.len().to_string(),
            ),
            (
                "a3s.container.created".to_string(),
                container_created.to_string(),
            ),
            (
                "a3s.container.running".to_string(),
                container_running.to_string(),
            ),
            (
                "a3s.container.exited".to_string(),
                container_exited.to_string(),
            ),
            ("a3s.vm.running".to_string(), running_vms.to_string()),
            ("a3s.image.count".to_string(), images.len().to_string()),
            ("a3s.image.bytes".to_string(), image_bytes.to_string()),
            (
                "a3s.image.store_dir".to_string(),
                self.image_store.store_dir().display().to_string(),
            ),
            ("a3s.network.pod_cidr".to_string(), network.pod_cidr),
            (
                "a3s.network.default_sandbox_network".to_string(),
                self.default_sandbox_network.clone().unwrap_or_default(),
            ),
        ])
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

fn connect_cri_network_endpoint(
    network_name: &str,
    box_id: &str,
    box_name: &str,
) -> Result<NetworkEndpoint, Status> {
    let store = NetworkStore::default_path().map_err(box_error_to_status)?;
    connect_cri_network_endpoint_in_store(&store, network_name, box_id, box_name)
}

fn connect_cri_network_endpoint_in_store(
    store: &NetworkStore,
    network_name: &str,
    box_id: &str,
    box_name: &str,
) -> Result<NetworkEndpoint, Status> {
    let mut network = store
        .get(network_name)
        .map_err(box_error_to_status)?
        .ok_or_else(|| Status::not_found(format!("Network not found: {}", network_name)))?;

    let endpoint = network
        .connect(box_id, box_name)
        .map_err(|e| Status::failed_precondition(format!("Failed to connect CRI network: {e}")))?;
    store.update(&network).map_err(box_error_to_status)?;

    Ok(endpoint)
}

fn disconnect_cri_network_endpoint(network_name: &str, box_id: &str) {
    let result = (|| -> Result<(), Status> {
        let store = NetworkStore::default_path().map_err(box_error_to_status)?;
        disconnect_cri_network_endpoint_in_store(&store, network_name, box_id)
    })();

    if let Err(error) = result {
        tracing::warn!(
            network = network_name,
            box_id,
            error = %error,
            "Failed to disconnect CRI network endpoint"
        );
    }
}

fn disconnect_cri_network_endpoint_in_store(
    store: &NetworkStore,
    network_name: &str,
    box_id: &str,
) -> Result<(), Status> {
    let Some(mut network) = store.get(network_name).map_err(box_error_to_status)? else {
        return Ok(());
    };
    if network.disconnect(box_id).is_ok() {
        store.update(&network).map_err(box_error_to_status)?;
    }
    Ok(())
}

fn network_ready_condition(default_sandbox_network: Option<&str>) -> RuntimeCondition {
    let Some(network_name) = default_sandbox_network
        .map(str::trim)
        .filter(|network| !network.is_empty())
    else {
        return runtime_condition("NetworkReady", true, "", "");
    };

    match NetworkStore::default_path() {
        Ok(store) => network_ready_condition_in_store(Some(network_name), &store),
        Err(error) => runtime_condition(
            "NetworkReady",
            false,
            "SandboxNetworkStoreError",
            &format!("Failed to open A3S network store: {error}"),
        ),
    }
}

fn network_ready_condition_in_store(
    default_sandbox_network: Option<&str>,
    store: &NetworkStore,
) -> RuntimeCondition {
    let Some(network_name) = default_sandbox_network
        .map(str::trim)
        .filter(|network| !network.is_empty())
    else {
        return runtime_condition("NetworkReady", true, "", "");
    };

    match store.get(network_name) {
        Ok(Some(_)) => runtime_condition("NetworkReady", true, "", ""),
        Ok(None) => runtime_condition(
            "NetworkReady",
            false,
            "SandboxNetworkNotFound",
            &format!("Default sandbox network '{network_name}' was not found"),
        ),
        Err(error) => runtime_condition(
            "NetworkReady",
            false,
            "SandboxNetworkStoreError",
            &format!("Failed to read default sandbox network '{network_name}': {error}"),
        ),
    }
}

fn runtime_condition(r#type: &str, status: bool, reason: &str, message: &str) -> RuntimeCondition {
    RuntimeCondition {
        r#type: r#type.to_string(),
        status,
        reason: reason.to_string(),
        message: message.to_string(),
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
        let box_config = pod_sandbox_config_to_box_config_with_defaults(
            &config,
            self.default_sandbox_image.as_deref(),
            self.default_sandbox_network.as_deref(),
        )
        .map_err(box_error_to_status)?;

        let network_name = match &box_config.network {
            NetworkMode::Bridge { network } => Some(network.clone()),
            _ => None,
        };

        // Acquire VM: from warm pool if available, otherwise cold boot
        let (vm, network_endpoint) = self.acquire_vm(box_config, &metadata.name).await?;
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
            network_name,
            ip_address: network_endpoint.map(|endpoint| endpoint.ip_address.to_string()),
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

        let sandbox = self.store.sandboxes.get(sandbox_id).await;

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

        if let Some(sandbox) = sandbox {
            if let Some(network_name) = sandbox.network_name.as_deref() {
                disconnect_cri_network_endpoint(network_name, sandbox_id);
                self.store
                    .update_sandbox_network(sandbox_id, sandbox.network_name.clone(), None)
                    .await;
            }
        }

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
        let removed_sandbox = self.store.sandboxes.get(sandbox_id).await;

        if let Some(vm) = self.vm_managers.write().await.remove(sandbox_id) {
            self.release_vm(vm).await;
        }

        // Remove all containers
        self.store.remove_containers_by_sandbox(sandbox_id).await;

        // Remove sandbox
        self.store.remove_sandbox(sandbox_id).await;

        if let Some(sandbox) = removed_sandbox {
            if let Some(network_name) = sandbox.network_name.as_deref() {
                disconnect_cri_network_endpoint(network_name, sandbox_id);
            }
        }

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
                ip: sandbox.ip_address.clone().unwrap_or_default(),
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
        let image_config = match self.image_config(&image_ref).await {
            Ok(config) => config,
            Err(status) if config.command.is_empty() => return Err(status),
            Err(status) => {
                tracing::warn!(
                    image = %image_ref,
                    error = %status,
                    "Failed to load image config; using explicit CRI container config only"
                );
                None
            }
        };
        let image_missing = !image_ref.is_empty() && image_config.is_none();

        let mut command = config.command.clone();
        let mut args = config.args.clone();
        if let Some(image_config) = &image_config {
            if command.is_empty() {
                if let Some(entrypoint) = &image_config.entrypoint {
                    command = entrypoint.clone();
                }
            }
            if args.is_empty() {
                if let Some(cmd) = &image_config.cmd {
                    if command.is_empty() {
                        command = cmd.clone();
                    } else {
                        args = cmd.clone();
                    }
                }
            }
        }

        if command.is_empty() && !args.is_empty() {
            command = std::mem::take(&mut args);
        }
        if command.is_empty() {
            if image_missing {
                return Err(Status::not_found(format!(
                    "Image {} not found in local store and no explicit command was provided",
                    image_ref
                )));
            }
            return Err(Status::invalid_argument(
                "no command specified: provide CRI command or use an image with Entrypoint/Cmd",
            ));
        }

        let mut envs = image_config
            .as_ref()
            .map(|c| c.env.clone())
            .unwrap_or_default();
        for kv in &config.envs {
            if let Some(existing) = envs.iter_mut().find(|(key, _)| key == &kv.key) {
                existing.1.clone_from(&kv.value);
            } else {
                envs.push((kv.key.clone(), kv.value.clone()));
            }
        }

        let working_dir = if !config.working_dir.is_empty() {
            Some(config.working_dir.clone())
        } else {
            image_config.as_ref().and_then(|c| c.working_dir.clone())
        };

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
            command,
            args,
            envs,
            working_dir,
            stdin: config.stdin,
            tty: config.tty,
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

        if container.state != ContainerState::Created {
            return Err(Status::failed_precondition(format!(
                "Container {} is not in Created state",
                container_id
            )));
        }

        if container.command.is_empty() {
            return Err(Status::invalid_argument(
                "a3s-box CRI session containers require an explicit command; image entrypoint resolution for secondary containers is not implemented",
            ));
        }
        let cmd = container.session_command();

        // Get the VmManager for this sandbox and execute the configured command.
        let managers = self.vm_managers.read().await;
        let vm = managers.get(&container.sandbox_id).ok_or_else(|| {
            Status::failed_precondition(format!(
                "Sandbox {} is not running (VM not found)",
                container.sandbox_id
            ))
        })?;

        let started_at = chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0);
        self.store
            .mark_container_started(container_id, started_at)
            .await;

        let request = a3s_box_core::exec::ExecRequest {
            cmd,
            timeout_ns: a3s_box_core::exec::DEFAULT_EXEC_TIMEOUT_NS,
            env: container.exec_env(),
            working_dir: container.working_dir.clone(),
            stdin: None,
            user: None,
            streaming: false,
        };

        let output = match vm.exec_request(request).await {
            Ok(output) => output,
            Err(error) => {
                let finished_at = chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0);
                self.store
                    .mark_container_exited(container_id, finished_at, 127)
                    .await;
                return Err(box_error_to_status(error));
            }
        };

        let finished_at = chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0);
        self.store
            .mark_container_exited(container_id, finished_at, output.exit_code)
            .await;

        if output.exit_code != 0 {
            return Err(Status::unknown(format!(
                "Container {} exited with code {}",
                container_id, output.exit_code
            )));
        }

        Ok(Response::new(StartContainerResponse {}))
    }

    async fn stop_container(
        &self,
        request: Request<StopContainerRequest>,
    ) -> Result<Response<StopContainerResponse>, Status> {
        let req = request.into_inner();
        let container_id = &req.container_id;

        tracing::info!(container_id = %container_id, "CRI StopContainer");

        let container = self
            .store
            .containers
            .get(container_id)
            .await
            .ok_or_else(|| Status::not_found(format!("Container not found: {}", container_id)))?;

        if container.state != ContainerState::Running {
            return Ok(Response::new(StopContainerResponse {}));
        }

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

        let container = self
            .store
            .containers
            .get(container_id)
            .await
            .ok_or_else(|| Status::not_found(format!("Container not found: {}", container_id)))?;

        if container.state == ContainerState::Running {
            return Err(Status::failed_precondition(format!(
                "Container {} is running; stop it before removal",
                container_id
            )));
        }

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
        let info = if req.verbose {
            HashMap::from([
                (
                    "a3s.command".to_string(),
                    serde_json::to_string(&container.command).unwrap_or_default(),
                ),
                (
                    "a3s.args".to_string(),
                    serde_json::to_string(&container.args).unwrap_or_default(),
                ),
                (
                    "a3s.env".to_string(),
                    serde_json::to_string(&container.exec_env()).unwrap_or_default(),
                ),
                (
                    "a3s.working_dir".to_string(),
                    container.working_dir.clone().unwrap_or_default(),
                ),
                ("a3s.stdin".to_string(), container.stdin.to_string()),
                ("a3s.tty".to_string(), container.tty.to_string()),
            ])
        } else {
            Default::default()
        };

        Ok(Response::new(ContainerStatusResponse {
            status: Some(status),
            info,
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
        request: Request<StatusRequest>,
    ) -> Result<Response<StatusResponse>, Status> {
        let req = request.into_inner();
        let conditions = vec![
            runtime_condition("RuntimeReady", true, "", ""),
            network_ready_condition(self.default_sandbox_network.as_deref()),
        ];
        let info = if req.verbose {
            self.runtime_status_info().await
        } else {
            Default::default()
        };

        Ok(Response::new(StatusResponse {
            status: Some(RuntimeStatus { conditions }),
            info,
        }))
    }

    async fn update_runtime_config(
        &self,
        request: Request<UpdateRuntimeConfigRequest>,
    ) -> Result<Response<UpdateRuntimeConfigResponse>, Status> {
        let req = request.into_inner();
        let pod_cidr = req
            .runtime_config
            .and_then(|config| config.network_config)
            .map(|network| network.pod_cidr)
            .unwrap_or_default();

        self.runtime_network.write().await.pod_cidr = pod_cidr;
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
    use std::fs;
    use std::net::SocketAddr;
    use std::path::Path;

    use crate::streaming::StreamingServer;

    /// Create a BoxRuntimeService for testing.
    /// Uses NoopStateStore (no disk I/O) and a dummy StreamingHandle.
    fn make_test_service() -> BoxRuntimeService {
        let tmp = tempfile::tempdir().unwrap();
        let image_store = Arc::new(ImageStore::new(tmp.path(), 100 * 1024 * 1024).unwrap());
        make_test_service_with_image_store(image_store)
    }

    fn make_test_service_with_image_store(image_store: Arc<ImageStore>) -> BoxRuntimeService {
        let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
        let streaming_server = StreamingServer::new(addr);
        let handle = streaming_server.handle();

        BoxRuntimeService {
            store: Arc::new(PersistentCriStore::new(Arc::new(NoopStateStore))),
            image_store,
            runtime_network: Arc::new(RwLock::new(RuntimeNetworkState::default())),
            default_sandbox_image: None,
            default_sandbox_network: None,
            vm_managers: Arc::new(RwLock::new(HashMap::new())),
            streaming: handle,
            warm_pool: None,
        }
    }

    fn make_test_service_with_state_store(state_store: Arc<dyn StateStore>) -> BoxRuntimeService {
        let tmp = tempfile::tempdir().unwrap();
        let image_store = Arc::new(ImageStore::new(tmp.path(), 100 * 1024 * 1024).unwrap());
        let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
        let streaming_server = StreamingServer::new(addr);
        let handle = streaming_server.handle();

        BoxRuntimeService::with_state_store(
            image_store,
            RegistryAuth::anonymous(),
            handle,
            state_store,
        )
    }

    fn create_test_oci_image(path: &Path) {
        fs::create_dir_all(path.join("blobs/sha256")).unwrap();
        fs::write(path.join("oci-layout"), r#"{"imageLayoutVersion":"1.0.0"}"#).unwrap();

        let config_content = r#"{
            "architecture": "amd64",
            "os": "linux",
            "config": {
                "Entrypoint": ["/bin/server"],
                "Cmd": ["--listen", "8080"],
                "Env": ["PATH=/usr/bin:/bin", "A3S_IMAGE=1"],
                "WorkingDir": "/srv/app"
            },
            "rootfs": {
                "type": "layers",
                "diff_ids": ["sha256:layer1hash"]
            },
            "history": []
        }"#;
        let config_hash = "configabc123";
        fs::write(path.join("blobs/sha256").join(config_hash), config_content).unwrap();

        let layer_hash = "layerdef456";
        fs::write(path.join("blobs/sha256").join(layer_hash), b"test layer").unwrap();

        let manifest_content = format!(
            r#"{{
                "schemaVersion": 2,
                "mediaType": "application/vnd.oci.image.manifest.v1+json",
                "config": {{
                    "mediaType": "application/vnd.oci.image.config.v1+json",
                    "digest": "sha256:{}",
                    "size": {}
                }},
                "layers": [
                    {{
                        "mediaType": "application/vnd.oci.image.layer.v1.tar+gzip",
                        "digest": "sha256:{}",
                        "size": 10
                    }}
                ]
            }}"#,
            config_hash,
            config_content.len(),
            layer_hash
        );
        let manifest_hash = "manifestxyz789";
        fs::write(
            path.join("blobs/sha256").join(manifest_hash),
            &manifest_content,
        )
        .unwrap();

        let index_content = format!(
            r#"{{
                "schemaVersion": 2,
                "mediaType": "application/vnd.oci.image.index.v1+json",
                "manifests": [
                    {{
                        "mediaType": "application/vnd.oci.image.manifest.v1+json",
                        "digest": "sha256:{}",
                        "size": {}
                    }}
                ]
            }}"#,
            manifest_hash,
            manifest_content.len()
        );
        fs::write(path.join("index.json"), index_content).unwrap();
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
            network_name: None,
            ip_address: None,
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
            command: vec!["echo".to_string()],
            args: vec!["hello".to_string()],
            envs: vec![],
            working_dir: None,
            stdin: false,
            tty: false,
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

    #[tokio::test]
    async fn test_load_state_reconciles_stale_running_state() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        let state_store: Arc<dyn StateStore> = Arc::new(JsonStateStore::new(&path));

        let svc = make_test_service_with_state_store(state_store.clone());
        svc.store.add_sandbox(test_sandbox("sb-1")).await;
        svc.store.add_container(test_container("c-1", "sb-1")).await;
        svc.store.mark_container_started("c-1", 2_000_000_000).await;

        let reloaded = make_test_service_with_state_store(state_store);
        reloaded.load_state().await;

        let sandbox = reloaded.store.sandboxes.get("sb-1").await.unwrap();
        assert_eq!(sandbox.state, SandboxState::NotReady);

        let container = reloaded.store.containers.get("c-1").await.unwrap();
        assert_eq!(container.state, ContainerState::Exited);
        assert_eq!(container.exit_code, 255);
        assert!(container.finished_at > 0);
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
        assert!(resp.info.is_empty());
    }

    #[test]
    fn test_network_ready_without_default_network() {
        let dir = tempfile::tempdir().unwrap();
        let store = NetworkStore::new(dir.path().join("networks.json"));

        let condition = network_ready_condition_in_store(None, &store);

        assert_eq!(condition.r#type, "NetworkReady");
        assert!(condition.status);
        assert!(condition.reason.is_empty());
    }

    #[test]
    fn test_network_ready_with_existing_default_network() {
        let dir = tempfile::tempdir().unwrap();
        let store = NetworkStore::new(dir.path().join("networks.json"));
        store
            .create(a3s_box_core::NetworkConfig::new("k8s-pods", "10.88.0.0/24").unwrap())
            .unwrap();

        let condition = network_ready_condition_in_store(Some("k8s-pods"), &store);

        assert!(condition.status);
        assert!(condition.reason.is_empty());
    }

    #[test]
    fn test_network_not_ready_when_default_network_missing() {
        let dir = tempfile::tempdir().unwrap();
        let store = NetworkStore::new(dir.path().join("networks.json"));

        let condition = network_ready_condition_in_store(Some("missing-network"), &store);

        assert_eq!(condition.r#type, "NetworkReady");
        assert!(!condition.status);
        assert_eq!(condition.reason, "SandboxNetworkNotFound");
        assert!(condition.message.contains("missing-network"));
    }

    #[tokio::test]
    async fn test_status_verbose_includes_runtime_summary() {
        let svc = make_test_service().with_default_sandbox_network(Some("k8s-pods".to_string()));
        svc.store.add_sandbox(test_sandbox("sb-1")).await;
        svc.store.add_container(test_container("c-1", "sb-1")).await;

        let resp = svc
            .status(Request::new(StatusRequest { verbose: true }))
            .await
            .unwrap()
            .into_inner();

        assert_eq!(
            resp.info.get("a3s.runtime.name").map(String::as_str),
            Some("a3s-box")
        );
        assert_eq!(
            resp.info.get("a3s.sandbox.total").map(String::as_str),
            Some("1")
        );
        assert_eq!(
            resp.info.get("a3s.sandbox.ready").map(String::as_str),
            Some("1")
        );
        assert_eq!(
            resp.info.get("a3s.container.total").map(String::as_str),
            Some("1")
        );
        assert_eq!(
            resp.info.get("a3s.container.created").map(String::as_str),
            Some("1")
        );
        assert!(resp.info.contains_key("a3s.image.store_dir"));
        assert_eq!(
            resp.info
                .get("a3s.network.default_sandbox_network")
                .map(String::as_str),
            Some("k8s-pods")
        );
    }

    // ── UpdateRuntimeConfig ──────────────────────────────────────────

    #[tokio::test]
    async fn test_update_runtime_config() {
        let svc = make_test_service();
        let result = svc
            .update_runtime_config(Request::new(UpdateRuntimeConfigRequest {
                runtime_config: Some(RuntimeConfig {
                    network_config: Some(NetworkConfig {
                        pod_cidr: "10.42.0.0/24".to_string(),
                    }),
                }),
            }))
            .await;
        assert!(result.is_ok());

        let status = svc
            .status(Request::new(StatusRequest { verbose: true }))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(
            status.info.get("a3s.network.pod_cidr").map(String::as_str),
            Some("10.42.0.0/24")
        );
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
    async fn test_pod_sandbox_status_reports_network_ip() {
        let svc = make_test_service();
        let mut sandbox = test_sandbox("sb-1");
        sandbox.network_name = Some("k8s-pods".to_string());
        sandbox.ip_address = Some("10.88.0.2".to_string());
        svc.store.sandboxes.add(sandbox).await;

        let resp = svc
            .pod_sandbox_status(Request::new(PodSandboxStatusRequest {
                pod_sandbox_id: "sb-1".to_string(),
                verbose: false,
            }))
            .await
            .unwrap()
            .into_inner();

        let status = resp.status.unwrap();
        assert_eq!(status.network.unwrap().ip, "10.88.0.2");
    }

    #[test]
    fn test_cri_network_endpoint_connect_and_disconnect() {
        let tmp = tempfile::tempdir().unwrap();
        let store = NetworkStore::new(tmp.path().join("networks.json"));
        store
            .create(a3s_box_core::NetworkConfig::new("k8s-pods", "10.88.0.0/24").unwrap())
            .unwrap();

        let endpoint =
            connect_cri_network_endpoint_in_store(&store, "k8s-pods", "sb-1", "pod-sb-1").unwrap();

        assert_eq!(endpoint.ip_address.to_string(), "10.88.0.2");
        let network = store.get("k8s-pods").unwrap().unwrap();
        assert!(network.endpoints.contains_key("sb-1"));

        disconnect_cri_network_endpoint_in_store(&store, "k8s-pods", "sb-1").unwrap();
        let network = store.get("k8s-pods").unwrap().unwrap();
        assert!(!network.endpoints.contains_key("sb-1"));
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
                    command: vec!["echo".to_string()],
                    args: vec!["hello".to_string()],
                    envs: vec![KeyValue {
                        key: "A3S_TEST".to_string(),
                        value: "1".to_string(),
                    }],
                    working_dir: "/workspace".to_string(),
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
    async fn test_create_container_missing_image_without_command() {
        let svc = make_test_service();
        svc.store.sandboxes.add(test_sandbox("sb-1")).await;

        let result = svc
            .create_container(Request::new(CreateContainerRequest {
                pod_sandbox_id: "sb-1".to_string(),
                config: Some(ContainerConfig {
                    metadata: Some(ContainerMetadata {
                        name: "my-container".to_string(),
                        attempt: 0,
                    }),
                    image: Some(ImageSpec {
                        image: "missing:latest".to_string(),
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
                    command: vec!["echo".to_string()],
                    args: vec!["hello".to_string()],
                    envs: vec![KeyValue {
                        key: "A3S_TEST".to_string(),
                        value: "1".to_string(),
                    }],
                    working_dir: "/workspace".to_string(),
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
        assert_eq!(c.session_command(), vec!["echo", "hello"]);
        assert_eq!(c.envs, vec![("A3S_TEST".to_string(), "1".to_string())]);
        assert_eq!(c.working_dir.as_deref(), Some("/workspace"));
    }

    #[tokio::test]
    async fn test_create_container_resolves_image_config_defaults() {
        let store_tmp = tempfile::tempdir().unwrap();
        let image_store = Arc::new(ImageStore::new(store_tmp.path(), 100 * 1024 * 1024).unwrap());
        let image_tmp = tempfile::tempdir().unwrap();
        create_test_oci_image(image_tmp.path());
        image_store
            .put("nginx:latest", "sha256:testimage001", image_tmp.path())
            .await
            .unwrap();

        let svc = make_test_service_with_image_store(image_store);
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
                    envs: vec![
                        KeyValue {
                            key: "PATH".to_string(),
                            value: "/custom/bin".to_string(),
                        },
                        KeyValue {
                            key: "A3S_CRI".to_string(),
                            value: "1".to_string(),
                        },
                    ],
                    ..Default::default()
                }),
                sandbox_config: None,
            }))
            .await
            .unwrap()
            .into_inner();

        let c = svc.store.containers.get(&resp.container_id).await.unwrap();
        assert_eq!(c.command, vec!["/bin/server"]);
        assert_eq!(c.args, vec!["--listen", "8080"]);
        assert_eq!(c.session_command(), vec!["/bin/server", "--listen", "8080"]);
        assert_eq!(c.working_dir.as_deref(), Some("/srv/app"));
        assert_eq!(
            c.envs,
            vec![
                ("PATH".to_string(), "/custom/bin".to_string()),
                ("A3S_IMAGE".to_string(), "1".to_string()),
                ("A3S_CRI".to_string(), "1".to_string())
            ]
        );
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
    async fn test_start_container_missing_vm() {
        let svc = make_test_service();
        svc.store
            .containers
            .add(test_container("c-1", "sb-1"))
            .await;

        let result = svc
            .start_container(Request::new(StartContainerRequest {
                container_id: "c-1".to_string(),
            }))
            .await;

        assert!(result.is_err());
        assert_eq!(result.unwrap_err().code(), tonic::Code::FailedPrecondition);
        let c = svc.store.containers.get("c-1").await.unwrap();
        assert_eq!(c.state, ContainerState::Created);
        assert_eq!(c.started_at, 0);
    }

    #[tokio::test]
    async fn test_start_container_requires_explicit_command() {
        let svc = make_test_service();
        let mut container = test_container("c-1", "sb-1");
        container.command.clear();
        svc.store.containers.add(container).await;

        let result = svc
            .start_container(Request::new(StartContainerRequest {
                container_id: "c-1".to_string(),
            }))
            .await;

        assert!(result.is_err());
        assert_eq!(result.unwrap_err().code(), tonic::Code::InvalidArgument);
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
    async fn test_stop_container_not_found() {
        let svc = make_test_service();

        let result = svc
            .stop_container(Request::new(StopContainerRequest {
                container_id: "missing".to_string(),
                timeout: 0,
            }))
            .await;

        assert!(result.is_err());
        assert_eq!(result.unwrap_err().code(), tonic::Code::NotFound);
    }

    #[tokio::test]
    async fn test_stop_container_already_exited_is_noop() {
        let svc = make_test_service();
        svc.store
            .containers
            .add(test_container("c-1", "sb-1"))
            .await;
        svc.store
            .containers
            .mark_exited("c-1", 3_000_000_000, 7)
            .await;

        svc.stop_container(Request::new(StopContainerRequest {
            container_id: "c-1".to_string(),
            timeout: 0,
        }))
        .await
        .unwrap();

        let c = svc.store.containers.get("c-1").await.unwrap();
        assert_eq!(c.state, ContainerState::Exited);
        assert_eq!(c.finished_at, 3_000_000_000);
        assert_eq!(c.exit_code, 7);
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

    #[tokio::test]
    async fn test_remove_container_not_found() {
        let svc = make_test_service();

        let result = svc
            .remove_container(Request::new(RemoveContainerRequest {
                container_id: "missing".to_string(),
            }))
            .await;

        assert!(result.is_err());
        assert_eq!(result.unwrap_err().code(), tonic::Code::NotFound);
    }

    #[tokio::test]
    async fn test_remove_running_container_rejected() {
        let svc = make_test_service();
        svc.store
            .containers
            .add(test_container("c-1", "sb-1"))
            .await;
        svc.store
            .containers
            .mark_started("c-1", 2_000_000_000)
            .await;

        let result = svc
            .remove_container(Request::new(RemoveContainerRequest {
                container_id: "c-1".to_string(),
            }))
            .await;

        assert!(result.is_err());
        assert_eq!(result.unwrap_err().code(), tonic::Code::FailedPrecondition);
        assert!(svc.store.containers.get("c-1").await.is_some());
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
    async fn test_container_status_verbose_info() {
        let svc = make_test_service();
        let mut container = test_container("c-1", "sb-1");
        container.envs = vec![("KEY".to_string(), "VALUE".to_string())];
        container.working_dir = Some("/workspace".to_string());
        container.tty = true;
        svc.store.containers.add(container).await;

        let resp = svc
            .container_status(Request::new(ContainerStatusRequest {
                container_id: "c-1".to_string(),
                verbose: true,
            }))
            .await
            .unwrap()
            .into_inner();

        assert_eq!(resp.info.get("a3s.command").unwrap(), r#"["echo"]"#);
        assert_eq!(resp.info.get("a3s.args").unwrap(), r#"["hello"]"#);
        assert_eq!(resp.info.get("a3s.env").unwrap(), r#"["KEY=VALUE"]"#);
        assert_eq!(resp.info.get("a3s.working_dir").unwrap(), "/workspace");
        assert_eq!(resp.info.get("a3s.tty").unwrap(), "true");
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
    async fn test_stop_pod_sandbox_clears_network_ip() {
        let svc = make_test_service();
        let mut sandbox = test_sandbox("sb-1");
        sandbox.network_name = Some("k8s-pods".to_string());
        sandbox.ip_address = Some("10.88.0.2".to_string());
        svc.store.sandboxes.add(sandbox).await;

        svc.stop_pod_sandbox(Request::new(StopPodSandboxRequest {
            pod_sandbox_id: "sb-1".to_string(),
        }))
        .await
        .unwrap();

        let sb = svc.store.sandboxes.get("sb-1").await.unwrap();
        assert_eq!(sb.state, SandboxState::NotReady);
        assert_eq!(sb.network_name.as_deref(), Some("k8s-pods"));
        assert!(sb.ip_address.is_none());
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

        if let Ok(pool) = result {
            let addr: std::net::SocketAddr = "127.0.0.1:0".parse().unwrap();
            let streaming_server = crate::streaming::StreamingServer::new(addr);
            let handle = streaming_server.handle();

            let svc = BoxRuntimeService {
                store: Arc::new(PersistentCriStore::new(Arc::new(NoopStateStore))),
                image_store: Arc::new(
                    ImageStore::new(&tempfile::tempdir().unwrap().path().join("images"), 1024)
                        .unwrap(),
                ),
                runtime_network: Arc::new(RwLock::new(RuntimeNetworkState::default())),
                default_sandbox_image: None,
                default_sandbox_network: None,
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
        let result = svc.acquire_vm(config, "test-pod").await;
        // Expected: error because no shim binary available
        assert!(result.is_err());
    }
}
