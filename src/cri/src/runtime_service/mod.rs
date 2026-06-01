//! CRI RuntimeService implementation.
//!
//! Maps CRI pod/container lifecycle to A3S Box VmManager instances.
//! - Pod Sandbox → Box instance (one microVM per pod)
//! - Container → Session within Box

use std::collections::HashMap;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;

use futures::{future::join_all, Stream};
use tokio::sync::{broadcast, oneshot, Notify, RwLock};
use tonic::{Request, Response, Status};

use a3s_box_core::event::EventEmitter;
use a3s_box_runtime::oci::{ImageStore, OciRootfsBuilder, RegistryAuth};
use a3s_box_runtime::pool::WarmPool;
use a3s_box_runtime::vm::VmManager;
use a3s_box_runtime::NetworkStore;

#[cfg(test)]
use crate::config_mapper::ANN_NETWORK;
use crate::config_mapper::{pod_sandbox_config_to_box_config, DEFAULT_AGENT_IMAGE};
use crate::container::{Container, ContainerMount, ContainerState};
use crate::cri_api::runtime_service_server::RuntimeService;
use crate::cri_api::*;
use crate::error::box_error_to_status;
use crate::persistent_store::PersistentCriStore;
use crate::sandbox::{PodSandbox, SandboxState};
#[cfg(test)]
use crate::state::NoopStateStore;
use crate::state::{default_state_path, JsonStateStore, StateStore};
use crate::streaming::{SessionKind, StreamingHandle, StreamingInput, StreamingSession};

mod convert;
mod log_writer;
mod mounts;
mod network;
mod service_ops;
mod stats;
mod supervisor;
#[cfg(test)]
mod tests;

#[cfg(test)]
use convert::ANN_ADDITIONAL_POD_IPS;
use convert::{
    container_event_response, container_exit_reason, container_mount_to_cri, container_state_label,
    container_state_to_cri, container_summary, container_user_from_linux_config,
    ensure_container_image_available, ensure_container_running, ensure_sandbox_ready,
    ensure_vm_ready, merge_env, resolve_command_and_args, resolve_container_mounts,
    sandbox_state_label, sandbox_summary, sanitize_path_component, stop_container_timeout_ms,
    stop_container_wait_duration, ContainerRootfsPaths, ResolvedContainerImage, ANN_POD_IP,
};
#[cfg(test)]
use log_writer::CriLogWriter;
use mounts::materialize_readonly_container_mount;
use network::{
    bridge_network_name, connect_sandbox_to_network_store, default_network_store,
    disconnect_sandbox_from_network_store, sandbox_network_name,
    sandbox_network_status_from_annotations, SandboxNetworkAllocation,
};
use stats::{container_stats, metric_descriptors, pod_sandbox_metrics, pod_sandbox_stats};
use supervisor::{spawn_container_exit_supervisor, ContainerExitSupervisor, SupervisedWorkload};

type CriResponseStream<T> = Pin<Box<dyn Stream<Item = Result<T, Status>> + Send + 'static>>;
type AttachStreamSender = broadcast::Sender<a3s_box_core::exec::ExecEvent>;
type AttachStreamMap = Arc<RwLock<HashMap<String, AttachStreamSender>>>;
type WorkloadStdinSender = StreamingInput;
type WorkloadStdinMap = Arc<RwLock<HashMap<String, WorkloadStdinSender>>>;
type WorkloadStopSender = oneshot::Sender<()>;
type WorkloadStopMap = Arc<RwLock<HashMap<String, WorkloadStopSender>>>;
type LogReopenMap = Arc<RwLock<HashMap<String, Arc<Notify>>>>;
type ContainerEventSender = broadcast::Sender<ContainerEventResponse>;

const CRI_CONTAINER_ROOTFS_HOST_DIR: &str = "cri-container-rootfs";
const CRI_CONTAINER_ROOTFS_GUEST_BASE: &str = "/run/a3s/cri/container-rootfs";
const CONTAINER_EVENT_BUFFER: usize = 1024;

#[derive(Debug, Clone)]
pub struct CriRuntimeOptions {
    pub default_agent_image: String,
    pub runtime_handler_agent_images: HashMap<String, String>,
}

impl Default for CriRuntimeOptions {
    fn default() -> Self {
        Self {
            default_agent_image: DEFAULT_AGENT_IMAGE.to_string(),
            runtime_handler_agent_images: HashMap::new(),
        }
    }
}

impl CriRuntimeOptions {
    pub fn agent_image_for(&self, runtime_handler: &str) -> &str {
        self.runtime_handler_agent_images
            .get(runtime_handler)
            .map(String::as_str)
            .filter(|image| !image.trim().is_empty())
            .unwrap_or(&self.default_agent_image)
    }
}

