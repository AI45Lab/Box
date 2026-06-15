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
use mounts::materialize_container_mount;
use network::{
    bridge_network_name, connect_sandbox_to_network_store, default_network_store,
    disconnect_sandbox_from_network_store, sandbox_network_name,
    sandbox_network_status_from_annotations, SandboxNetworkAllocation,
};
use stats::{
    container_stats, metric_descriptors, pod_sandbox_metrics, pod_sandbox_stats, read_vm_usage,
    VmUsage,
};
use supervisor::{spawn_container_exit_supervisor, ContainerExitSupervisor, SupervisedWorkload};

type CriResponseStream<T> = Pin<Box<dyn Stream<Item = Result<T, Status>> + Send + 'static>>;
type AttachStreamSender = broadcast::Sender<a3s_box_core::exec::ExecEvent>;
type AttachStreamMap = Arc<RwLock<HashMap<String, AttachStreamSender>>>;
type WorkloadStdinSender = StreamingInput;
type WorkloadStdinMap = Arc<RwLock<HashMap<String, WorkloadStdinSender>>>;
type WorkloadStopSender = oneshot::Sender<()>;
type WorkloadStopMap = Arc<RwLock<HashMap<String, WorkloadStopSender>>>;
/// Per-container handle for CRI ReopenContainerLog. `request` wakes the exit
/// supervisor to reopen its log writer; `done` is notified once the reopen has
/// completed, so ReopenContainerLog returns only after the new file is in place
/// (the RPC is synchronous — output written after it returns must land in the
/// rotated file).
pub(super) struct LogReopenHandle {
    pub(super) request: Arc<Notify>,
    pub(super) done: Arc<Notify>,
}
type LogReopenMap = Arc<RwLock<HashMap<String, LogReopenHandle>>>;
type ContainerEventSender = broadcast::Sender<ContainerEventResponse>;

const CRI_CONTAINER_ROOTFS_HOST_DIR: &str = "cri-container-rootfs";
const CRI_CONTAINER_ROOTFS_GUEST_BASE: &str = "/run/a3s/cri/container-rootfs";
const CONTAINER_EVENT_BUFFER: usize = 1024;

/// Capabilities a non-privileged container keeps by default (matches the
/// containerd/runc default set). A privileged container keeps the full set.
const DEFAULT_CAPABILITIES: &[&str] = &[
    "CHOWN",
    "DAC_OVERRIDE",
    "FSETID",
    "FOWNER",
    "MKNOD",
    "NET_RAW",
    "SETGID",
    "SETUID",
    "SETFCAP",
    "SETPCAP",
    "NET_BIND_SERVICE",
    "SYS_CHROOT",
    "KILL",
    "AUDIT_WRITE",
];

/// Compute the capability set a non-privileged container keeps: the default set
/// unioned with `add_capabilities` and minus `drop_capabilities` (`ALL` is
/// honored on either side). Returns `None` when the container should keep the
/// full set (an `add` of `ALL`), in which case no keep-set is emitted.
fn kept_capabilities(capabilities: Option<&Capability>) -> Option<Vec<String>> {
    let normalize = |cap: &str| {
        cap.trim()
            .strip_prefix("CAP_")
            .unwrap_or(cap.trim())
            .to_uppercase()
    };
    let mut kept: std::collections::BTreeSet<String> =
        DEFAULT_CAPABILITIES.iter().map(|c| c.to_string()).collect();
    if let Some(capabilities) = capabilities {
        if capabilities
            .add_capabilities
            .iter()
            .any(|c| normalize(c) == "ALL")
        {
            return None;
        }
        if capabilities
            .drop_capabilities
            .iter()
            .any(|c| normalize(c) == "ALL")
        {
            kept.clear();
        } else {
            for cap in &capabilities.drop_capabilities {
                kept.remove(&normalize(cap));
            }
        }
        for cap in &capabilities.add_capabilities {
            kept.insert(normalize(cap));
        }
    }
    Some(kept.into_iter().collect())
}