/// A3S Box implementation of the CRI RuntimeService.
///
/// Cheaply cloneable (all state is `Arc`-shared): the gRPC server takes one
/// clone while a second handle drives graceful shutdown reaping.
#[derive(Clone)]
pub struct BoxRuntimeService {
    store: Arc<PersistentCriStore>,
    /// Shared image store used to resolve image default command metadata.
    image_store: Arc<ImageStore>,
    /// Shared network store used to allocate CRI bridge-network endpoints.
    network_store: Arc<NetworkStore>,
    /// Keeps test image-store temp directories alive without leaking them.
    #[cfg(test)]
    _image_store_tempdir: Option<Arc<tempfile::TempDir>>,
    /// Keeps test network-store temp directories alive without leaking them.
    #[cfg(test)]
    _network_store_tempdir: Option<Arc<tempfile::TempDir>>,
    /// Maps sandbox_id → VmManager for running VMs.
    vm_managers: Arc<RwLock<HashMap<String, VmManager>>>,
    /// Handle for registering CRI streaming sessions.
    streaming: StreamingHandle,
    /// Running container output streams exposed to CRI Attach.
    attach_streams: AttachStreamMap,
    /// Running container stdin sinks exposed to non-TTY CRI Attach.
    workload_stdins: WorkloadStdinMap,
    /// Best-effort stop controls for running CRI container workloads.
    workload_stops: WorkloadStopMap,
    /// Per-container signals for CRI ReopenContainerLog (log rotation).
    log_reopens: LogReopenMap,
    /// Broadcasts CRI container lifecycle events to GetContainerEvents streams.
    container_events: ContainerEventSender,
    /// Optional warm pool for instant VM acquisition.
    warm_pool: Option<Arc<RwLock<WarmPool>>>,
    /// Runtime-level CRI defaults and RuntimeClass overrides.
    runtime_options: CriRuntimeOptions,
    /// Test-only hook for forcing VM acquisition failures without host virtualization.
    #[cfg(test)]
    test_vm_acquire_error: Option<String>,
    /// Test-only hook for attaching RunPodSandbox to a fake exec socket.
    #[cfg(test)]
    test_vm_exec_socket_path: Option<PathBuf>,
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
        Self {
            store: Arc::new(PersistentCriStore::new(state_store)),
            image_store,
            network_store: Arc::new(default_network_store()),
            #[cfg(test)]
            _image_store_tempdir: None,
            #[cfg(test)]
            _network_store_tempdir: None,
            vm_managers: Arc::new(RwLock::new(HashMap::new())),
            streaming,
            attach_streams: Arc::new(RwLock::new(HashMap::new())),
            workload_stdins: Arc::new(RwLock::new(HashMap::new())),
            workload_stops: Arc::new(RwLock::new(HashMap::new())),
            log_reopens: Arc::new(RwLock::new(HashMap::new())),
            container_events: broadcast::channel(CONTAINER_EVENT_BUFFER).0,
            warm_pool: None,
            runtime_options: CriRuntimeOptions::default(),
            #[cfg(test)]
            test_vm_acquire_error: None,
            #[cfg(test)]
            test_vm_exec_socket_path: None,
        }
    }

    /// Override the network store, primarily for embedding and isolated tests.
    pub fn with_network_store(mut self, network_store: NetworkStore) -> Self {
        self.network_store = Arc::new(network_store);
        #[cfg(test)]
        {
            self._network_store_tempdir = None;
        }
        self
    }

    /// Override runtime-level CRI defaults.
    pub fn with_runtime_options(mut self, runtime_options: CriRuntimeOptions) -> Self {
        self.runtime_options = runtime_options;
        self
    }

    /// Attach a warm pool for instant VM acquisition on RunPodSandbox.
    pub fn with_warm_pool(mut self, pool: WarmPool) -> Self {
        self.warm_pool = Some(Arc::new(RwLock::new(pool)));
        self
    }

    /// Load persisted state from disk and reconcile it against live VMs.
    /// Call once after construction.
    pub async fn load_state(&self) {
        if let Err(e) = self.store.load().await {
            tracing::warn!(error = %e, "Failed to load persisted CRI state — starting fresh");
            return;
        }

        // The microVMs do not survive a CRI server restart, and `vm_managers` is
        // empty on a fresh process, so every sandbox/container loaded from disk
        // has no backing VM. Without reconciliation, sandboxes stay
        // `SandboxReady` and containers stay `Running` forever, hiding the
        // restart from the kubelet. Mark orphaned sandboxes `NotReady` and
        // downgrade their not-yet-exited containers to `Exited` (code 255) so the
        // kubelet sees an accurate state and can recreate the pods. Mirrors the
        // existing StopContainer/StopPodSandbox no-VM reconcile.
        let live_sandboxes: std::collections::HashSet<String> = {
            let vm_managers = self.vm_managers.read().await;
            vm_managers.keys().cloned().collect()
        };
        let now_ns = chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0);

        for sandbox in self.store.sandboxes.list(None).await {
            if !live_sandboxes.contains(&sandbox.id) {
                self.store
                    .update_sandbox_state(&sandbox.id, SandboxState::NotReady)
                    .await;
            }
        }

        let mut reconciled = 0usize;
        for container in self.store.containers.list(None, None).await {
            if !live_sandboxes.contains(&container.sandbox_id)
                && container.state != ContainerState::Exited
                && self
                    .store
                    .mark_container_exited(&container.id, now_ns, 255)
                    .await
            {
                reconciled += 1;
            }
        }
        if reconciled > 0 {
            tracing::info!(
                count = reconciled,
                "Reconciled containers without a live VM to Exited after CRI restart"
            );
        }
    }

    /// Tear down every live sandbox VM on CRI shutdown.
    ///
    /// Without this, a SIGTERM kills the CRI process but leaves its sandbox
    /// shim microVMs running (reparented to init) and their overlay mounts
    /// dangling — they orphan and accumulate across restarts. The microVMs do
    /// not survive a CRI restart anyway (`vm_managers` is in-memory and
    /// `load_state` marks everything NotReady), so reaping them on graceful
    /// shutdown is the clean behaviour: `destroy()` kills each shim and unmounts
    /// its overlay, and the container rootfs directory is removed.
    pub async fn shutdown_all_sandboxes(&self) {
        let sandbox_ids: Vec<String> = {
            let vm_managers = self.vm_managers.read().await;
            vm_managers.keys().cloned().collect()
        };
        if sandbox_ids.is_empty() {
            return;
        }
        tracing::info!(
            count = sandbox_ids.len(),
            "Reaping sandbox VMs on CRI shutdown"
        );
        for sandbox_id in sandbox_ids {
            if let Err(e) = self.destroy_sandbox_vm(&sandbox_id, None).await {
                tracing::warn!(
                    sandbox_id = %sandbox_id,
                    error = %e,
                    "Failed to destroy sandbox VM during shutdown"
                );
            }
            self.cleanup_sandbox_rootfs(&sandbox_id).await;
        }
    }
}

#[tonic::async_trait]
impl RuntimeService for BoxRuntimeService {
    type StreamPodSandboxesStream = CriResponseStream<StreamPodSandboxesResponse>;
    type StreamContainersStream = CriResponseStream<StreamContainersResponse>;
    type StreamContainerStatsStream = CriResponseStream<StreamContainerStatsResponse>;
    type StreamPodSandboxStatsStream = CriResponseStream<StreamPodSandboxStatsResponse>;
    type GetContainerEventsStream = CriResponseStream<ContainerEventResponse>;
    type StreamPodSandboxMetricsStream = CriResponseStream<StreamPodSandboxMetricsResponse>;

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
            runtime_handler = %req.runtime_handler,
            "CRI RunPodSandbox"
        );

        // Convert CRI config to BoxConfig. The annotation may override the
        // runtime default, but ordinary Pods do not need A3S-specific fields.
        let agent_image = self.runtime_options.agent_image_for(&req.runtime_handler);
        let mut box_config =
            pod_sandbox_config_to_box_config(&config, agent_image).map_err(box_error_to_status)?;
        let (mut network_ip, additional_ips) =
            sandbox_network_status_from_annotations(&config.annotations)?;
        let rootfs_base = self.ensure_container_rootfs_mount_base().await?;
        box_config.volumes.push(format!(
            "{}:{}:rw",
            rootfs_base.to_string_lossy(),
            CRI_CONTAINER_ROOTFS_GUEST_BASE
        ));
        tracing::debug!(
            agent_image = %box_config.image,
            rootfs_base = %rootfs_base.display(),
            runtime_handler = %req.runtime_handler,
            "Resolved CRI sandbox agent image"
        );

        let sandbox_id = uuid::Uuid::new_v4().to_string();
        let network_allocation = self
            .connect_sandbox_network(&box_config, &sandbox_id, &metadata.name)
            .await?;
        if let Some(allocation) = &network_allocation {
            if network_ip.is_empty() {
                network_ip = allocation.ip.clone();
            } else if network_ip != allocation.ip {
                self.disconnect_sandbox_network_by_name(&allocation.network_name, &sandbox_id)
                    .await;
                return Err(Status::invalid_argument(format!(
                    "Annotation {ANN_POD_IP} value {network_ip} does not match allocated network IP {}",
                    allocation.ip
                )));
            }
        }

        // Acquire VM: from warm pool if available, otherwise cold boot
        let vm = match self
            .acquire_vm_with_box_id(box_config, sandbox_id.clone())
            .await
        {
            Ok(vm) => vm,
            Err(status) => {
                if let Some(allocation) = &network_allocation {
                    self.disconnect_sandbox_network_by_name(&allocation.network_name, &sandbox_id)
                        .await;
                }
                return Err(status);
            }
        };

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
            network_ip,
            additional_ips,
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
        let sandbox = self
            .store
            .sandboxes
            .get(sandbox_id)
            .await
            .ok_or_else(|| Status::not_found(format!("Sandbox not found: {}", sandbox_id)))?;

        if sandbox.state != SandboxState::Ready {
            return Ok(Response::new(StopPodSandboxResponse {}));
        }

        // Stop all containers in this sandbox. Prefer workload-level stop
        // controls so supervised containers can publish their real exit status
        // before the sandbox VM is torn down.
        let containers = self.store.containers.list(Some(sandbox_id), None).await;
        let stop_results = join_all(
            containers
                .iter()
                .filter(|container| container.state == ContainerState::Running)
                .map(|container| async move {
                    let stopped = self.stop_container_workload(container, 0).await?;
                    Ok::<_, Status>((container, stopped))
                }),
        )
        .await;

        for result in stop_results {
            let (container, stopped) = result?;
            if !stopped {
                tracing::debug!(
                    container_id = %container.id,
                    sandbox_id = %container.sandbox_id,
                    "CRI StopPodSandbox falling back to sandbox VM teardown for container workload"
                );
            }
        }

        let now_ns = chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0);
        let containers = self.store.containers.list(Some(sandbox_id), None).await;
        for container in &containers {
            if container.state != ContainerState::Exited {
                let updated = self
                    .store
                    .mark_container_exited(&container.id, now_ns, 137)
                    .await;
                if updated {
                    self.emit_container_event(
                        &container.id,
                        &container.sandbox_id,
                        ContainerEventType::ContainerStoppedEvent,
                        now_ns,
                        "StopPodSandbox",
                        "Container stopped by pod sandbox shutdown",
                    );
                }
            }
        }
        {
            let mut attach_streams = self.attach_streams.write().await;
            let mut workload_stdins = self.workload_stdins.write().await;
            let mut workload_stops = self.workload_stops.write().await;
            let mut log_reopens = self.log_reopens.write().await;
            for container in &containers {
                attach_streams.remove(&container.id);
                workload_stdins.remove(&container.id);
                workload_stops.remove(&container.id);
                log_reopens.remove(&container.id);
            }
        }

        self.destroy_sandbox_vm(sandbox_id, None).await?;
        self.disconnect_sandbox_network(&sandbox).await;

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
        let Some(sandbox) = self.store.sandboxes.get(sandbox_id).await else {
            return Ok(Response::new(RemovePodSandboxResponse {}));
        };

        if sandbox.state == SandboxState::Ready {
            return Err(Status::failed_precondition(format!(
                "RemovePodSandbox requires a stopped sandbox; sandbox {} is Ready",
                sandbox_id
            )));
        }

        // Used sandbox VMs must be destroyed; they are not clean enough to
        // return to the warm pool.
        self.destroy_sandbox_vm(sandbox_id, None).await?;
        self.disconnect_sandbox_network(&sandbox).await;

        // Remove all containers and their prepared rootfs directories.
        let removed_containers = self.store.remove_containers_by_sandbox(sandbox_id).await;
        {
            let mut attach_streams = self.attach_streams.write().await;
            let mut workload_stdins = self.workload_stdins.write().await;
            let mut workload_stops = self.workload_stops.write().await;
            let mut log_reopens = self.log_reopens.write().await;
            for container in &removed_containers {
                attach_streams.remove(&container.id);
                workload_stdins.remove(&container.id);
                workload_stops.remove(&container.id);
                log_reopens.remove(&container.id);
            }
        }
        for container in &removed_containers {
            let event_time = chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0);
            self.emit_container_event(
                &container.id,
                &container.sandbox_id,
                ContainerEventType::ContainerDeletedEvent,
                event_time,
                "ContainerDeleted",
                format!("Container {} removed with pod sandbox", container.name),
            );
            self.cleanup_container_rootfs_path(&container.rootfs_path)
                .await;
        }
        self.cleanup_sandbox_rootfs(sandbox_id).await;

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
        let info = if req.verbose {
            let vm_present = self.vm_managers.read().await.contains_key(sandbox_id);
            let container_count = self
                .store
                .containers
                .list(Some(sandbox_id), None)
                .await
                .len();

            HashMap::from([
                (
                    "sandbox_state".to_string(),
                    sandbox_state_label(sandbox.state).to_string(),
                ),
                ("vm_present".to_string(), vm_present.to_string()),
                ("container_count".to_string(), container_count.to_string()),
                ("network_ip".to_string(), sandbox.network_ip.clone()),
                (
                    "additional_ip_count".to_string(),
                    sandbox.additional_ips.len().to_string(),
                ),
            ])
        } else {
            Default::default()
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
                ip: sandbox.network_ip.clone(),
                additional_ips: sandbox
                    .additional_ips
                    .iter()
                    .map(|ip| PodIp { ip: ip.clone() })
                    .collect(),
            }),
            linux: None,
            labels: sandbox.labels.clone(),
            annotations: sandbox.annotations.clone(),
            runtime_handler: sandbox.runtime_handler.clone(),
        };

        Ok(Response::new(PodSandboxStatusResponse {
            status: Some(status),
            info,
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
            .map(sandbox_summary)
            .collect();

        Ok(Response::new(ListPodSandboxResponse { items }))
    }

    async fn stream_pod_sandboxes(
        &self,
        request: Request<StreamPodSandboxesRequest>,
    ) -> Result<Response<Self::StreamPodSandboxesStream>, Status> {
        let req = request.into_inner();
        let label_filter = req
            .filter
            .as_ref()
            .map(|f| &f.label_selector)
            .filter(|m| !m.is_empty());

        let sandboxes = self.store.sandboxes.list(label_filter).await;
        let pod_sandboxes = sandboxes
            .into_iter()
            .filter(|sb| {
                if let Some(ref filter) = req.filter {
                    if !filter.id.is_empty() && sb.id != filter.id {
                        return false;
                    }
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
            .map(sandbox_summary)
            .collect();
        let response = StreamPodSandboxesResponse { pod_sandboxes };
        let stream: Self::StreamPodSandboxesStream =
            Box::pin(tokio_stream::iter(vec![Ok(response)]));

        Ok(Response::new(stream))
    }

    // ── Container ────────────────────────────────────────────────────

    async fn create_container(
        &self,
        request: Request<CreateContainerRequest>,
    ) -> Result<Response<CreateContainerResponse>, Status> {
        let req = request.into_inner();
        let sandbox_id = &req.pod_sandbox_id;

        // Verify sandbox exists
        let sandbox = self
            .store
            .sandboxes
            .get(sandbox_id)
            .await
            .ok_or_else(|| Status::not_found(format!("Sandbox not found: {}", sandbox_id)))?;
        ensure_sandbox_ready(&sandbox, "CreateContainer")?;

        let config = req
            .config
            .ok_or_else(|| Status::invalid_argument("container config required"))?;

        let metadata = config
            .metadata
            .as_ref()
            .ok_or_else(|| Status::invalid_argument("container metadata required"))?;
        if !config.devices.is_empty() {
            return Err(Status::unimplemented(
                "CRI devices are not yet supported for microVM-backed containers",
            ));
        }
        let mounts = resolve_container_mounts(&config.mounts)?;

        let image_ref = config
            .image
            .as_ref()
            .map(|i| i.image.clone())
            .unwrap_or_default();
        let resolved_image = self.resolve_container_image(&image_ref).await?;
        let image_config = resolved_image.as_ref().map(|image| &image.config);
        let (command, args) = resolve_command_and_args(&config, image_config);
        let image_env = image_config
            .map(|image| image.env.as_slice())
            .unwrap_or(&[]);
        let mut env = merge_env(image_env, &config.envs);
        let working_dir = if config.working_dir.is_empty() {
            image_config
                .and_then(|image| image.working_dir.clone())
                .unwrap_or_default()
        } else {
            config.working_dir.clone()
        };
        // CRI requires run_as_group to be set only alongside run_as_user or
        // run_as_username; otherwise the runtime MUST reject the container.
        if let Some(sc) = config
            .linux
            .as_ref()
            .and_then(|linux| linux.security_context.as_ref())
        {
            if sc.run_as_group.is_some()
                && sc.run_as_user.is_none()
                && sc.run_as_username.is_empty()
            {
                return Err(Status::invalid_argument(
                    "run_as_group must not be set without run_as_user or run_as_username",
                ));
            }
            // Carry CRI SupplementalGroups to guest-init over the env channel;
            // the guest applies them with setgroups before dropping privileges.
            if !sc.supplemental_groups.is_empty() {
                let groups = sc
                    .supplemental_groups
                    .iter()
                    .map(|gid| gid.to_string())
                    .collect::<Vec<_>>()
                    .join(",");
                env.push(("A3S_SEC_SUPPLEMENTAL_GROUPS".to_string(), groups));
            }
            // CRI MaskedPaths/ReadonlyPaths — ':'-separated absolute paths the
            // guest masks (bind /dev/null or ro tmpfs) / remounts read-only
            // inside the container rootfs.
            if !sc.masked_paths.is_empty() {
                env.push(("A3S_SEC_MASKED_PATHS".to_string(), sc.masked_paths.join(":")));
            }
            if !sc.readonly_paths.is_empty() {
                env.push((
                    "A3S_SEC_READONLY_PATHS".to_string(),
                    sc.readonly_paths.join(":"),
                ));
            }
        }
        let user = container_user_from_linux_config(config.linux.as_ref())
            .or_else(|| image_config.and_then(|image| image.user.clone()));
        let (resolved_image_digest, resolved_image_path) = resolved_image
            .as_ref()
            .map(|image| (image.digest.clone(), image.path.clone()))
            .unwrap_or_default();

        tracing::info!(
            sandbox_id = %sandbox_id,
            name = %metadata.name,
            image = %image_ref,
            image_digest = %resolved_image_digest,
            "CRI CreateContainer"
        );

        let container_id = uuid::Uuid::new_v4().to_string();
        let now_ns = chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0);
        let (rootfs_path, rootfs_guest_path) = match resolved_image.as_ref() {
            Some(image) => {
                let paths = self.container_rootfs_paths(sandbox_id, &container_id);
                if let Err(status) = self.prepare_container_rootfs(image, &paths).await {
                    let failed_path = paths.host_path.to_string_lossy().to_string();
                    self.cleanup_container_rootfs_path(&failed_path).await;
                    return Err(status);
                }
                (
                    paths.host_path.to_string_lossy().to_string(),
                    paths.guest_path,
                )
            }
            None => (String::new(), String::new()),
        };
        if !mounts.is_empty() {
            if let Err(status) = self
                .materialize_readonly_container_mounts(&rootfs_path, &mounts)
                .await
            {
                self.cleanup_container_rootfs_path(&rootfs_path).await;
                return Err(status);
            }
        }

        // The CRI container `log_path` is relative to the sandbox's
        // `log_directory`; store the combined path so the supervisor writes —
        // and ContainerStatus reports — the file where the kubelet/critest look
        // (`<log_directory>/<log_path>`).
        let log_path = if config.log_path.is_empty() {
            String::new()
        } else {
            std::path::Path::new(&sandbox.log_directory)
                .join(&config.log_path)
                .to_string_lossy()
                .into_owned()
        };

        let container = Container {
            id: container_id.clone(),
            sandbox_id: sandbox_id.to_string(),
            name: metadata.name.clone(),
            image_ref,
            resolved_image_digest,
            resolved_image_path,
            command,
            args,
            env,
            working_dir,
            user,
            stdin: config.stdin,
            stdin_once: config.stdin_once,
            tty: config.tty,
            mounts,
            state: ContainerState::Created,
            created_at: now_ns,
            started_at: 0,
            finished_at: 0,
            exit_code: 0,
            labels: config.labels.clone(),
            annotations: config.annotations.clone(),
            log_path,
            rootfs_path,
            rootfs_guest_path,
        };

        self.store.add_container(container.clone()).await;
        self.emit_container_event(
            &container.id,
            &container.sandbox_id,
            ContainerEventType::ContainerCreatedEvent,
            container.created_at,
            "ContainerCreated",
            format!("Container {} created", container.name),
        );

        Ok(Response::new(CreateContainerResponse { container_id }))
    }

    async fn start_container(
        &self,
        request: Request<StartContainerRequest>,
    ) -> Result<Response<StartContainerResponse>, Status> {
        let container_id = request.into_inner().container_id;

        let container = self
            .store
            .containers
            .get(&container_id)
            .await
            .ok_or_else(|| Status::not_found(format!("Container not found: {}", container_id)))?;

        tracing::info!(
            container_id = %container_id,
            sandbox_id = %container.sandbox_id,
            "CRI StartContainer"
        );

        match container.state {
            ContainerState::Created => {}
            ContainerState::Running => {
                return Err(Status::failed_precondition(format!(
                    "Container {} is already running",
                    container_id
                )));
            }
            ContainerState::Exited => {
                return Err(Status::failed_precondition(format!(
                    "Container {} has already exited and cannot be restarted",
                    container_id
                )));
            }
        }

        ensure_container_image_available(&container).await?;

        // A container's main process runs until it exits or is stopped — it must
        // NOT be killed by the one-shot exec timeout (`DEFAULT_EXEC_TIMEOUT_NS`).
        // Using the default 5s timeout would kill every long-running container
        // and inject "Process killed: timeout exceeded" into its stderr/logs and
        // any `attach` stream. Run it effectively unbounded; `StopContainer`
        // cancels it explicitly.
        let exec_request = container
            .to_exec_request(u64::MAX)
            .map_err(|e| Status::failed_precondition(format!("Invalid container command: {e}")))?;

        let (exec_socket_path, pty_socket_path) = {
            let managers = self.vm_managers.read().await;
            let vm = managers.get(&container.sandbox_id).ok_or_else(|| {
                Status::failed_precondition(format!(
                    "Sandbox {} is not running (VM not found)",
                    container.sandbox_id
                ))
            })?;

            ensure_vm_ready(vm, "StartContainer", &container.sandbox_id).await?;
            let exec_socket = vm
                .exec_socket_path()
                .map(|path| path.to_path_buf())
                .ok_or_else(|| {
                    Status::internal(format!(
                        "Sandbox {} exec socket is not available",
                        container.sandbox_id
                    ))
                })?;
            let pty_socket = vm.pty_socket_path().map(|path| path.to_path_buf());
            (exec_socket, pty_socket)
        };

        let (workload, stdin_handle) = if container.tty {
            let pty_socket_path = pty_socket_path.ok_or_else(|| {
                Status::internal(format!(
                    "Sandbox {} PTY socket is not available",
                    container.sandbox_id
                ))
            })?;
            let pty_client = a3s_box_runtime::PtyClient::connect(&pty_socket_path)
                .await
                .map_err(|e| {
                    Status::internal(format!("Failed to connect to sandbox PTY server: {}", e))
                })?;
            let pty_request = a3s_box_core::pty::PtyRequest {
                cmd: exec_request.cmd.clone(),
                env: exec_request.env.clone(),
                working_dir: exec_request.working_dir.clone(),
                rootfs: exec_request.rootfs.clone(),
                user: exec_request.user.clone(),
                cols: 80,
                rows: 24,
            };
            let stream = pty_client.start_stream(&pty_request).await.map_err(|e| {
                Status::internal(format!("Failed to start TTY container workload: {}", e))
            })?;
            let stdin_handle = if container.stdin {
                Some(StreamingInput::Pty(stream.input()))
            } else {
                None
            };
            (SupervisedWorkload::Pty(stream), stdin_handle)
        } else {
            let exec_client = a3s_box_runtime::ExecClient::connect(&exec_socket_path)
                .await
                .map_err(|e| {
                    Status::internal(format!("Failed to connect to sandbox exec server: {}", e))
                })?;
            let stream = exec_client.exec_stream(&exec_request).await.map_err(|e| {
                Status::internal(format!("Failed to start container workload: {}", e))
            })?;
            let stdin_handle = if container.stdin {
                Some(StreamingInput::Exec(stream.input()))
            } else {
                None
            };
            (SupervisedWorkload::Exec(stream), stdin_handle)
        };

        let now_ns = chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0);
        let started = self
            .store
            .mark_container_started_if_created(&container_id, now_ns)
            .await;

        let (attach_tx, _) = broadcast::channel(128);
        let (stop_tx, stop_rx) = oneshot::channel();
        let log_reopen = Arc::new(Notify::new());
        if started {
            self.attach_streams
                .write()
                .await
                .insert(container_id.clone(), attach_tx.clone());
            if let Some(stdin_handle) = stdin_handle {
                self.workload_stdins
                    .write()
                    .await
                    .insert(container_id.clone(), stdin_handle);
            }
            self.workload_stops
                .write()
                .await
                .insert(container_id.clone(), stop_tx);
            self.log_reopens
                .write()
                .await
                .insert(container_id.clone(), log_reopen.clone());
            self.emit_container_event(
                &container_id,
                &container.sandbox_id,
                ContainerEventType::ContainerStartedEvent,
                now_ns,
                "ContainerStarted",
                format!("Container {} started", container.name),
            );
        }

        spawn_container_exit_supervisor(ContainerExitSupervisor {
            store: self.store.clone(),
            attach_streams: self.attach_streams.clone(),
            workload_stdins: self.workload_stdins.clone(),
            workload_stops: self.workload_stops.clone(),
            log_reopens: self.log_reopens.clone(),
            container_events: self.container_events.clone(),
            container_id: container_id.clone(),
            sandbox_id: container.sandbox_id.clone(),
            log_path: container.log_path.clone(),
            attach_tx,
            stop_rx,
            log_reopen,
            workload,
        });

        if !started {
            return Err(Status::failed_precondition(format!(
                "Container {} is no longer in the Created state",
                container_id
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

        if container.state == ContainerState::Exited {
            return Ok(Response::new(StopContainerResponse {}));
        }

        let now_ns = chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0);
        match container.state {
            ContainerState::Created => {
                let updated = self
                    .store
                    .mark_container_exited(container_id, now_ns, 0)
                    .await;
                if updated {
                    self.emit_container_event(
                        container_id,
                        &container.sandbox_id,
                        ContainerEventType::ContainerStoppedEvent,
                        now_ns,
                        "StopContainer",
                        "Created container stopped before workload start",
                    );
                }
            }
            ContainerState::Running => {
                if !self
                    .stop_container_workload(&container, req.timeout)
                    .await?
                {
                    if self.has_other_running_containers(&container).await {
                        return Err(Status::failed_precondition(format!(
                            "StopContainer cannot fall back to sandbox VM teardown for container {} while other containers in sandbox {} are still running",
                            container.id, container.sandbox_id
                        )));
                    }
                    self.stop_container_vm(&container, req.timeout).await?;
                    let updated = self
                        .store
                        .mark_container_exited_if_running(container_id, now_ns, 137)
                        .await;
                    if updated {
                        self.emit_container_event(
                            container_id,
                            &container.sandbox_id,
                            ContainerEventType::ContainerStoppedEvent,
                            now_ns,
                            "StopContainer",
                            "Container stopped by sandbox VM teardown fallback",
                        );
                    }
                }
            }
            ContainerState::Exited => {}
        }
        self.attach_streams.write().await.remove(container_id);
        self.workload_stdins.write().await.remove(container_id);
        self.workload_stops.write().await.remove(container_id);
        self.log_reopens.write().await.remove(container_id);

        Ok(Response::new(StopContainerResponse {}))
    }

    async fn remove_container(
        &self,
        request: Request<RemoveContainerRequest>,
    ) -> Result<Response<RemoveContainerResponse>, Status> {
        let req = request.into_inner();
        let container_id = &req.container_id;

        tracing::info!(container_id = %container_id, "CRI RemoveContainer");

        let Some(container) = self.store.containers.get(container_id).await else {
            return Ok(Response::new(RemoveContainerResponse {}));
        };

        if container.state == ContainerState::Running {
            return Err(Status::failed_precondition(format!(
                "RemoveContainer requires a stopped container; container {} is Running",
                container_id
            )));
        }

        if let Some(removed) = self.store.remove_container(container_id).await {
            self.attach_streams.write().await.remove(container_id);
            self.workload_stdins.write().await.remove(container_id);
            self.workload_stops.write().await.remove(container_id);
        self.log_reopens.write().await.remove(container_id);
            let event_time = chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0);
            self.emit_container_event(
                &removed.id,
                &removed.sandbox_id,
                ContainerEventType::ContainerDeletedEvent,
                event_time,
                "ContainerDeleted",
                format!("Container {} removed", removed.name),
            );
            self.cleanup_container_rootfs_path(&removed.rootfs_path)
                .await;
        }

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
        let (reason, message) = match container.state {
            ContainerState::Exited => container_exit_reason(container.exit_code),
            ContainerState::Created | ContainerState::Running => ("", String::new()),
        };
        let info = if req.verbose {
            let vm_present = self
                .vm_managers
                .read()
                .await
                .contains_key(&container.sandbox_id);

            HashMap::from([
                (
                    "container_state".to_string(),
                    container_state_label(container.state).to_string(),
                ),
                ("sandbox_id".to_string(), container.sandbox_id.clone()),
                ("image_ref".to_string(), container.image_ref.clone()),
                (
                    "resolved_image_digest".to_string(),
                    container.resolved_image_digest.clone(),
                ),
                (
                    "resolved_image_path".to_string(),
                    container.resolved_image_path.clone(),
                ),
                ("rootfs_path".to_string(), container.rootfs_path.clone()),
                (
                    "rootfs_guest_path".to_string(),
                    container.rootfs_guest_path.clone(),
                ),
                ("vm_present".to_string(), vm_present.to_string()),
                (
                    "command_count".to_string(),
                    container.command.len().to_string(),
                ),
                ("arg_count".to_string(), container.args.len().to_string()),
                ("env_count".to_string(), container.env.len().to_string()),
                (
                    "mount_count".to_string(),
                    container.mounts.len().to_string(),
                ),
                ("tty".to_string(), container.tty.to_string()),
                ("stdin".to_string(), container.stdin.to_string()),
                ("stdin_once".to_string(), container.stdin_once.to_string()),
            ])
        } else {
            Default::default()
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
            image_ref: container.status_image_ref().to_string(),
            reason: reason.to_string(),
            message,
            labels: container.labels.clone(),
            annotations: container.annotations.clone(),
            mounts: container
                .mounts
                .iter()
                .map(container_mount_to_cri)
                .collect(),
            log_path: container.log_path.clone(),
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
            .map(container_summary)
            .collect();

        Ok(Response::new(ListContainersResponse { containers: items }))
    }

    async fn stream_containers(
        &self,
        request: Request<StreamContainersRequest>,
    ) -> Result<Response<Self::StreamContainersStream>, Status> {
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

        let containers = containers
            .into_iter()
            .filter(|container| {
                if let Some(ref filter) = req.filter {
                    if !filter.id.is_empty() && container.id != filter.id {
                        return false;
                    }
                    if let Some(ref state_val) = filter.state {
                        let state = container_state_to_cri(container.state) as i32;
                        if state_val.state != state {
                            return false;
                        }
                    }
                }
                true
            })
            .map(container_summary)
            .collect();

        let response = StreamContainersResponse { containers };
        let stream: Self::StreamContainersStream = Box::pin(tokio_stream::iter(vec![Ok(response)]));

        Ok(Response::new(stream))
    }

    // ── Status ───────────────────────────────────────────────────────

    async fn status(
        &self,
        request: Request<StatusRequest>,
    ) -> Result<Response<StatusResponse>, Status> {
        let req = request.into_inner();
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
        let info = if req.verbose {
            let sandboxes = self.store.sandboxes.list(None).await;
            let containers = self.store.containers.list(None, None).await;
            let vm_manager_count = self.vm_managers.read().await.len();

            let ready_sandboxes = sandboxes
                .iter()
                .filter(|sandbox| sandbox.state == SandboxState::Ready)
                .count();
            let not_ready_sandboxes = sandboxes.len().saturating_sub(ready_sandboxes);
            let created_containers = containers
                .iter()
                .filter(|container| container.state == ContainerState::Created)
                .count();
            let running_containers = containers
                .iter()
                .filter(|container| container.state == ContainerState::Running)
                .count();
            let exited_containers = containers
                .iter()
                .filter(|container| container.state == ContainerState::Exited)
                .count();

            HashMap::from([
                ("sandbox_count".to_string(), sandboxes.len().to_string()),
                (
                    "sandbox_ready_count".to_string(),
                    ready_sandboxes.to_string(),
                ),
                (
                    "sandbox_not_ready_count".to_string(),
                    not_ready_sandboxes.to_string(),
                ),
                ("container_count".to_string(), containers.len().to_string()),
                (
                    "container_created_count".to_string(),
                    created_containers.to_string(),
                ),
                (
                    "container_running_count".to_string(),
                    running_containers.to_string(),
                ),
                (
                    "container_exited_count".to_string(),
                    exited_containers.to_string(),
                ),
                ("vm_manager_count".to_string(), vm_manager_count.to_string()),
                (
                    "warm_pool_enabled".to_string(),
                    self.warm_pool.is_some().to_string(),
                ),
            ])
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
        _request: Request<UpdateRuntimeConfigRequest>,
    ) -> Result<Response<UpdateRuntimeConfigResponse>, Status> {
        // Accept but ignore runtime config updates for now
        Ok(Response::new(UpdateRuntimeConfigResponse {}))
    }

    async fn runtime_config(
        &self,
        _request: Request<RuntimeConfigRequest>,
    ) -> Result<Response<RuntimeConfigResponse>, Status> {
        Ok(Response::new(RuntimeConfigResponse {
            linux: Some(LinuxRuntimeConfiguration {
                cgroup_driver: CgroupDriver::Cgroupfs as i32,
            }),
        }))
    }

    async fn update_pod_sandbox_resources(
        &self,
        request: Request<UpdatePodSandboxResourcesRequest>,
    ) -> Result<Response<UpdatePodSandboxResourcesResponse>, Status> {
        let req = request.into_inner();
        let sandbox = self
            .store
            .sandboxes
            .get(&req.pod_sandbox_id)
            .await
            .ok_or_else(|| {
                Status::not_found(format!("Sandbox not found: {}", req.pod_sandbox_id))
            })?;
        ensure_sandbox_ready(&sandbox, "UpdatePodSandboxResources")?;

        tracing::info!(
            sandbox_id = %req.pod_sandbox_id,
            "CRI UpdatePodSandboxResources accepted as a no-op; pod-level VM resizing is not supported yet"
        );

        Ok(Response::new(UpdatePodSandboxResourcesResponse {}))
    }

    async fn checkpoint_container(
        &self,
        request: Request<CheckpointContainerRequest>,
    ) -> Result<Response<CheckpointContainerResponse>, Status> {
        let req = request.into_inner();
        Err(Status::unimplemented(format!(
            "CheckpointContainer is not supported for microVM-backed container {}",
            req.container_id
        )))
    }

    async fn get_container_events(
        &self,
        _request: Request<GetEventsRequest>,
    ) -> Result<Response<Self::GetContainerEventsStream>, Status> {
        let receiver = self.container_events.subscribe();
        let stream = futures::stream::unfold(receiver, |mut receiver| async move {
            loop {
                match receiver.recv().await {
                    Ok(event) => return Some((Ok(event), receiver)),
                    Err(broadcast::error::RecvError::Lagged(skipped)) => {
                        tracing::warn!(
                            skipped,
                            "CRI container event stream lagged and dropped events"
                        );
                    }
                    Err(broadcast::error::RecvError::Closed) => return None,
                }
            }
        });
        let stream: Self::GetContainerEventsStream = Box::pin(stream);
        Ok(Response::new(stream))
    }

    async fn list_metric_descriptors(
        &self,
        _request: Request<ListMetricDescriptorsRequest>,
    ) -> Result<Response<ListMetricDescriptorsResponse>, Status> {
        Ok(Response::new(ListMetricDescriptorsResponse {
            descriptors: metric_descriptors(),
        }))
    }

    async fn list_pod_sandbox_metrics(
        &self,
        request: Request<ListPodSandboxMetricsRequest>,
    ) -> Result<Response<ListPodSandboxMetricsResponse>, Status> {
        let req = request.into_inner();
        let label_filter = req
            .filter
            .as_ref()
            .map(|filter| &filter.label_selector)
            .filter(|labels| !labels.is_empty());
        let sandboxes = self.store.sandboxes.list(label_filter).await;
        let vm_manager_ids: std::collections::HashSet<String> =
            self.vm_managers.read().await.keys().cloned().collect();

        let mut metrics = Vec::new();
        for sandbox in sandboxes {
            if let Some(ref filter) = req.filter {
                if !filter.id.is_empty() && sandbox.id != filter.id {
                    continue;
                }
            }

            let containers = self.store.containers.list(Some(&sandbox.id), None).await;
            metrics.push(pod_sandbox_metrics(
                &sandbox,
                &containers,
                vm_manager_ids.contains(&sandbox.id),
            ));
        }

        Ok(Response::new(ListPodSandboxMetricsResponse {
            pod_sandbox_metrics: metrics,
        }))
    }

    async fn stream_pod_sandbox_metrics(
        &self,
        request: Request<StreamPodSandboxMetricsRequest>,
    ) -> Result<Response<Self::StreamPodSandboxMetricsStream>, Status> {
        let req = request.into_inner();
        let response = self
            .list_pod_sandbox_metrics(Request::new(ListPodSandboxMetricsRequest {
                filter: req.filter,
            }))
            .await?
            .into_inner();
        let stream: Self::StreamPodSandboxMetricsStream = Box::pin(tokio_stream::iter(vec![Ok(
            StreamPodSandboxMetricsResponse {
                pod_sandbox_metrics: response.pod_sandbox_metrics,
            },
        )]));
        Ok(Response::new(stream))
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

        if req.cmd.is_empty() {
            return Err(Status::invalid_argument(
                "ExecSync command must contain at least one argument",
            ));
        }

        // Look up the container to find its sandbox
        let container = self
            .store
            .containers
            .get(container_id)
            .await
            .ok_or_else(|| Status::not_found(format!("Container not found: {}", container_id)))?;
        ensure_container_running(&container, "ExecSync")?;
        ensure_container_image_available(&container).await?;

        // Get the VmManager for this sandbox
        let vm_managers = self.vm_managers.read().await;
        let vm = vm_managers.get(&container.sandbox_id).ok_or_else(|| {
            Status::not_found(format!("Sandbox not found: {}", container.sandbox_id))
        })?;
        ensure_vm_ready(vm, "ExecSync", &container.sandbox_id).await?;

        // Execute the command via the exec client
        let timeout_ns = if req.timeout > 0 {
            req.timeout as u64 * 1_000_000_000
        } else {
            a3s_box_core::exec::DEFAULT_EXEC_TIMEOUT_NS
        };

        let exec_request = a3s_box_core::exec::ExecRequest {
            cmd: req.cmd,
            timeout_ns,
            // Inherit the container's security envelope (A3S_SEC_*, e.g.
            // SupplementalGroups) so ExecSync probes (`id`) observe the same
            // privileges the main process runs with.
            env: container
                .env
                .iter()
                .filter(|(key, _)| key.starts_with("A3S_SEC_"))
                .map(|(key, value)| format!("{key}={value}"))
                .collect(),
            working_dir: None,
            rootfs: if container.rootfs_guest_path.is_empty() {
                None
            } else {
                Some(container.rootfs_guest_path.clone())
            },
            stdin: None,
            stdin_streaming: false,
            // ExecSync runs in the container, so it inherits the container's
            // configured user (RunAsUser/RunAsGroup) — not root.
            user: container.user.clone(),
            streaming: false,
        };
        let output = vm
            .exec_request(&exec_request)
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

        if req.cmd.is_empty() {
            return Err(Status::invalid_argument(
                "Exec command must contain at least one argument",
            ));
        }

        // Look up the container to find its sandbox
        let container = self
            .store
            .containers
            .get(container_id)
            .await
            .ok_or_else(|| Status::not_found(format!("Container not found: {}", container_id)))?;
        ensure_container_running(&container, "Exec")?;
        ensure_container_image_available(&container).await?;

        // Get the VmManager for this sandbox
        let vm_managers = self.vm_managers.read().await;
        let vm = vm_managers.get(&container.sandbox_id).ok_or_else(|| {
            Status::not_found(format!("Sandbox not found: {}", container.sandbox_id))
        })?;
        ensure_vm_ready(vm, "Exec", &container.sandbox_id).await?;

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
            rootfs: if container.rootfs_guest_path.is_empty() {
                None
            } else {
                Some(container.rootfs_guest_path.clone())
            },
            tty: req.tty,
            stdin: req.stdin,
            stdin_once: false,
            stdout: req.stdout,
            stderr: req.stderr,
            ports: vec![],
            attach_stream: None,
            attach_stdin: None,
            exec_socket_path: exec_socket,
            pty_socket_path: pty_socket,
            port_forward_socket_path: String::new(),
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

        if !req.stdin && !req.stdout && !req.stderr {
            return Err(Status::invalid_argument(
                "Attach must request at least one stream",
            ));
        }
        let container = self
            .store
            .containers
            .get(container_id)
            .await
            .ok_or_else(|| Status::not_found(format!("Container not found: {}", container_id)))?;
        ensure_container_running(&container, "Attach")?;
        if req.tty != container.tty {
            return Err(Status::failed_precondition(format!(
                "Attach TTY flag must match container {} TTY configuration",
                container_id
            )));
        }
        if req.stdin && !container.stdin {
            return Err(Status::failed_precondition(format!(
                "Attach stdin requires container {} to be created with stdin enabled",
                container_id
            )));
        }
        ensure_container_image_available(&container).await?;

        let vm_managers = self.vm_managers.read().await;
        let vm = vm_managers.get(&container.sandbox_id).ok_or_else(|| {
            Status::not_found(format!("Sandbox not found: {}", container.sandbox_id))
        })?;
        ensure_vm_ready(vm, "Attach", &container.sandbox_id).await?;

        let attach_stream = self
            .attach_streams
            .read()
            .await
            .get(container_id)
            .cloned()
            .ok_or_else(|| {
                Status::failed_precondition(format!(
                    "Attach requires an active workload stream for container {}",
                    container_id
                ))
            })?;
        let attach_stdin = if req.stdin {
            let stdin = if container.stdin_once {
                self.workload_stdins.write().await.remove(container_id)
            } else {
                self.workload_stdins.read().await.get(container_id).cloned()
            };
            Some(stdin.ok_or_else(|| {
                Status::failed_precondition(format!(
                    "Attach stdin requires an active workload stdin stream for container {}",
                    container_id
                ))
            })?)
        } else {
            None
        };

        let session = StreamingSession {
            kind: SessionKind::Attach,
            sandbox_id: container.sandbox_id.clone(),
            cmd: vec![],
            rootfs: if container.rootfs_guest_path.is_empty() {
                None
            } else {
                Some(container.rootfs_guest_path.clone())
            },
            tty: req.tty,
            stdin: req.stdin,
            stdin_once: container.stdin_once,
            stdout: req.stdout,
            stderr: req.stderr,
            ports: vec![],
            attach_stream: Some(attach_stream),
            attach_stdin,
            exec_socket_path: String::new(),
            pty_socket_path: String::new(),
            port_forward_socket_path: String::new(),
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

        if req.port.is_empty() {
            return Err(Status::invalid_argument(
                "PortForward must request at least one port",
            ));
        }
        if req.port.len() != 1 {
            return Err(Status::unimplemented(
                "PortForward currently supports exactly one port per streaming session",
            ));
        }

        // Verify sandbox exists
        let sandbox = self
            .store
            .sandboxes
            .get(sandbox_id)
            .await
            .ok_or_else(|| Status::not_found(format!("Sandbox not found: {}", sandbox_id)))?;
        ensure_sandbox_ready(&sandbox, "PortForward")?;

        let vm_managers = self.vm_managers.read().await;
        let vm = vm_managers.get(sandbox_id).ok_or_else(|| {
            Status::not_found(format!("VM not found for sandbox: {}", sandbox_id))
        })?;
        ensure_vm_ready(vm, "PortForward", sandbox_id).await?;

        let port_forward_socket = vm
            .port_forward_socket_path()
            .ok_or_else(|| Status::unavailable("VM port-forward socket not ready"))?
            .to_string_lossy()
            .to_string();

        let session = StreamingSession {
            kind: SessionKind::PortForward,
            sandbox_id: sandbox_id.to_string(),
            cmd: vec![],
            rootfs: None,
            tty: false,
            stdin: false,
            stdin_once: false,
            stdout: false,
            stderr: false,
            ports: req.port,
            attach_stream: None,
            attach_stdin: None,
            exec_socket_path: String::new(),
            pty_socket_path: String::new(),
            port_forward_socket_path: port_forward_socket,
        };

        let url = self.streaming.register(session).await;
        Ok(Response::new(PortForwardResponse { url }))
    }

    async fn container_stats(
        &self,
        request: Request<ContainerStatsRequest>,
    ) -> Result<Response<ContainerStatsResponse>, Status> {
        let container_id = request.into_inner().container_id;
        let container = self
            .store
            .containers
            .get(&container_id)
            .await
            .ok_or_else(|| Status::not_found(format!("Container not found: {}", container_id)))?;

        Ok(Response::new(ContainerStatsResponse {
            stats: Some(container_stats(&container).await),
        }))
    }

    async fn list_container_stats(
        &self,
        request: Request<ListContainerStatsRequest>,
    ) -> Result<Response<ListContainerStatsResponse>, Status> {
        let req = request.into_inner();
        let sandbox_filter = req
            .filter
            .as_ref()
            .map(|filter| filter.pod_sandbox_id.as_str())
            .filter(|sandbox_id| !sandbox_id.is_empty());
        let label_filter = req
            .filter
            .as_ref()
            .map(|filter| &filter.label_selector)
            .filter(|labels| !labels.is_empty());

        let containers = self
            .store
            .containers
            .list(sandbox_filter, label_filter)
            .await;
        let containers: Vec<Container> = containers
            .into_iter()
            .filter(|container| {
                if container.state != ContainerState::Running {
                    return false;
                }
                if let Some(ref filter) = req.filter {
                    if !filter.id.is_empty() && container.id != filter.id {
                        return false;
                    }
                }
                true
            })
            .collect();
        let stats = join_all(containers.iter().map(container_stats)).await;

        Ok(Response::new(ListContainerStatsResponse { stats }))
    }

    async fn stream_container_stats(
        &self,
        request: Request<StreamContainerStatsRequest>,
    ) -> Result<Response<Self::StreamContainerStatsStream>, Status> {
        let req = request.into_inner();
        let response = self
            .list_container_stats(Request::new(ListContainerStatsRequest {
                filter: req.filter,
            }))
            .await?
            .into_inner();
        let stream: Self::StreamContainerStatsStream =
            Box::pin(tokio_stream::iter(vec![Ok(StreamContainerStatsResponse {
                stats: response.stats,
            })]));

        Ok(Response::new(stream))
    }

    async fn pod_sandbox_stats(
        &self,
        request: Request<PodSandboxStatsRequest>,
    ) -> Result<Response<PodSandboxStatsResponse>, Status> {
        let sandbox_id = request.into_inner().pod_sandbox_id;
        let sandbox = self
            .store
            .sandboxes
            .get(&sandbox_id)
            .await
            .ok_or_else(|| Status::not_found(format!("Sandbox not found: {}", sandbox_id)))?;
        let containers = self.store.containers.list(Some(&sandbox_id), None).await;

        Ok(Response::new(PodSandboxStatsResponse {
            stats: Some(pod_sandbox_stats(&sandbox, containers).await),
        }))
    }

    async fn list_pod_sandbox_stats(
        &self,
        request: Request<ListPodSandboxStatsRequest>,
    ) -> Result<Response<ListPodSandboxStatsResponse>, Status> {
        let req = request.into_inner();
        let label_filter = req
            .filter
            .as_ref()
            .map(|filter| &filter.label_selector)
            .filter(|labels| !labels.is_empty());
        let sandboxes = self.store.sandboxes.list(label_filter).await;

        let mut stats = Vec::new();
        for sandbox in sandboxes {
            if let Some(ref filter) = req.filter {
                if !filter.id.is_empty() && sandbox.id != filter.id {
                    continue;
                }
            }

            let containers = self.store.containers.list(Some(&sandbox.id), None).await;
            stats.push(pod_sandbox_stats(&sandbox, containers).await);
        }

        Ok(Response::new(ListPodSandboxStatsResponse { stats }))
    }

    async fn stream_pod_sandbox_stats(
        &self,
        request: Request<StreamPodSandboxStatsRequest>,
    ) -> Result<Response<Self::StreamPodSandboxStatsStream>, Status> {
        let req = request.into_inner();
        let response = self
            .list_pod_sandbox_stats(Request::new(ListPodSandboxStatsRequest {
                filter: req.filter,
            }))
            .await?
            .into_inner();
        let stream: Self::StreamPodSandboxStatsStream = Box::pin(tokio_stream::iter(vec![Ok(
            StreamPodSandboxStatsResponse {
                stats: response.stats,
            },
        )]));

        Ok(Response::new(stream))
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
        ensure_container_running(&container, "UpdateContainerResources")?;

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
        ensure_vm_ready(vm, "UpdateContainerResources", &container.sandbox_id).await?;

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

        // Signal the container's supervisor to reopen its log writer at
        // `log_path`. The kubelet rotates by renaming the current file before
        // calling this, so the supervisor must drop the stale handle and open a
        // fresh file at the original path (see `CriLogWriter::reopen`).
        if let Some(reopen) = self.log_reopens.read().await.get(container_id) {
            reopen.notify_one();
        }

        Ok(Response::new(ReopenContainerLogResponse {}))
    }
}