/// Whether an AppArmor profile of the given name is currently loaded on the
/// host (listed in `/sys/kernel/security/apparmor/profiles`).
///
/// Returns `false` when AppArmor is unavailable (file absent/unreadable), which
/// makes a requested Localhost profile fail closed rather than run unconfined.
fn apparmor_profile_loaded(name: &str) -> bool {
    let Ok(content) = std::fs::read_to_string("/sys/kernel/security/apparmor/profiles") else {
        return false;
    };
    // Each line is `<profile-name> (<mode>)`.
    content
        .lines()
        .any(|line| line.split_whitespace().next() == Some(name))
}

#[derive(serde::Deserialize)]
struct OciSeccompProfile {
    #[serde(rename = "defaultAction")]
    default_action: String,
    #[serde(default)]
    syscalls: Vec<OciSeccompSyscall>,
}

#[derive(serde::Deserialize)]
struct OciSeccompSyscall {
    #[serde(default)]
    names: Vec<String>,
    action: String,
}

/// Parse a CRI localhost seccomp profile file and return the syscall names it
/// blocks with `SCMP_ACT_ERRNO`/`SCMP_ACT_KILL*`.
///
/// Supports the conformance shape — a permissive default action
/// (`SCMP_ACT_ALLOW`/`SCMP_ACT_LOG`) plus an ERRNO deny list. A deny-by-default
/// profile (which would need a full allow-list compiled to BPF) is not yet
/// supported; returns an error so the caller can fall back rather than silently
/// run unconfined.
fn parse_localhost_seccomp_deny(localhost_ref: &str) -> Result<Vec<String>, String> {
    let raw = std::fs::read_to_string(localhost_ref)
        .map_err(|e| format!("read seccomp profile {localhost_ref}: {e}"))?;
    let profile: OciSeccompProfile =
        serde_json::from_str(&raw).map_err(|e| format!("parse seccomp profile: {e}"))?;
    if !matches!(
        profile.default_action.as_str(),
        "SCMP_ACT_ALLOW" | "SCMP_ACT_LOG"
    ) {
        return Err(format!(
            "unsupported seccomp defaultAction {} (only allow-default profiles are supported)",
            profile.default_action
        ));
    }
    let deny: Vec<String> = profile
        .syscalls
        .into_iter()
        .filter(|s| {
            matches!(
                s.action.as_str(),
                "SCMP_ACT_ERRNO"
                    | "SCMP_ACT_KILL"
                    | "SCMP_ACT_KILL_THREAD"
                    | "SCMP_ACT_KILL_PROCESS"
            )
        })
        .flat_map(|s| s.names)
        .collect();
    Ok(deny)
}

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
    /// Real CPU + memory usage of a sandbox's microVM, read from the host-side
    /// shim process (the shim *is* the pod in the microVM-per-pod model).
    /// Returns zeros if the VM is not booted or its procfs is unreadable.
    async fn sandbox_vm_usage(&self, sandbox_id: &str) -> VmUsage {
        let pid = {
            let vm_managers = self.vm_managers.read().await;
            match vm_managers.get(sandbox_id) {
                Some(vm) => vm.pid().await,
                None => None,
            }
        };
        match pid {
            Some(pid) => read_vm_usage(pid),
            None => VmUsage::default(),
        }
    }

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
                // Crash recovery: a graceful shutdown already destroyed the VMs,
                // but a crash (SIGKILL/OOM) leaves the shim microVM, its overlay
                // mount, and the rootfs dirs orphaned. Reap them so they do not
                // leak across restarts. No-op when nothing is left behind.
                a3s_box_runtime::vm::reap::reap_orphaned_box(&sandbox.id);
                self.cleanup_sandbox_rootfs(&sandbox.id).await;
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

        // Reject host-namespace requests fail-closed (microVM cannot share the
        // host's namespaces) before acquiring any VM or network resources.
        convert::validate_namespace_options(
            config
                .linux
                .as_ref()
                .and_then(|linux| linux.security_context.as_ref())
                .and_then(|sc| sc.namespace_options.as_ref()),
            "RunPodSandbox",
        )?;

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

        // For a default (TSI) pod that publishes ports but has no
        // allocated/annotated IP, report the loopback as the pod IP: TSI binds
        // 0.0.0.0:<hostPort> and forwards to the guest, so the pod's published
        // ports are genuinely reachable at 127.0.0.1:<port> from the node (which
        // is what a node-local CRI consumer / conformance GET targets).
        if network_ip.is_empty() && !config.port_mappings.is_empty() {
            network_ip = "127.0.0.1".to_string();
        }

        // Store sandbox state
        let now_ns = chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0);
        let sandbox = PodSandbox {
            id: sandbox_id.clone(),
            name: metadata.name.clone(),
            namespace: metadata.namespace.clone(),
            uid: metadata.uid.clone(),
            attempt: metadata.attempt,
            state: SandboxState::Ready,
            created_at: now_ns,
            labels: config.labels.clone(),
            annotations: config.annotations.clone(),
            log_directory: config.log_directory.clone(),
            runtime_handler: req.runtime_handler,
            network_ip,
            additional_ips,
            dns: config
                .dns_config
                .as_ref()
                .map(|dns| crate::sandbox::SandboxDns {
                    servers: dns.servers.clone(),
                    searches: dns.searches.clone(),
                    options: dns.options.clone(),
                })
                .unwrap_or_default(),
            container_ports: config
                .port_mappings
                .iter()
                .map(|mapping| mapping.container_port)
                .filter(|port| *port > 0)
                .collect(),
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

        // Mark the sandbox NotReady regardless of whether VM teardown succeeds:
        // its containers are already Exited and their streams removed, so it must
        // not remain Ready — otherwise a later CreateContainer (which only checks
        // the sandbox is Ready) would attach to a sandbox whose VM is gone. A
        // VM-destroy error is surfaced after the state is made consistent.
        let destroy_result = self.destroy_sandbox_vm(sandbox_id, None).await;
        self.disconnect_sandbox_network(&sandbox).await;
        self.store
            .update_sandbox_state(sandbox_id, SandboxState::NotReady)
            .await;
        destroy_result?;

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
                attempt: sandbox.attempt,
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
        // Reject host-namespace requests fail-closed before allocating anything.
        convert::validate_namespace_options(
            config
                .linux
                .as_ref()
                .and_then(|linux| linux.security_context.as_ref())
                .and_then(|sc| sc.namespace_options.as_ref()),
            "CreateContainer",
        )?;
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
        // A3S_SEC_* is the runtime's TRUSTED security envelope. Strip any
        // image- or pod-supplied A3S_SEC_* entry first: the guest matches these
        // keys first-wins, so a caller-controlled value would otherwise
        // override the runtime's (escalating supplemental groups, unmasking
        // paths, or toggling seccomp). Only runtime-derived values may set them.
        env.retain(|(key, _)| !key.starts_with("A3S_SEC_"));
        // Carry the container memory limit so guest-init enforces it with a
        // per-container cgroup v2 `memory.max` and reports OOMKilled when the
        // kernel OOM-killer reaps the cgroup. The microVM RAM stays larger than
        // this, so the cgroup limit (not the VM) is what bounds the workload.
        if let Some(resources) = config
            .linux
            .as_ref()
            .and_then(|linux| linux.resources.as_ref())
        {
            if resources.memory_limit_in_bytes > 0 {
                env.push((
                    "A3S_SEC_MEM_LIMIT".to_string(),
                    resources.memory_limit_in_bytes.to_string(),
                ));
            }
            // CPU limit → guest cgroup v2 cpu.max ("<quota_us> <period_us>"). A
            // quota of 0 / -1 means unlimited, so only plumb a positive quota.
            if resources.cpu_quota > 0 {
                env.push((
                    "A3S_SEC_CPU_QUOTA".to_string(),
                    resources.cpu_quota.to_string(),
                ));
                if resources.cpu_period > 0 {
                    env.push((
                        "A3S_SEC_CPU_PERIOD".to_string(),
                        resources.cpu_period.to_string(),
                    ));
                }
            }
            // CPU shares → guest cgroup v2 cpu.weight (relative CPU under
            // contention, the kubelet's CPU *request*).
            if resources.cpu_shares > 0 {
                env.push((
                    "A3S_SEC_CPU_SHARES".to_string(),
                    resources.cpu_shares.to_string(),
                ));
            }
        }
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
                env.push((
                    "A3S_SEC_MASKED_PATHS".to_string(),
                    sc.masked_paths.join(":"),
                ));
            }
            if !sc.readonly_paths.is_empty() {
                env.push((
                    "A3S_SEC_READONLY_PATHS".to_string(),
                    sc.readonly_paths.join(":"),
                ));
            }
            // CRI seccomp: RuntimeDefault installs the default BPF filter in the
            // guest (Seccomp: 2); Unconfined / unset leave the container
            // unconfined; Localhost compiles the profile's deny list. Privileged
            // containers bypass seccomp entirely — a requested profile is
            // ignored when privileged (per the CRI contract).
            if let Some(profile) = sc.seccomp.as_ref().filter(|_| !sc.privileged) {
                use crate::cri_api::security_profile::ProfileType;
                match profile.profile_type() {
                    ProfileType::RuntimeDefault => {
                        env.push(("A3S_SEC_SECCOMP".to_string(), "default".to_string()));
                    }
                    ProfileType::Localhost => {
                        // Read + parse the localhost profile (allow-default +
                        // ERRNO deny list) and pass the blocked syscalls to the
                        // guest, which compiles them into a BPF filter.
                        match parse_localhost_seccomp_deny(&profile.localhost_ref) {
                            Ok(deny) => {
                                env.push(("A3S_SEC_SECCOMP_LOCALHOST".to_string(), deny.join(",")));
                            }
                            Err(e) => {
                                // Fall back to RuntimeDefault rather than running
                                // unconfined when the profile can't be applied.
                                tracing::warn!(
                                    container = %metadata.name,
                                    profile = %profile.localhost_ref,
                                    error = %e,
                                    "Localhost seccomp profile unsupported; applying RuntimeDefault instead"
                                );
                                env.push(("A3S_SEC_SECCOMP".to_string(), "default".to_string()));
                            }
                        }
                    }
                    ProfileType::Unconfined => {}
                }
            }
            // CRI capabilities: a privileged container keeps the full set; a
            // non-privileged one is restricted to the runtime default set,
            // adjusted by add/drop (A3S_SEC_CAP_KEEP — the guest drops every
            // capability not listed). This is what stops a non-privileged
            // container from doing privileged operations (e.g. creating a bridge
            // with CAP_NET_ADMIN). An `add` of `ALL` keeps the full set.
            if !sc.privileged {
                if let Some(kept) = kept_capabilities(sc.capabilities.as_ref()) {
                    env.push(("A3S_SEC_CAP_KEEP".to_string(), kept.join(",")));
                }
            }
            // no_new_privs: the guest sets PR_SET_NO_NEW_PRIVS before exec so a
            // setuid/setgid (or file-capability) binary can no longer raise the
            // process's privileges. Privileged containers opt out.
            if sc.no_new_privs && !sc.privileged {
                env.push(("A3S_SEC_NO_NEW_PRIVS".to_string(), "1".to_string()));
            }
            // readonly_rootfs: the guest remounts the container root read-only
            // before exec. This is a deliberate config (not a hardening default),
            // so it applies to privileged containers too.
            if sc.readonly_rootfs {
                env.push(("A3S_SEC_READONLY_ROOTFS".to_string(), "1".to_string()));
            }
            // AppArmor: a microVM cannot enforce an in-guest LSM profile, but a
            // requested Localhost profile must not be silently ignored. Validate
            // it against the host's loaded profiles and reject when it is not
            // loaded (the CRI contract: an unloaded profile fails). A loaded
            // profile is accepted but cannot be enforced in-guest. The modern
            // `apparmor` SecurityProfile takes precedence over the deprecated
            // `apparmor_profile` string.
            #[allow(deprecated)] // clients (incl. critest) still send apparmor_profile
            let apparmor_localhost = sc
                .apparmor
                .as_ref()
                .filter(|profile| {
                    profile.profile_type()
                        == crate::cri_api::security_profile::ProfileType::Localhost
                })
                .map(|profile| profile.localhost_ref.clone())
                .or_else(|| {
                    sc.apparmor_profile
                        .strip_prefix("localhost/")
                        .map(|name| name.to_string())
                });
            if let Some(profile) = apparmor_localhost {
                let profile = profile.trim();
                if profile.is_empty() || !apparmor_profile_loaded(profile) {
                    return Err(Status::failed_precondition(format!(
                        "AppArmor profile 'localhost/{profile}' is not loaded"
                    )));
                }
                tracing::warn!(
                    container = %metadata.name,
                    profile = %profile,
                    "AppArmor localhost profile is loaded on the host but the microVM \
                     runtime cannot enforce it in-guest; the container runs without \
                     AppArmor confinement"
                );
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
                // Render the pod's DNS config into the container's resolv.conf
                // (empty -> the builder writes its default).
                let resolv_conf = self
                    .store
                    .sandboxes
                    .get(sandbox_id)
                    .await
                    .map(|sb| {
                        a3s_box_core::dns::render_resolv_conf(
                            &sb.dns.servers,
                            &sb.dns.searches,
                            &sb.dns.options,
                        )
                    })
                    .unwrap_or_default();
                if let Err(status) = self
                    .prepare_container_rootfs(image, &paths, resolv_conf)
                    .await
                {
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
                .materialize_container_mounts(&rootfs_path, &mounts)
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
            attempt: metadata.attempt,
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
            oom_killed: false,
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

        let (mut workload, stdin_handle) = if container.tty {
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

        if !started {
            // We already started the workload, but the container concurrently
            // left the Created state (a racing Stop/Remove won the CAS). Stop the
            // workload directly and return: spawning the exit supervisor here
            // would ORPHAN a long-running workload (e.g. nginx) — its stop handle
            // is never inserted into `workload_stops`, so StopContainer can't
            // reach it, and it would run until it self-exits.
            if let Err(error) = workload.cancel().await {
                tracing::warn!(
                    container_id = %container_id,
                    sandbox_id = %container.sandbox_id,
                    error = %error,
                    "Failed to stop workload after losing the Created-state CAS"
                );
            }
            return Err(Status::failed_precondition(format!(
                "Container {} is no longer in the Created state",
                container_id
            )));
        }

        // started == true: register the control handles and spawn the supervisor.
        let (attach_tx, _) = broadcast::channel(128);
        let (stop_tx, stop_rx) = oneshot::channel();
        let log_reopen = Arc::new(Notify::new());
        let log_reopen_done = Arc::new(Notify::new());
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
        self.log_reopens.write().await.insert(
            container_id.clone(),
            LogReopenHandle {
                request: log_reopen.clone(),
                done: log_reopen_done.clone(),
            },
        );
        self.emit_container_event(
            &container_id,
            &container.sandbox_id,
            ContainerEventType::ContainerStartedEvent,
            now_ns,
            "ContainerStarted",
            format!("Container {} started", container.name),
        );

        // Open (create) the container log file now, before StartContainer
        // returns, so a caller that opens it immediately — e.g. critest's
        // ReopenContainerLog test, before the container has produced any output
        // — finds it. The supervisor would otherwise create it lazily in its
        // task, racing the caller.
        let log_writer = match log_writer::CriLogWriter::open(&container.log_path).await {
            Ok(writer) => writer,
            Err(e) => {
                // The container still runs — logging degrades rather than failing
                // StartContainer — but surface the failure instead of swallowing it
                // silently, so an unwritable log path (permissions, full disk) is
                // diagnosable.
                tracing::warn!(
                    container = %container_id,
                    log_path = %container.log_path,
                    error = %e,
                    "Failed to open container log file; container output will not be logged"
                );
                None
            }
        };

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
            log_writer,
            attach_tx,
            stop_rx,
            log_reopen,
            log_reopen_done,
            workload,
        });

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
                        .mark_container_exited_if_running(container_id, now_ns, 137, false)
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

        // CRI RemoveContainer force-removes: a still-running container is
        // stopped first (timeout 0), then deleted — matching containerd/cri-o.
        // stop_container handles the workload stop + sandbox VM teardown
        // fallback and the multi-container guard.
        if container.state == ContainerState::Running {
            self.stop_container(Request::new(StopContainerRequest {
                container_id: container_id.clone(),
                timeout: 0,
            }))
            .await?;
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
            ContainerState::Exited => {
                container_exit_reason(container.exit_code, container.oom_killed)
            }
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
                attempt: container.attempt,
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

        if req.port.len() > 1 {
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

        // critest passes the target port in the RPC; `crictl port-forward` sends
        // an empty list and carries the port only in the SPDY stream header
        // (which we cannot read without SPDY/3 dictionary decompression). In the
        // empty case, fall back to the sandbox's single declared container port.
        let ports = if req.port.is_empty() {
            sandbox.container_ports.clone()
        } else {
            req.port
        };
        if ports.is_empty() {
            return Err(Status::invalid_argument(
                "PortForward requested no port and the sandbox declares none",
            ));
        }
        if ports.len() != 1 {
            return Err(Status::unimplemented(
                "PortForward currently supports exactly one port per streaming session",
            ));
        }

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
            ports,
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

        // Only a running container consumes VM resources; a Created/Exited one
        // reports zero rather than a share of the pod VM's usage.
        let usage = if container.state == ContainerState::Running {
            let running = self
                .store
                .containers
                .list(Some(&container.sandbox_id), None)
                .await
                .into_iter()
                .filter(|c| c.state == ContainerState::Running)
                .count();
            self.sandbox_vm_usage(&container.sandbox_id)
                .await
                .per_container(running)
        } else {
            VmUsage::default()
        };
        Ok(Response::new(ContainerStatsResponse {
            stats: Some(container_stats(&container, usage).await),
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
        // Resolve each pod's VM usage once, split across its running containers.
        let mut usage_by_sandbox: std::collections::HashMap<String, VmUsage> =
            std::collections::HashMap::new();
        for container in &containers {
            if !usage_by_sandbox.contains_key(&container.sandbox_id) {
                // Split across RUNNING containers only (a non-running one consumes
                // nothing) so the running ones get an accurate share.
                let running = containers
                    .iter()
                    .filter(|c| {
                        c.sandbox_id == container.sandbox_id && c.state == ContainerState::Running
                    })
                    .count();
                let usage = self
                    .sandbox_vm_usage(&container.sandbox_id)
                    .await
                    .per_container(running);
                usage_by_sandbox.insert(container.sandbox_id.clone(), usage);
            }
        }
        let stats = join_all(containers.iter().map(|c| {
            // Only running containers report a share of the VM usage; others zero.
            let usage = if c.state == ContainerState::Running {
                usage_by_sandbox
                    .get(&c.sandbox_id)
                    .copied()
                    .unwrap_or_default()
            } else {
                VmUsage::default()
            };
            container_stats(c, usage)
        }))
        .await;

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
        let vm_usage = self.sandbox_vm_usage(&sandbox_id).await;

        Ok(Response::new(PodSandboxStatsResponse {
            stats: Some(pod_sandbox_stats(&sandbox, containers, vm_usage).await),
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
            let vm_usage = self.sandbox_vm_usage(&sandbox.id).await;
            stats.push(pod_sandbox_stats(&sandbox, containers, vm_usage).await);
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
        // `log_path` and WAIT for it to finish. The kubelet rotates by renaming
        // the current file before calling this, so the supervisor must drop the
        // stale handle and open a fresh file at the original path (see
        // `CriLogWriter::reopen`). Returning synchronously guarantees output
        // written after this RPC lands in the rotated file rather than racing
        // the supervisor. Clone the notify handles out of the lock so we never
        // hold it across the await.
        let handles = {
            let reopens = self.log_reopens.read().await;
            reopens
                .get(container_id)
                .map(|handle| (handle.request.clone(), handle.done.clone()))
        };
        if let Some((request, done)) = handles {
            // Register the done future BEFORE signalling so a fast supervisor
            // cannot complete the reopen before we start waiting (notify_one
            // also stores a permit, so there is no lost-wakeup either way).
            let done = done.notified();
            request.notify_one();
            if tokio::time::timeout(std::time::Duration::from_secs(5), done)
                .await
                .is_err()
            {
                tracing::warn!(
                    container_id = %container_id,
                    "ReopenContainerLog timed out waiting for the supervisor to reopen the log"
                );
            }
        }

        Ok(Response::new(ReopenContainerLogResponse {}))
    }
}
