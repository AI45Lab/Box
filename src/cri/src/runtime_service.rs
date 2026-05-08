//! CRI RuntimeService implementation.
//!
//! Maps CRI pod/container lifecycle to A3S Box VmManager instances.
//! - Pod Sandbox → Box instance (one microVM per pod)
//! - Container → Session within Box

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;

use futures::{future::join_all, Stream};
use tokio::io::AsyncWriteExt;
use tokio::sync::{broadcast, oneshot, RwLock};
use tonic::{Request, Response, Status};

use a3s_box_core::event::EventEmitter;
use a3s_box_core::NetworkMode;
use a3s_box_runtime::oci::{ImageStore, OciImageConfig, OciRootfsBuilder, RegistryAuth};
use a3s_box_runtime::pool::WarmPool;
use a3s_box_runtime::vm::VmManager;
use a3s_box_runtime::NetworkStore;

use crate::config_mapper::{pod_sandbox_config_to_box_config, ANN_NETWORK, DEFAULT_AGENT_IMAGE};
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

type CriResponseStream<T> = Pin<Box<dyn Stream<Item = Result<T, Status>> + Send + 'static>>;
type AttachStreamSender = broadcast::Sender<a3s_box_core::exec::ExecEvent>;
type AttachStreamMap = Arc<RwLock<HashMap<String, AttachStreamSender>>>;
type WorkloadStdinSender = StreamingInput;
type WorkloadStdinMap = Arc<RwLock<HashMap<String, WorkloadStdinSender>>>;
type WorkloadStopSender = oneshot::Sender<()>;
type WorkloadStopMap = Arc<RwLock<HashMap<String, WorkloadStopSender>>>;
type ContainerEventSender = broadcast::Sender<ContainerEventResponse>;

const CRI_CONTAINER_ROOTFS_HOST_DIR: &str = "cri-container-rootfs";
const CRI_CONTAINER_ROOTFS_GUEST_BASE: &str = "/run/a3s/cri/container-rootfs";
const DEFAULT_STOP_CONTAINER_WAIT_SECS: u64 = 10;
const CONTAINER_EVENT_BUFFER: usize = 1024;
const ANN_POD_IP: &str = "a3s.box/pod-ip";
const ANN_ADDITIONAL_POD_IPS: &str = "a3s.box/additional-pod-ips";

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

struct ResolvedContainerImage {
    digest: String,
    path: String,
    config: OciImageConfig,
}

struct ContainerRootfsPaths {
    host_path: PathBuf,
    guest_path: String,
}

struct SandboxNetworkAllocation {
    network_name: String,
    ip: String,
}

enum SupervisedWorkload {
    Exec(a3s_box_runtime::StreamingExec),
    Pty(a3s_box_runtime::StreamingPty),
}

impl SupervisedWorkload {
    async fn next_event(
        &mut self,
    ) -> a3s_box_core::error::Result<Option<a3s_box_core::exec::ExecEvent>> {
        match self {
            Self::Exec(stream) => stream.next_event().await,
            Self::Pty(stream) => stream.next_event().await,
        }
    }

    async fn cancel(&mut self) -> a3s_box_core::error::Result<()> {
        match self {
            Self::Exec(stream) => stream.cancel().await,
            Self::Pty(stream) => stream.cancel().await,
        }
    }
}

fn sanitize_path_component(value: &str) -> String {
    let sanitized: String = value
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
                ch
            } else {
                '_'
            }
        })
        .collect();

    if sanitized.is_empty() {
        "unknown".to_string()
    } else {
        sanitized
    }
}

fn container_user_from_linux_config(linux: Option<&LinuxContainerConfig>) -> Option<String> {
    let security_context = linux.and_then(|linux| linux.security_context.as_ref())?;

    if !security_context.run_as_username.is_empty() {
        return Some(security_context.run_as_username.clone());
    }

    let user = security_context.run_as_user.as_ref()?.value;
    Some(match security_context.run_as_group.as_ref() {
        Some(group) => format!("{user}:{}", group.value),
        None => user.to_string(),
    })
}

fn container_mount_from_cri(mount: &Mount) -> ContainerMount {
    ContainerMount {
        container_path: mount.container_path.clone(),
        host_path: mount.host_path.clone(),
        readonly: mount.readonly,
        selinux_relabel: mount.selinux_relabel,
        propagation: mount.propagation,
    }
}

fn container_mount_to_cri(mount: &ContainerMount) -> Mount {
    Mount {
        container_path: mount.container_path.clone(),
        host_path: mount.host_path.clone(),
        readonly: mount.readonly,
        selinux_relabel: mount.selinux_relabel,
        propagation: mount.propagation,
    }
}

fn validate_container_mount(mount: &Mount) -> Result<(), Status> {
    if mount.host_path.trim().is_empty() {
        return Err(Status::invalid_argument(
            "CRI mount host_path must not be empty",
        ));
    }
    if mount.container_path.trim().is_empty() {
        return Err(Status::invalid_argument(
            "CRI mount container_path must not be empty",
        ));
    }
    if !Path::new(&mount.container_path).is_absolute() {
        return Err(Status::invalid_argument(format!(
            "CRI mount container_path must be absolute: {}",
            mount.container_path
        )));
    }
    if !mount.readonly {
        return Err(Status::unimplemented(format!(
            "Writable CRI mounts are not yet supported for microVM-backed containers: {}",
            mount.container_path
        )));
    }
    if mount.selinux_relabel {
        return Err(Status::unimplemented(format!(
            "SELinux relabeling is not supported for CRI mount {}",
            mount.container_path
        )));
    }

    let propagation = crate::cri_api::mount::MountPropagation::try_from(mount.propagation)
        .map_err(|_| {
            Status::invalid_argument(format!(
                "Invalid CRI mount propagation value {} for {}",
                mount.propagation, mount.container_path
            ))
        })?;
    if propagation != crate::cri_api::mount::MountPropagation::PropagationPrivate {
        return Err(Status::unimplemented(format!(
            "CRI mount propagation {:?} is not supported for microVM-backed containers",
            propagation
        )));
    }

    Ok(())
}

fn resolve_container_mounts(mounts: &[Mount]) -> Result<Vec<ContainerMount>, Status> {
    mounts
        .iter()
        .map(|mount| {
            validate_container_mount(mount)?;
            Ok(container_mount_from_cri(mount))
        })
        .collect()
}

fn merge_env(image_env: &[(String, String)], cri_env: &[KeyValue]) -> Vec<(String, String)> {
    let mut merged = image_env.to_vec();

    for kv in cri_env {
        if let Some((_, value)) = merged.iter_mut().find(|(key, _)| key == &kv.key) {
            *value = kv.value.clone();
        } else {
            merged.push((kv.key.clone(), kv.value.clone()));
        }
    }

    merged
}

fn resolve_command_and_args(
    config: &ContainerConfig,
    image_config: Option<&OciImageConfig>,
) -> (Vec<String>, Vec<String>) {
    if !config.command.is_empty() {
        return (config.command.clone(), config.args.clone());
    }

    let command = image_config
        .and_then(|image| image.entrypoint.clone())
        .unwrap_or_default();
    let args = if config.args.is_empty() {
        image_config
            .and_then(|image| image.cmd.clone())
            .unwrap_or_default()
    } else {
        config.args.clone()
    };

    (command, args)
}

fn sandbox_network_status_from_annotations(
    annotations: &HashMap<String, String>,
) -> Result<(String, Vec<String>), Status> {
    let network_ip = annotations
        .get(ANN_POD_IP)
        .map(|ip| ip.trim())
        .filter(|ip| !ip.is_empty())
        .map(parse_sandbox_ip)
        .transpose()?
        .unwrap_or_default();

    let additional_ips = annotations
        .get(ANN_ADDITIONAL_POD_IPS)
        .map(|value| {
            value
                .split(',')
                .map(str::trim)
                .filter(|ip| !ip.is_empty())
                .map(parse_sandbox_ip)
                .collect::<Result<Vec<_>, _>>()
        })
        .transpose()?
        .unwrap_or_default();

    if network_ip.is_empty() && !additional_ips.is_empty() {
        return Err(Status::invalid_argument(format!(
            "Annotation {ANN_ADDITIONAL_POD_IPS} requires primary annotation {ANN_POD_IP}"
        )));
    }

    Ok((network_ip, additional_ips))
}

fn parse_sandbox_ip(value: &str) -> Result<String, Status> {
    value
        .parse::<std::net::IpAddr>()
        .map(|ip| ip.to_string())
        .map_err(|e| {
            Status::invalid_argument(format!(
                "Invalid CRI sandbox IP annotation value '{value}': {e}"
            ))
        })
}

fn bridge_network_name(config: &a3s_box_core::config::BoxConfig) -> Option<String> {
    match &config.network {
        NetworkMode::Bridge { network } if !network.trim().is_empty() => {
            Some(network.trim().to_string())
        }
        _ => None,
    }
}

fn sandbox_network_name(sandbox: &PodSandbox) -> Option<String> {
    sandbox
        .annotations
        .get(ANN_NETWORK)
        .map(|network| network.trim())
        .filter(|network| !network.is_empty())
        .map(ToOwned::to_owned)
}

fn connect_sandbox_to_network_store(
    store: &NetworkStore,
    network_name: &str,
    sandbox_id: &str,
    pod_name: &str,
) -> Result<SandboxNetworkAllocation, Status> {
    let mut network = store
        .get(network_name)
        .map_err(box_error_to_status)?
        .ok_or_else(|| Status::not_found(format!("Network not found: {network_name}")))?;

    let endpoint = network.connect(sandbox_id, pod_name).map_err(|e| {
        Status::failed_precondition(format!(
            "Failed to connect sandbox {sandbox_id} to network {network_name}: {e}"
        ))
    })?;
    let ip = endpoint.ip_address.to_string();

    store.update(&network).map_err(box_error_to_status)?;

    Ok(SandboxNetworkAllocation {
        network_name: network_name.to_string(),
        ip,
    })
}

fn disconnect_sandbox_from_network_store(
    store: &NetworkStore,
    network_name: &str,
    sandbox_id: &str,
) -> Result<(), Status> {
    let Some(mut network) = store.get(network_name).map_err(box_error_to_status)? else {
        return Ok(());
    };

    if network.disconnect(sandbox_id).is_ok() {
        store.update(&network).map_err(box_error_to_status)?;
    }

    Ok(())
}

fn default_network_store() -> NetworkStore {
    match NetworkStore::default_path() {
        Ok(store) => store,
        Err(e) => {
            tracing::warn!(
                error = %e,
                "Failed to resolve default network store path; falling back to dirs_home"
            );
            NetworkStore::new(a3s_box_core::dirs_home().join("networks.json"))
        }
    }
}

fn container_exit_reason(exit_code: i32) -> (&'static str, String) {
    if exit_code == 0 {
        ("Completed", "Container exited successfully".to_string())
    } else {
        ("Error", format!("Container exited with code {exit_code}"))
    }
}

fn ensure_container_running(container: &Container, operation: &str) -> Result<(), Status> {
    if container.state == ContainerState::Running {
        return Ok(());
    }

    Err(Status::failed_precondition(format!(
        "{operation} requires a running container; container {} is {:?}",
        container.id, container.state
    )))
}

fn ensure_sandbox_ready(sandbox: &PodSandbox, operation: &str) -> Result<(), Status> {
    if sandbox.state == SandboxState::Ready {
        return Ok(());
    }

    Err(Status::failed_precondition(format!(
        "{operation} requires a ready sandbox; sandbox {} is {:?}",
        sandbox.id, sandbox.state
    )))
}

async fn ensure_container_image_available(container: &Container) -> Result<(), Status> {
    if container.image_ref.trim().is_empty() {
        return Ok(());
    }

    if container.resolved_image_digest.trim().is_empty()
        || container.resolved_image_path.trim().is_empty()
    {
        return Err(Status::failed_precondition(format!(
            "Container {} was created without resolved image metadata for {}; recreate it after PullImage",
            container.id, container.image_ref
        )));
    }

    let image_metadata = tokio::fs::metadata(&container.resolved_image_path)
        .await
        .map_err(|e| {
            Status::failed_precondition(format!(
                "Resolved image path for container {} is unavailable: {} ({})",
                container.id, container.resolved_image_path, e
            ))
        })?;

    if !image_metadata.is_dir() {
        return Err(Status::failed_precondition(format!(
            "Resolved image path for container {} is not a directory: {}",
            container.id, container.resolved_image_path
        )));
    }

    if container.rootfs_path.trim().is_empty() || container.rootfs_guest_path.trim().is_empty() {
        return Err(Status::failed_precondition(format!(
            "Container {} was created without prepared rootfs metadata for {}; recreate it",
            container.id, container.image_ref
        )));
    }

    let rootfs_metadata = tokio::fs::metadata(&container.rootfs_path)
        .await
        .map_err(|e| {
            Status::failed_precondition(format!(
                "Prepared rootfs for container {} is unavailable: {} ({})",
                container.id, container.rootfs_path, e
            ))
        })?;

    if !rootfs_metadata.is_dir() {
        return Err(Status::failed_precondition(format!(
            "Prepared rootfs for container {} is not a directory: {}",
            container.id, container.rootfs_path
        )));
    }

    Ok(())
}

fn sandbox_state_label(state: SandboxState) -> &'static str {
    match state {
        SandboxState::Ready => "ready",
        SandboxState::NotReady => "not_ready",
        SandboxState::Removed => "removed",
    }
}

fn container_state_label(state: ContainerState) -> &'static str {
    match state {
        ContainerState::Created => "created",
        ContainerState::Running => "running",
        ContainerState::Exited => "exited",
    }
}

fn container_state_to_cri(state: ContainerState) -> crate::cri_api::ContainerState {
    match state {
        ContainerState::Created => crate::cri_api::ContainerState::ContainerCreated,
        ContainerState::Running => crate::cri_api::ContainerState::ContainerRunning,
        ContainerState::Exited => crate::cri_api::ContainerState::ContainerExited,
    }
}

fn sandbox_state_to_cri(state: SandboxState) -> PodSandboxState {
    match state {
        SandboxState::Ready => PodSandboxState::SandboxReady,
        SandboxState::NotReady | SandboxState::Removed => PodSandboxState::SandboxNotready,
    }
}

fn container_summary(container: Container) -> crate::cri_api::Container {
    let status_image_ref = container.status_image_ref().to_string();

    crate::cri_api::Container {
        id: container.id,
        pod_sandbox_id: container.sandbox_id,
        metadata: Some(ContainerMetadata {
            name: container.name,
            attempt: 0,
        }),
        image: Some(ImageSpec {
            image: container.image_ref.clone(),
            annotations: Default::default(),
        }),
        image_ref: status_image_ref,
        state: container_state_to_cri(container.state).into(),
        created_at: container.created_at,
        labels: container.labels,
        annotations: container.annotations,
    }
}

fn sandbox_summary(sandbox: PodSandbox) -> crate::cri_api::PodSandbox {
    crate::cri_api::PodSandbox {
        id: sandbox.id,
        metadata: Some(PodSandboxMetadata {
            name: sandbox.name,
            uid: sandbox.uid,
            namespace: sandbox.namespace,
            attempt: 0,
        }),
        state: sandbox_state_to_cri(sandbox.state).into(),
        created_at: sandbox.created_at,
        labels: sandbox.labels,
        annotations: sandbox.annotations,
        runtime_handler: sandbox.runtime_handler,
    }
}

fn zero_cpu_usage(now_ns: i64) -> CpuUsage {
    CpuUsage {
        timestamp: now_ns,
        usage_core_nano_seconds: Some(UInt64Value { value: 0 }),
        usage_nano_cores: Some(UInt64Value { value: 0 }),
    }
}

fn zero_memory_usage(now_ns: i64) -> MemoryUsage {
    MemoryUsage {
        timestamp: now_ns,
        working_set_bytes: Some(UInt64Value { value: 0 }),
        available_bytes: Some(UInt64Value { value: 0 }),
        usage_bytes: Some(UInt64Value { value: 0 }),
        rss_bytes: Some(UInt64Value { value: 0 }),
        page_faults: Some(UInt64Value { value: 0 }),
        major_page_faults: Some(UInt64Value { value: 0 }),
    }
}

#[derive(Debug, Clone, Copy, Default)]
struct PathUsage {
    used_bytes: u64,
    inodes_used: u64,
}

impl PathUsage {
    fn add(&mut self, other: PathUsage) {
        self.used_bytes = self.used_bytes.saturating_add(other.used_bytes);
        self.inodes_used = self.inodes_used.saturating_add(other.inodes_used);
    }
}

fn should_collect_rootfs_usage(rootfs_path: &str) -> bool {
    let trimmed = rootfs_path.trim();
    !trimmed.is_empty() && Path::new(trimmed) != Path::new("/")
}

fn collect_path_usage(path: &Path) -> std::io::Result<PathUsage> {
    let metadata = std::fs::symlink_metadata(path)?;
    let mut usage = PathUsage {
        used_bytes: metadata.len(),
        inodes_used: 1,
    };

    if metadata.is_dir() {
        for entry in std::fs::read_dir(path)? {
            usage.add(collect_path_usage(&entry?.path())?);
        }
    }

    Ok(usage)
}

async fn rootfs_path_usage(rootfs_path: &str) -> Option<PathUsage> {
    if !should_collect_rootfs_usage(rootfs_path) {
        return None;
    }

    let path = PathBuf::from(rootfs_path);
    let display_path = path.display().to_string();
    match tokio::task::spawn_blocking(move || collect_path_usage(&path)).await {
        Ok(Ok(usage)) => Some(usage),
        Ok(Err(error)) => {
            tracing::debug!(
                rootfs_path = %display_path,
                error = %error,
                "Failed to collect CRI container rootfs usage"
            );
            None
        }
        Err(error) => {
            tracing::warn!(
                rootfs_path = %display_path,
                error = %error,
                "CRI container rootfs usage collection task failed"
            );
            None
        }
    }
}

fn writable_layer_usage(
    rootfs_path: &str,
    usage: Option<PathUsage>,
    now_ns: i64,
) -> FilesystemUsage {
    FilesystemUsage {
        timestamp: now_ns,
        fs_id: should_collect_rootfs_usage(rootfs_path).then(|| FilesystemIdentifier {
            mountpoint: rootfs_path.to_string(),
        }),
        used_bytes: Some(UInt64Value {
            value: usage.map(|usage| usage.used_bytes).unwrap_or(0),
        }),
        inodes_used: usage.map(|usage| UInt64Value {
            value: usage.inodes_used,
        }),
    }
}

fn container_path_inside_rootfs(rootfs: &Path, container_path: &str) -> Result<PathBuf, Status> {
    let path = Path::new(container_path);
    if !path.is_absolute() {
        return Err(Status::invalid_argument(format!(
            "CRI mount container_path must be absolute: {container_path}"
        )));
    }

    let mut relative = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::RootDir | std::path::Component::CurDir => {}
            std::path::Component::Normal(part) => relative.push(part),
            std::path::Component::ParentDir | std::path::Component::Prefix(_) => {
                return Err(Status::invalid_argument(format!(
                    "CRI mount container_path must not escape the rootfs: {container_path}"
                )));
            }
        }
    }

    if relative.as_os_str().is_empty() {
        return Err(Status::invalid_argument(
            "CRI mount container_path must not be the rootfs",
        ));
    }

    Ok(rootfs.join(relative))
}

fn remove_existing_mount_target(path: &Path) -> std::io::Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() => std::fs::remove_dir_all(path),
        Ok(_) => std::fs::remove_file(path),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

fn copy_mount_source(source: &Path, target: &Path) -> std::io::Result<()> {
    let metadata = std::fs::metadata(source)?;
    if metadata.is_dir() {
        std::fs::create_dir_all(target)?;
        for entry in std::fs::read_dir(source)? {
            let entry = entry?;
            copy_mount_source(&entry.path(), &target.join(entry.file_name()))?;
        }
    } else {
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::copy(source, target)?;
    }

    Ok(())
}

fn materialize_readonly_container_mount(
    rootfs: &Path,
    mount: &ContainerMount,
) -> Result<(), Status> {
    let source = Path::new(&mount.host_path);
    if !source.exists() {
        return Err(Status::failed_precondition(format!(
            "CRI mount host_path does not exist: {}",
            mount.host_path
        )));
    }

    let target = container_path_inside_rootfs(rootfs, &mount.container_path)?;
    remove_existing_mount_target(&target).map_err(|e| {
        Status::internal(format!(
            "Failed to clear CRI mount target {}: {e}",
            target.display()
        ))
    })?;
    copy_mount_source(source, &target).map_err(|e| {
        Status::internal(format!(
            "Failed to materialize CRI read-only mount {} -> {}: {e}",
            source.display(),
            target.display()
        ))
    })
}

async fn container_stats(container: &Container) -> ContainerStats {
    let now_ns = chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0);
    let rootfs_usage = rootfs_path_usage(&container.rootfs_path).await;
    ContainerStats {
        attributes: Some(ContainerAttributes {
            id: container.id.clone(),
            metadata: Some(ContainerMetadata {
                name: container.name.clone(),
                attempt: 0,
            }),
            labels: container.labels.clone(),
            annotations: container.annotations.clone(),
        }),
        cpu: Some(zero_cpu_usage(now_ns)),
        memory: Some(zero_memory_usage(now_ns)),
        writable_layer: Some(writable_layer_usage(
            &container.rootfs_path,
            rootfs_usage,
            now_ns,
        )),
    }
}

async fn pod_sandbox_stats(sandbox: &PodSandbox, containers: Vec<Container>) -> PodSandboxStats {
    let now_ns = chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0);
    let running_containers: Vec<Container> = containers
        .iter()
        .filter(|container| container.state == ContainerState::Running)
        .cloned()
        .collect();
    let container_stats = join_all(running_containers.iter().map(container_stats)).await;

    PodSandboxStats {
        attributes: Some(PodSandboxAttributes {
            id: sandbox.id.clone(),
            metadata: Some(PodSandboxMetadata {
                name: sandbox.name.clone(),
                uid: sandbox.uid.clone(),
                namespace: sandbox.namespace.clone(),
                attempt: 0,
            }),
            labels: sandbox.labels.clone(),
            annotations: sandbox.annotations.clone(),
        }),
        linux: Some(LinuxPodSandboxStats {
            cpu: Some(zero_cpu_usage(now_ns)),
            memory: Some(zero_memory_usage(now_ns)),
            network: Some(NetworkUsage {
                timestamp: now_ns,
                default_interface: None,
                interfaces: vec![],
            }),
            process: Some(ProcessUsage {
                timestamp: now_ns,
                process_count: Some(UInt64Value {
                    value: containers
                        .iter()
                        .filter(|container| container.state == ContainerState::Running)
                        .count() as u64,
                }),
            }),
        }),
        containers: container_stats,
    }
}

fn metric_descriptors() -> Vec<MetricDescriptor> {
    vec![
        MetricDescriptor {
            name: "a3s_box_pod_sandbox_ready".to_string(),
            help: "Whether the CRI pod sandbox is Ready according to the runtime store."
                .to_string(),
            kind: "gauge".to_string(),
            unit: "1".to_string(),
        },
        MetricDescriptor {
            name: "a3s_box_pod_sandbox_vm_manager_present".to_string(),
            help: "Whether the runtime has an in-process VM manager for the pod sandbox."
                .to_string(),
            kind: "gauge".to_string(),
            unit: "1".to_string(),
        },
        MetricDescriptor {
            name: "a3s_box_pod_sandbox_containers_total".to_string(),
            help: "Number of containers tracked for the pod sandbox.".to_string(),
            kind: "gauge".to_string(),
            unit: "count".to_string(),
        },
        MetricDescriptor {
            name: "a3s_box_pod_sandbox_containers_running".to_string(),
            help: "Number of running containers tracked for the pod sandbox.".to_string(),
            kind: "gauge".to_string(),
            unit: "count".to_string(),
        },
        MetricDescriptor {
            name: "a3s_box_pod_sandbox_containers_exited".to_string(),
            help: "Number of exited containers tracked for the pod sandbox.".to_string(),
            kind: "gauge".to_string(),
            unit: "count".to_string(),
        },
    ]
}

fn pod_sandbox_metric_labels(sandbox: &PodSandbox) -> HashMap<String, String> {
    HashMap::from([
        ("pod_sandbox_id".to_string(), sandbox.id.clone()),
        ("namespace".to_string(), sandbox.namespace.clone()),
        ("name".to_string(), sandbox.name.clone()),
        ("uid".to_string(), sandbox.uid.clone()),
        (
            "runtime_handler".to_string(),
            sandbox.runtime_handler.clone(),
        ),
    ])
}

fn pod_metric(name: &str, labels: &HashMap<String, String>, value: f64, timestamp: i64) -> Metric {
    Metric {
        name: name.to_string(),
        labels: labels.clone(),
        value,
        timestamp,
    }
}

fn pod_sandbox_metrics(
    sandbox: &PodSandbox,
    containers: &[Container],
    vm_manager_present: bool,
) -> PodSandboxMetrics {
    let now_ns = chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0);
    let labels = pod_sandbox_metric_labels(sandbox);
    let running_containers = containers
        .iter()
        .filter(|container| container.state == ContainerState::Running)
        .count();
    let exited_containers = containers
        .iter()
        .filter(|container| container.state == ContainerState::Exited)
        .count();

    PodSandboxMetrics {
        pod_sandbox_id: sandbox.id.clone(),
        metrics: vec![
            pod_metric(
                "a3s_box_pod_sandbox_ready",
                &labels,
                if sandbox.state == SandboxState::Ready {
                    1.0
                } else {
                    0.0
                },
                now_ns,
            ),
            pod_metric(
                "a3s_box_pod_sandbox_vm_manager_present",
                &labels,
                if vm_manager_present { 1.0 } else { 0.0 },
                now_ns,
            ),
            pod_metric(
                "a3s_box_pod_sandbox_containers_total",
                &labels,
                containers.len() as f64,
                now_ns,
            ),
            pod_metric(
                "a3s_box_pod_sandbox_containers_running",
                &labels,
                running_containers as f64,
                now_ns,
            ),
            pod_metric(
                "a3s_box_pod_sandbox_containers_exited",
                &labels,
                exited_containers as f64,
                now_ns,
            ),
        ],
    }
}

fn container_event_response(
    container_id: &str,
    pod_sandbox_id: &str,
    container_event_type: ContainerEventType,
    created_at: i64,
    reason: impl Into<String>,
    message: impl Into<String>,
) -> ContainerEventResponse {
    ContainerEventResponse {
        container_id: container_id.to_string(),
        pod_sandbox_id: pod_sandbox_id.to_string(),
        container_event_type: container_event_type as i32,
        created_at,
        reason: reason.into(),
        message: message.into(),
    }
}

async fn ensure_vm_ready(vm: &VmManager, operation: &str, sandbox_id: &str) -> Result<(), Status> {
    if !vm
        .health_check()
        .await
        .map_err(|e| Status::internal(format!("Failed to check VM health: {}", e)))?
    {
        return Err(Status::failed_precondition(format!(
            "{operation} requires a ready VM; sandbox {sandbox_id} VM is not ready",
        )));
    }

    Ok(())
}

fn stop_container_timeout_ms(timeout_seconds: i64) -> Option<u64> {
    if timeout_seconds <= 0 {
        return None;
    }

    Some((timeout_seconds as u64).saturating_mul(1_000))
}

fn stop_container_wait_duration(timeout_seconds: i64) -> tokio::time::Duration {
    if timeout_seconds <= 0 {
        return tokio::time::Duration::from_secs(DEFAULT_STOP_CONTAINER_WAIT_SECS);
    }

    tokio::time::Duration::from_secs(timeout_seconds as u64)
}

struct CriLogWriter {
    file: tokio::fs::File,
    stdout_partial: Vec<u8>,
    stderr_partial: Vec<u8>,
}

impl CriLogWriter {
    async fn open(log_path: &str) -> std::io::Result<Option<Self>> {
        if log_path.is_empty() {
            return Ok(None);
        }

        let path = std::path::Path::new(log_path);
        if let Some(parent) = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            tokio::fs::create_dir_all(parent).await?;
        }

        let file = tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .await?;

        Ok(Some(Self {
            file,
            stdout_partial: Vec::new(),
            stderr_partial: Vec::new(),
        }))
    }

    async fn write_chunk(
        &mut self,
        stream: a3s_box_core::exec::StreamType,
        data: &[u8],
    ) -> std::io::Result<()> {
        let partial = match stream {
            a3s_box_core::exec::StreamType::Stdout => &mut self.stdout_partial,
            a3s_box_core::exec::StreamType::Stderr => &mut self.stderr_partial,
        };

        partial.extend_from_slice(data);
        let mut complete_lines = Vec::new();
        while let Some(newline) = partial.iter().position(|byte| *byte == b'\n') {
            let mut line: Vec<u8> = partial.drain(..=newline).collect();
            line.pop();
            complete_lines.push(line);
        }

        for line in complete_lines {
            self.write_record(stream, &line).await?;
        }

        Ok(())
    }

    async fn flush_partials(&mut self) -> std::io::Result<()> {
        if !self.stdout_partial.is_empty() {
            let line = std::mem::take(&mut self.stdout_partial);
            self.write_record(a3s_box_core::exec::StreamType::Stdout, &line)
                .await?;
        }
        if !self.stderr_partial.is_empty() {
            let line = std::mem::take(&mut self.stderr_partial);
            self.write_record(a3s_box_core::exec::StreamType::Stderr, &line)
                .await?;
        }

        self.file.flush().await
    }

    async fn write_record(
        &mut self,
        stream: a3s_box_core::exec::StreamType,
        line: &[u8],
    ) -> std::io::Result<()> {
        let timestamp = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Nanos, true);
        let stream = match stream {
            a3s_box_core::exec::StreamType::Stdout => "stdout",
            a3s_box_core::exec::StreamType::Stderr => "stderr",
        };

        self.file.write_all(timestamp.as_bytes()).await?;
        self.file.write_all(b" ").await?;
        self.file.write_all(stream.as_bytes()).await?;
        self.file.write_all(b" F ").await?;
        self.file.write_all(line).await?;
        self.file.write_all(b"\n").await
    }
}

fn spawn_container_exit_supervisor(
    store: Arc<PersistentCriStore>,
    attach_streams: AttachStreamMap,
    workload_stdins: WorkloadStdinMap,
    workload_stops: WorkloadStopMap,
    container_events: ContainerEventSender,
    container_id: String,
    sandbox_id: String,
    log_path: String,
    attach_tx: AttachStreamSender,
    stop_rx: oneshot::Receiver<()>,
    workload: SupervisedWorkload,
) {
    tokio::spawn(async move {
        let mut workload = workload;
        let mut stop_rx = stop_rx;
        let mut stop_requested = false;
        let mut log_writer = match CriLogWriter::open(&log_path).await {
            Ok(log_writer) => log_writer,
            Err(error) => {
                tracing::warn!(
                    container_id = %container_id,
                    sandbox_id = %sandbox_id,
                    log_path = %log_path,
                    error = %error,
                    "Failed to open CRI container log"
                );
                None
            }
        };
        let mut exit_code = -1;

        loop {
            tokio::select! {
                stop = &mut stop_rx, if !stop_requested => {
                    stop_requested = true;
                    if stop.is_ok() {
                        tracing::info!(
                            container_id = %container_id,
                            sandbox_id = %sandbox_id,
                            "Stopping CRI container workload through streaming exec control"
                        );
                        if let Err(error) = workload.cancel().await {
                            tracing::warn!(
                                container_id = %container_id,
                                sandbox_id = %sandbox_id,
                                error = %error,
                                "Failed to send CRI container workload stop control"
                            );
                        }
                    }
                }
                event = workload.next_event() => {
                    match event {
                        Ok(Some(a3s_box_core::exec::ExecEvent::Chunk(chunk))) => {
                            let _ = attach_tx.send(a3s_box_core::exec::ExecEvent::Chunk(chunk.clone()));
                            if let Some(writer) = log_writer.as_mut() {
                                if let Err(error) = writer.write_chunk(chunk.stream, &chunk.data).await {
                                    tracing::warn!(
                                        container_id = %container_id,
                                        sandbox_id = %sandbox_id,
                                        log_path = %log_path,
                                        error = %error,
                                        "Failed to write CRI container log; disabling log writes for this workload"
                                    );
                                    log_writer = None;
                                }
                            }
                        }
                        Ok(Some(a3s_box_core::exec::ExecEvent::Exit(exit))) => {
                            exit_code = exit.exit_code;
                            break;
                        }
                        Ok(None) => break,
                        Err(error) => {
                            tracing::warn!(
                                container_id = %container_id,
                                sandbox_id = %sandbox_id,
                                error = %error,
                                "Container workload supervision failed; recording synthetic failure exit"
                            );
                            exit_code = 255;
                            break;
                        }
                    }
                }
            }
        }

        if let Some(writer) = log_writer.as_mut() {
            if let Err(error) = writer.flush_partials().await {
                tracing::warn!(
                    container_id = %container_id,
                    sandbox_id = %sandbox_id,
                    log_path = %log_path,
                    error = %error,
                    "Failed to flush CRI container log"
                );
            }
        }

        if exit_code < 0 {
            tracing::warn!(
                container_id = %container_id,
                sandbox_id = %sandbox_id,
                exit_code,
                "Container workload stream ended without a valid exit code; recording synthetic failure"
            );
            exit_code = 255;
        }
        let _ = attach_tx.send(a3s_box_core::exec::ExecEvent::Exit(
            a3s_box_core::exec::ExecExit { exit_code },
        ));

        let finished_ns = chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0);
        let updated = store
            .mark_container_exited_if_running(&container_id, finished_ns, exit_code)
            .await;

        if updated {
            tracing::info!(
                container_id = %container_id,
                sandbox_id = %sandbox_id,
                exit_code,
                "Container workload exited"
            );
            let _ = container_events.send(container_event_response(
                &container_id,
                &sandbox_id,
                ContainerEventType::ContainerStoppedEvent,
                finished_ns,
                "ContainerStopped",
                format!("Container workload exited with code {exit_code}"),
            ));
        } else {
            tracing::debug!(
                container_id = %container_id,
                sandbox_id = %sandbox_id,
                exit_code,
                "Container exit was already recorded by another lifecycle path"
            );
        }

        attach_streams.write().await.remove(&container_id);
        workload_stdins.write().await.remove(&container_id);
        workload_stops.write().await.remove(&container_id);
    });
}

/// A3S Box implementation of the CRI RuntimeService.
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

    /// Load persisted state from disk. Call once after construction.
    pub async fn load_state(&self) {
        if let Err(e) = self.store.load().await {
            tracing::warn!(error = %e, "Failed to load persisted CRI state — starting fresh");
        }
    }

    async fn connect_sandbox_network(
        &self,
        box_config: &a3s_box_core::config::BoxConfig,
        sandbox_id: &str,
        pod_name: &str,
    ) -> Result<Option<SandboxNetworkAllocation>, Status> {
        let Some(network_name) = bridge_network_name(box_config) else {
            return Ok(None);
        };

        let sandbox_id = sandbox_id.to_string();
        let pod_name = pod_name.to_string();
        let store = self.network_store.clone();
        tokio::task::spawn_blocking(move || {
            connect_sandbox_to_network_store(&store, &network_name, &sandbox_id, &pod_name)
                .map(Some)
        })
        .await
        .map_err(|e| Status::internal(format!("CRI sandbox network allocation task failed: {e}")))?
    }

    async fn disconnect_sandbox_network_by_name(&self, network_name: &str, sandbox_id: &str) {
        let network_name = network_name.to_string();
        let sandbox_id = sandbox_id.to_string();
        let task_network_name = network_name.clone();
        let task_sandbox_id = sandbox_id.clone();
        let store = self.network_store.clone();
        match tokio::task::spawn_blocking(move || {
            disconnect_sandbox_from_network_store(&store, &task_network_name, &task_sandbox_id)
        })
        .await
        {
            Ok(Ok(())) => {}
            Ok(Err(error)) => {
                tracing::warn!(
                    sandbox_id = %sandbox_id,
                    network = %network_name,
                    error = %error,
                    "Failed to disconnect CRI sandbox from network"
                );
            }
            Err(error) => {
                tracing::warn!(
                    sandbox_id = %sandbox_id,
                    network = %network_name,
                    error = %error,
                    "CRI sandbox network cleanup task failed"
                );
            }
        }
    }

    async fn disconnect_sandbox_network(&self, sandbox: &PodSandbox) {
        if let Some(network_name) = sandbox_network_name(sandbox) {
            self.disconnect_sandbox_network_by_name(&network_name, &sandbox.id)
                .await;
        }
    }

    async fn resolve_container_image(
        &self,
        image_ref: &str,
    ) -> Result<Option<ResolvedContainerImage>, Status> {
        if image_ref.is_empty() {
            return Ok(None);
        }

        let Some(stored) = self.image_store.get(image_ref).await else {
            return Err(Status::not_found(format!(
                "Image not found locally: {image_ref}; pull it before CreateContainer"
            )));
        };

        let image = a3s_box_runtime::OciImage::from_path(&stored.path)
            .map_err(|e| Status::failed_precondition(format!("Invalid stored image: {e}")))?;
        Ok(Some(ResolvedContainerImage {
            digest: stored.digest,
            path: stored.path.to_string_lossy().to_string(),
            config: image.config().clone(),
        }))
    }

    fn container_rootfs_base(&self) -> PathBuf {
        self.image_store
            .store_dir()
            .join(CRI_CONTAINER_ROOTFS_HOST_DIR)
    }

    fn container_rootfs_paths(&self, sandbox_id: &str, container_id: &str) -> ContainerRootfsPaths {
        let sandbox_component = sanitize_path_component(sandbox_id);
        let container_component = sanitize_path_component(container_id);
        let relative = PathBuf::from(&sandbox_component)
            .join(&container_component)
            .join("rootfs");

        ContainerRootfsPaths {
            host_path: self.container_rootfs_base().join(relative),
            guest_path: format!(
                "{}/{}/{}/rootfs",
                CRI_CONTAINER_ROOTFS_GUEST_BASE, sandbox_component, container_component
            ),
        }
    }

    async fn ensure_container_rootfs_mount_base(&self) -> Result<PathBuf, Status> {
        let rootfs_base = self.container_rootfs_base();
        tokio::fs::create_dir_all(&rootfs_base).await.map_err(|e| {
            Status::internal(format!(
                "Failed to create CRI container rootfs mount base {}: {}",
                rootfs_base.display(),
                e
            ))
        })?;
        Ok(rootfs_base)
    }

    async fn prepare_container_rootfs(
        &self,
        image: &ResolvedContainerImage,
        paths: &ContainerRootfsPaths,
    ) -> Result<(), Status> {
        let image_path = PathBuf::from(&image.path);
        let rootfs_path = paths.host_path.clone();

        tokio::task::spawn_blocking(move || {
            OciRootfsBuilder::new(&rootfs_path)
                .with_image(&image_path)
                .build()
        })
        .await
        .map_err(|e| Status::internal(format!("Container rootfs build task failed: {e}")))?
        .map_err(|e| {
            Status::failed_precondition(format!("Failed to prepare container rootfs: {e}"))
        })
    }

    async fn materialize_readonly_container_mounts(
        &self,
        rootfs_path: &str,
        mounts: &[ContainerMount],
    ) -> Result<(), Status> {
        if mounts.is_empty() {
            return Ok(());
        }
        if rootfs_path.trim().is_empty() {
            return Err(Status::failed_precondition(
                "CRI mounts require a prepared container rootfs",
            ));
        }

        let rootfs = PathBuf::from(rootfs_path);
        let mounts = mounts.to_vec();
        tokio::task::spawn_blocking(move || {
            for mount in &mounts {
                materialize_readonly_container_mount(&rootfs, mount)?;
            }
            Ok::<(), Status>(())
        })
        .await
        .map_err(|e| Status::internal(format!("CRI mount materialization task failed: {e}")))?
    }

    async fn cleanup_container_rootfs_path(&self, rootfs_path: &str) {
        if rootfs_path.trim().is_empty() {
            return;
        }

        let rootfs_path = PathBuf::from(rootfs_path);
        if !rootfs_path.starts_with(self.container_rootfs_base()) {
            tracing::debug!(
                path = %rootfs_path.display(),
                "Skipping CRI rootfs cleanup outside managed rootfs base"
            );
            return;
        }

        match tokio::fs::remove_dir_all(&rootfs_path).await {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                tracing::warn!(
                    path = %rootfs_path.display(),
                    error = %e,
                    "Failed to remove CRI container rootfs"
                );
            }
        }
    }

    async fn cleanup_sandbox_rootfs(&self, sandbox_id: &str) {
        let path = self
            .container_rootfs_base()
            .join(sanitize_path_component(sandbox_id));
        match tokio::fs::remove_dir_all(&path).await {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                tracing::warn!(
                    path = %path.display(),
                    error = %e,
                    "Failed to remove CRI sandbox container rootfs directory"
                );
            }
        }
    }

    async fn stop_container_vm(
        &self,
        container: &Container,
        timeout_seconds: i64,
    ) -> Result<(), Status> {
        let destroyed = self
            .destroy_sandbox_vm(
                &container.sandbox_id,
                stop_container_timeout_ms(timeout_seconds),
            )
            .await?;

        if !destroyed {
            tracing::warn!(
                container_id = %container.id,
                sandbox_id = %container.sandbox_id,
                "StopContainer reconciled a running container without an active VM manager"
            );
        }

        self.store
            .update_sandbox_state(&container.sandbox_id, SandboxState::NotReady)
            .await;
        if let Some(sandbox) = self.store.sandboxes.get(&container.sandbox_id).await {
            self.disconnect_sandbox_network(&sandbox).await;
        }

        Ok(())
    }

    async fn stop_container_workload(
        &self,
        container: &Container,
        timeout_seconds: i64,
    ) -> Result<bool, Status> {
        let Some(stop_tx) = self.workload_stops.write().await.remove(&container.id) else {
            return Ok(false);
        };

        let _ = stop_tx.send(());
        let wait_for = stop_container_wait_duration(timeout_seconds);
        let deadline = tokio::time::Instant::now() + wait_for;

        loop {
            if let Some(current) = self.store.containers.get(&container.id).await {
                if current.state == ContainerState::Exited {
                    tracing::info!(
                        container_id = %container.id,
                        sandbox_id = %container.sandbox_id,
                        "CRI StopContainer stopped workload without tearing down sandbox VM"
                    );
                    return Ok(true);
                }
            } else {
                return Ok(true);
            }

            if tokio::time::Instant::now() >= deadline {
                tracing::warn!(
                    container_id = %container.id,
                    sandbox_id = %container.sandbox_id,
                    timeout_secs = wait_for.as_secs(),
                    "Timed out waiting for CRI container workload stop; falling back to sandbox VM teardown"
                );
                return Ok(false);
            }

            tokio::time::sleep(tokio::time::Duration::from_millis(20)).await;
        }
    }

    async fn has_other_running_containers(&self, container: &Container) -> bool {
        self.store
            .containers
            .list(Some(&container.sandbox_id), None)
            .await
            .into_iter()
            .any(|other| other.id != container.id && other.state == ContainerState::Running)
    }

    fn emit_container_event(
        &self,
        container_id: &str,
        sandbox_id: &str,
        container_event_type: ContainerEventType,
        created_at: i64,
        reason: impl Into<String>,
        message: impl Into<String>,
    ) {
        let event = container_event_response(
            container_id,
            sandbox_id,
            container_event_type,
            created_at,
            reason,
            message,
        );
        let _ = self.container_events.send(event);
    }

    async fn acquire_vm_with_box_id(
        &self,
        box_config: a3s_box_core::config::BoxConfig,
        box_id: String,
    ) -> Result<VmManager, Status> {
        self.acquire_vm_inner(box_config, Some(box_id)).await
    }

    async fn acquire_vm_inner(
        &self,
        box_config: a3s_box_core::config::BoxConfig,
        box_id: Option<String>,
    ) -> Result<VmManager, Status> {
        if let Some(ref pool) = self.warm_pool {
            if box_id.is_some() {
                tracing::debug!(
                    "Skipping warm pool because this sandbox requires a preallocated box ID"
                );
            } else if !box_config.volumes.is_empty() {
                tracing::debug!(
                    volume_count = box_config.volumes.len(),
                    "Skipping warm pool because this sandbox requires explicit volume mounts"
                );
            } else {
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
        }

        // Cold boot
        #[cfg(test)]
        if let Some(error) = &self.test_vm_acquire_error {
            return Err(Status::internal(error.clone()));
        }

        #[cfg(test)]
        if let Some(exec_socket_path) = &self.test_vm_exec_socket_path {
            let box_id = box_id.unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
            let mut vm = VmManager::with_box_id(box_config, EventEmitter::new(256), box_id);
            vm.attach_running_process(
                std::process::id(),
                exec_socket_path.clone(),
                Some(exec_socket_path.with_file_name("pty.sock")),
            )
            .await
            .map_err(box_error_to_status)?;
            return Ok(vm);
        }

        let event_emitter = EventEmitter::new(256);
        let mut vm = match box_id {
            Some(box_id) => VmManager::with_box_id(box_config, event_emitter, box_id),
            None => VmManager::new(box_config, event_emitter),
        };
        vm.boot().await.map_err(box_error_to_status)?;
        Ok(vm)
    }

    async fn destroy_sandbox_vm(
        &self,
        sandbox_id: &str,
        timeout_ms: Option<u64>,
    ) -> Result<bool, Status> {
        let vm = self.vm_managers.write().await.remove(sandbox_id);
        let Some(mut vm) = vm else {
            return Ok(false);
        };

        match timeout_ms {
            Some(timeout_ms) => vm
                .destroy_with_timeout(timeout_ms)
                .await
                .map_err(box_error_to_status)?,
            None => vm.destroy().await.map_err(box_error_to_status)?,
        }

        Ok(true)
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
        let stop_results = join_all(containers.iter().filter_map(|container| {
            (container.state == ContainerState::Running).then(|| async move {
                let stopped = self.stop_container_workload(container, 0).await?;
                Ok::<_, Status>((container, stopped))
            })
        }))
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
            for container in &containers {
                attach_streams.remove(&container.id);
                workload_stdins.remove(&container.id);
                workload_stops.remove(&container.id);
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
            for container in &removed_containers {
                attach_streams.remove(&container.id);
                workload_stdins.remove(&container.id);
                workload_stops.remove(&container.id);
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
        let env = merge_env(image_env, &config.envs);
        let working_dir = if config.working_dir.is_empty() {
            image_config
                .and_then(|image| image.working_dir.clone())
                .unwrap_or_default()
        } else {
            config.working_dir.clone()
        };
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
            log_path: config.log_path,
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

        let exec_request = container
            .to_exec_request(a3s_box_core::exec::DEFAULT_EXEC_TIMEOUT_NS)
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
            self.emit_container_event(
                &container_id,
                &container.sandbox_id,
                ContainerEventType::ContainerStartedEvent,
                now_ns,
                "ContainerStarted",
                format!("Container {} started", container.name),
            );
        }

        spawn_container_exit_supervisor(
            self.store.clone(),
            self.attach_streams.clone(),
            self.workload_stdins.clone(),
            self.workload_stops.clone(),
            self.container_events.clone(),
            container_id.clone(),
            container.sandbox_id.clone(),
            container.log_path.clone(),
            attach_tx,
            stop_rx,
            workload,
        );

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
            env: vec![],
            working_dir: None,
            rootfs: if container.rootfs_guest_path.is_empty() {
                None
            } else {
                Some(container.rootfs_guest_path.clone())
            },
            stdin: None,
            stdin_streaming: false,
            user: None,
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
    use std::path::{Path, PathBuf};

    use futures::StreamExt;
    use tokio::io::AsyncWriteExt;
    use tokio::net::UnixListener;
    use tokio::time::{sleep, Duration};

    use crate::streaming::StreamingServer;

    /// Create a BoxRuntimeService for testing.
    /// Uses NoopStateStore (no disk I/O) and a dummy StreamingHandle.
    fn make_test_service() -> BoxRuntimeService {
        let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
        let streaming_server = StreamingServer::new(addr);
        let handle = streaming_server.handle();
        let (image_store, image_store_tempdir) = make_test_image_store();
        let (network_store, network_store_tempdir) = make_test_network_store();

        BoxRuntimeService {
            store: Arc::new(PersistentCriStore::new(Arc::new(NoopStateStore))),
            image_store,
            network_store,
            _image_store_tempdir: Some(image_store_tempdir),
            _network_store_tempdir: Some(network_store_tempdir),
            vm_managers: Arc::new(RwLock::new(HashMap::new())),
            streaming: handle,
            attach_streams: Arc::new(RwLock::new(HashMap::new())),
            workload_stdins: Arc::new(RwLock::new(HashMap::new())),
            workload_stops: Arc::new(RwLock::new(HashMap::new())),
            container_events: broadcast::channel(CONTAINER_EVENT_BUFFER).0,
            warm_pool: None,
            runtime_options: CriRuntimeOptions::default(),
            test_vm_acquire_error: None,
            test_vm_exec_socket_path: None,
        }
    }

    fn make_test_image_store() -> (Arc<ImageStore>, Arc<tempfile::TempDir>) {
        let tmp = tempfile::tempdir().unwrap();
        let store_dir = tmp.path().join("images");
        let store = Arc::new(ImageStore::new(&store_dir, 100 * 1024 * 1024).unwrap());
        (store, Arc::new(tmp))
    }

    fn make_test_network_store() -> (Arc<NetworkStore>, Arc<tempfile::TempDir>) {
        let tmp = tempfile::tempdir().unwrap();
        let store = Arc::new(NetworkStore::new(tmp.path().join("networks.json")));
        (store, Arc::new(tmp))
    }

    #[test]
    fn test_sandbox_network_status_from_annotations() {
        let annotations = HashMap::from([
            (ANN_POD_IP.to_string(), "10.244.0.12".to_string()),
            (
                ANN_ADDITIONAL_POD_IPS.to_string(),
                "fd00::12, 10.244.0.13".to_string(),
            ),
        ]);

        let (network_ip, additional_ips) =
            sandbox_network_status_from_annotations(&annotations).unwrap();

        assert_eq!(network_ip, "10.244.0.12");
        assert_eq!(additional_ips, vec!["fd00::12", "10.244.0.13"]);
    }

    #[test]
    fn test_sandbox_network_status_rejects_invalid_primary_ip() {
        let annotations = HashMap::from([(ANN_POD_IP.to_string(), "not-an-ip".to_string())]);

        let err = sandbox_network_status_from_annotations(&annotations).unwrap_err();

        assert_eq!(err.code(), tonic::Code::InvalidArgument);
        assert!(err.message().contains("Invalid CRI sandbox IP"));
    }

    #[test]
    fn test_sandbox_network_status_requires_primary_for_additional_ips() {
        let annotations =
            HashMap::from([(ANN_ADDITIONAL_POD_IPS.to_string(), "fd00::12".to_string())]);

        let err = sandbox_network_status_from_annotations(&annotations).unwrap_err();

        assert_eq!(err.code(), tonic::Code::InvalidArgument);
        assert!(err.message().contains(ANN_POD_IP));
    }

    #[test]
    fn test_connect_sandbox_to_network_store_allocates_pod_ip() {
        let dir = tempfile::tempdir().unwrap();
        let store = NetworkStore::new(dir.path().join("networks.json"));
        store
            .create(a3s_box_core::NetworkConfig::new("cri-net", "10.244.0.0/24").unwrap())
            .unwrap();

        let allocation =
            connect_sandbox_to_network_store(&store, "cri-net", "sb-1", "pod-sb-1").unwrap();

        assert_eq!(allocation.network_name, "cri-net");
        assert_eq!(allocation.ip, "10.244.0.2");

        let network = store.get("cri-net").unwrap().unwrap();
        let endpoint = network.endpoints.get("sb-1").unwrap();
        assert_eq!(endpoint.box_name, "pod-sb-1");
        assert_eq!(endpoint.ip_address.to_string(), "10.244.0.2");
    }

    #[test]
    fn test_disconnect_sandbox_from_network_store_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let store = NetworkStore::new(dir.path().join("networks.json"));
        store
            .create(a3s_box_core::NetworkConfig::new("cri-net", "10.244.0.0/24").unwrap())
            .unwrap();
        connect_sandbox_to_network_store(&store, "cri-net", "sb-1", "pod-sb-1").unwrap();

        disconnect_sandbox_from_network_store(&store, "cri-net", "sb-1").unwrap();
        disconnect_sandbox_from_network_store(&store, "cri-net", "sb-1").unwrap();

        let network = store.get("cri-net").unwrap().unwrap();
        assert!(network.endpoints.is_empty());
    }

    #[tokio::test]
    async fn test_run_pod_sandbox_cleans_network_endpoint_on_pod_ip_mismatch() {
        let svc = make_test_service();
        svc.network_store
            .create(a3s_box_core::NetworkConfig::new("cri-net", "10.244.0.0/24").unwrap())
            .unwrap();

        let result = svc
            .run_pod_sandbox(Request::new(RunPodSandboxRequest {
                config: Some(PodSandboxConfig {
                    metadata: Some(PodSandboxMetadata {
                        name: "pod-sb-1".to_string(),
                        uid: "uid-sb-1".to_string(),
                        namespace: "default".to_string(),
                        attempt: 0,
                    }),
                    log_directory: "/var/log/pods".to_string(),
                    annotations: HashMap::from([
                        (ANN_NETWORK.to_string(), "cri-net".to_string()),
                        (ANN_POD_IP.to_string(), "10.244.0.99".to_string()),
                    ]),
                    ..Default::default()
                }),
                runtime_handler: "a3s".to_string(),
            }))
            .await;

        assert!(result.is_err());
        let err = result.unwrap_err();
        assert_eq!(err.code(), tonic::Code::InvalidArgument);
        assert!(err
            .message()
            .contains("does not match allocated network IP"));

        let network = svc.network_store.get("cri-net").unwrap().unwrap();
        assert!(network.endpoints.is_empty());
        assert!(svc.store.sandboxes.list(None).await.is_empty());
    }

    #[tokio::test]
    async fn test_run_pod_sandbox_cleans_network_endpoint_on_vm_acquire_failure() {
        let mut svc = make_test_service();
        svc.test_vm_acquire_error = Some("forced VM acquire failure".to_string());
        svc.network_store
            .create(a3s_box_core::NetworkConfig::new("cri-net", "10.244.0.0/24").unwrap())
            .unwrap();

        let result = svc
            .run_pod_sandbox(Request::new(RunPodSandboxRequest {
                config: Some(PodSandboxConfig {
                    metadata: Some(PodSandboxMetadata {
                        name: "pod-sb-1".to_string(),
                        uid: "uid-sb-1".to_string(),
                        namespace: "default".to_string(),
                        attempt: 0,
                    }),
                    log_directory: "/var/log/pods".to_string(),
                    annotations: HashMap::from([(ANN_NETWORK.to_string(), "cri-net".to_string())]),
                    ..Default::default()
                }),
                runtime_handler: "a3s".to_string(),
            }))
            .await;

        assert!(result.is_err());
        let err = result.unwrap_err();
        assert_eq!(err.code(), tonic::Code::Internal);
        assert!(err.message().contains("forced VM acquire failure"));

        let network = svc.network_store.get("cri-net").unwrap().unwrap();
        assert!(network.endpoints.is_empty());
        assert!(svc.store.sandboxes.list(None).await.is_empty());
    }

    #[tokio::test]
    async fn test_cri_one_container_pod_smoke_flow() {
        let mut svc = make_test_service();
        svc.network_store
            .create(a3s_box_core::NetworkConfig::new("cri-net", "10.244.0.0/24").unwrap())
            .unwrap();
        put_test_oci_image(&svc.image_store, "example.com/app:latest").await;

        let expected_exec = Arc::new(std::sync::Mutex::new(
            None::<(Vec<String>, Vec<String>, String)>,
        ));
        let expected_exec_for_server = expected_exec.clone();
        let Some(exec_server) = spawn_exec_stream_server_with_assert(
            b"ready\n",
            b"",
            0,
            Duration::from_millis(100),
            move |request| {
                let expected = expected_exec_for_server.lock().unwrap();
                let (cmd, env, rootfs) = expected
                    .as_ref()
                    .expect("expected exec request should be set before StartContainer");
                assert_eq!(request.cmd.as_slice(), cmd.as_slice());
                assert_eq!(request.env.as_slice(), env.as_slice());
                assert_eq!(request.working_dir.as_deref(), Some("/image"));
                assert_eq!(request.user.as_deref(), Some("2000:2000"));
                assert_eq!(request.rootfs.as_deref(), Some(rootfs.as_str()));
            },
        )
        .await
        else {
            return;
        };
        svc.test_vm_exec_socket_path = Some(exec_server.socket_path.clone());

        let sandbox_id = svc
            .run_pod_sandbox(Request::new(RunPodSandboxRequest {
                config: Some(PodSandboxConfig {
                    metadata: Some(PodSandboxMetadata {
                        name: "pod-smoke".to_string(),
                        uid: "uid-smoke".to_string(),
                        namespace: "default".to_string(),
                        attempt: 0,
                    }),
                    log_directory: "/var/log/pods".to_string(),
                    annotations: HashMap::from([(ANN_NETWORK.to_string(), "cri-net".to_string())]),
                    ..Default::default()
                }),
                runtime_handler: "a3s".to_string(),
            }))
            .await
            .unwrap()
            .into_inner()
            .pod_sandbox_id;

        let sandbox_status = svc
            .pod_sandbox_status(Request::new(PodSandboxStatusRequest {
                pod_sandbox_id: sandbox_id.clone(),
                verbose: false,
            }))
            .await
            .unwrap()
            .into_inner()
            .status
            .unwrap();
        assert_eq!(sandbox_status.network.unwrap().ip, "10.244.0.2");
        assert!(svc
            .network_store
            .get("cri-net")
            .unwrap()
            .unwrap()
            .endpoints
            .contains_key(&sandbox_id));

        let log_dir = tempfile::tempdir().unwrap();
        let log_path = log_dir.path().join("container.log");
        let container_id = svc
            .create_container(Request::new(CreateContainerRequest {
                pod_sandbox_id: sandbox_id.clone(),
                config: Some(ContainerConfig {
                    metadata: Some(ContainerMetadata {
                        name: "app".to_string(),
                        attempt: 0,
                    }),
                    image: Some(ImageSpec {
                        image: "example.com/app:latest".to_string(),
                        annotations: HashMap::new(),
                    }),
                    log_path: log_path.to_string_lossy().to_string(),
                    ..Default::default()
                }),
                sandbox_config: None,
            }))
            .await
            .unwrap()
            .into_inner()
            .container_id;

        let container = svc.store.containers.get(&container_id).await.unwrap();
        *expected_exec.lock().unwrap() = Some((
            vec!["/usr/local/bin/app".to_string(), "serve".to_string()],
            vec![
                "PATH=/usr/local/bin:/usr/bin:/bin".to_string(),
                "ENV=image".to_string(),
            ],
            container.rootfs_guest_path.clone(),
        ));

        svc.start_container(Request::new(StartContainerRequest {
            container_id: container_id.clone(),
        }))
        .await
        .unwrap();

        let exited = tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                let container = svc.store.containers.get(&container_id).await.unwrap();
                if container.state == ContainerState::Exited {
                    break container;
                }
                sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("container should exit under background supervision");
        assert_eq!(exited.exit_code, 0);

        let log = tokio::fs::read_to_string(&log_path).await.unwrap();
        assert!(log.contains(" stdout F ready\n"));

        // Avoid destroying the attached current test process; lifecycle state
        // and network cleanup are still covered by the stop/remove calls.
        svc.vm_managers.write().await.remove(&sandbox_id);

        svc.stop_pod_sandbox(Request::new(StopPodSandboxRequest {
            pod_sandbox_id: sandbox_id.clone(),
        }))
        .await
        .unwrap();
        assert!(svc
            .network_store
            .get("cri-net")
            .unwrap()
            .unwrap()
            .endpoints
            .is_empty());

        svc.remove_pod_sandbox(Request::new(RemovePodSandboxRequest {
            pod_sandbox_id: sandbox_id.clone(),
        }))
        .await
        .unwrap();
        assert!(svc.store.sandboxes.get(&sandbox_id).await.is_none());
        assert!(svc.store.containers.get(&container_id).await.is_none());
    }

    struct TestExecServer {
        _tmp: tempfile::TempDir,
        socket_path: PathBuf,
    }

    struct TestPtyServer {
        _tmp: tempfile::TempDir,
        exec_socket_path: PathBuf,
        pty_socket_path: PathBuf,
    }

    fn bind_test_exec_listener(path: &Path) -> Option<UnixListener> {
        match UnixListener::bind(path) {
            Ok(listener) => Some(listener),
            Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => {
                eprintln!(
                    "skipping Unix socket test; sandbox denied bind at {}: {}",
                    path.display(),
                    error
                );
                None
            }
            Err(error) => panic!("failed to bind test socket {}: {}", path.display(), error),
        }
    }

    async fn spawn_exec_stream_server_with_assert<F>(
        stdout: &'static [u8],
        stderr: &'static [u8],
        exit_code: i32,
        exit_delay: Duration,
        assert_request: F,
    ) -> Option<TestExecServer>
    where
        F: FnOnce(&a3s_box_core::exec::ExecRequest) + Send + 'static,
    {
        let tmp = tempfile::Builder::new()
            .prefix("a3s-cri-exec-test")
            .tempdir_in("/private/tmp")
            .unwrap();
        let socket_path = tmp.path().join("exec.sock");
        let Some(listener) = bind_test_exec_listener(&socket_path) else {
            return None;
        };

        tokio::spawn(async move {
            let mut assert_request = Some(assert_request);

            loop {
                let (stream, _) = listener.accept().await.unwrap();
                let (r, w) = tokio::io::split(stream);
                let mut reader = a3s_transport::FrameReader::new(r);
                let mut writer = a3s_transport::FrameWriter::new(w);

                match reader.read_frame().await.unwrap() {
                    None => continue,
                    Some(frame) if frame.frame_type == a3s_transport::FrameType::Heartbeat => {
                        let heartbeat = a3s_transport::Frame::heartbeat();
                        let encoded = heartbeat.encode().unwrap();
                        writer.into_inner().write_all(&encoded).await.unwrap();
                    }
                    Some(frame) if frame.frame_type == a3s_transport::FrameType::Data => {
                        let request: a3s_box_core::exec::ExecRequest =
                            serde_json::from_slice(&frame.payload).unwrap();
                        assert!(request.streaming);
                        let Some(assert_request) = assert_request.take() else {
                            panic!("exec stream server received more than one request");
                        };
                        assert_request(&request);

                        if !stdout.is_empty() {
                            let chunk = a3s_box_core::exec::ExecChunk {
                                stream: a3s_box_core::exec::StreamType::Stdout,
                                data: stdout.to_vec(),
                            };
                            writer
                                .write_data(&serde_json::to_vec(&chunk).unwrap())
                                .await
                                .unwrap();
                        }

                        if !stderr.is_empty() {
                            let chunk = a3s_box_core::exec::ExecChunk {
                                stream: a3s_box_core::exec::StreamType::Stderr,
                                data: stderr.to_vec(),
                            };
                            writer
                                .write_data(&serde_json::to_vec(&chunk).unwrap())
                                .await
                                .unwrap();
                        }

                        sleep(exit_delay).await;

                        let exit = a3s_box_core::exec::ExecExit { exit_code };
                        writer
                            .write_control(&serde_json::to_vec(&exit).unwrap())
                            .await
                            .unwrap();
                        break;
                    }
                    Some(frame) => {
                        panic!("unexpected frame type: {:?}", frame.frame_type);
                    }
                }
            }
        });

        Some(TestExecServer {
            _tmp: tmp,
            socket_path,
        })
    }

    async fn spawn_exec_stream_server(
        stdout: &'static [u8],
        stderr: &'static [u8],
        exit_code: i32,
        exit_delay: Duration,
    ) -> Option<TestExecServer> {
        spawn_exec_stream_server_with_assert(stdout, stderr, exit_code, exit_delay, |request| {
            assert_eq!(
                request.rootfs.as_deref(),
                Some("/run/a3s/cri/container-rootfs/sb-1/c-1/rootfs")
            );
        })
        .await
    }

    async fn spawn_multi_exec_stream_server(
        expected: Vec<(&'static str, &'static [u8], i32, Duration)>,
    ) -> Option<TestExecServer> {
        let tmp = tempfile::Builder::new()
            .prefix("a3s-cri-multi-exec-test")
            .tempdir_in("/private/tmp")
            .unwrap();
        let socket_path = tmp.path().join("exec.sock");
        let Some(listener) = bind_test_exec_listener(&socket_path) else {
            return None;
        };

        let expected = Arc::new(tokio::sync::Mutex::new(std::collections::VecDeque::from(
            expected,
        )));
        tokio::spawn(async move {
            loop {
                let (stream, _) = listener.accept().await.unwrap();
                let expected = expected.clone();
                tokio::spawn(async move {
                    let (r, w) = tokio::io::split(stream);
                    let mut reader = a3s_transport::FrameReader::new(r);
                    let mut writer = a3s_transport::FrameWriter::new(w);

                    match reader.read_frame().await.unwrap() {
                        None => {}
                        Some(frame) if frame.frame_type == a3s_transport::FrameType::Heartbeat => {
                            let heartbeat = a3s_transport::Frame::heartbeat();
                            let encoded = heartbeat.encode().unwrap();
                            writer.into_inner().write_all(&encoded).await.unwrap();
                        }
                        Some(frame) if frame.frame_type == a3s_transport::FrameType::Data => {
                            let request: a3s_box_core::exec::ExecRequest =
                                serde_json::from_slice(&frame.payload).unwrap();
                            assert!(request.streaming);
                            let Some((expected_cmd, stdout, exit_code, exit_delay)) =
                                expected.lock().await.pop_front()
                            else {
                                panic!("multi exec stream server received unexpected request");
                            };
                            assert_eq!(request.cmd.first().map(String::as_str), Some(expected_cmd));

                            if !stdout.is_empty() {
                                let chunk = a3s_box_core::exec::ExecChunk {
                                    stream: a3s_box_core::exec::StreamType::Stdout,
                                    data: stdout.to_vec(),
                                };
                                writer
                                    .write_data(&serde_json::to_vec(&chunk).unwrap())
                                    .await
                                    .unwrap();
                            }

                            sleep(exit_delay).await;

                            let exit = a3s_box_core::exec::ExecExit { exit_code };
                            writer
                                .write_control(&serde_json::to_vec(&exit).unwrap())
                                .await
                                .unwrap();
                        }
                        Some(frame) => panic!("unexpected frame type: {:?}", frame.frame_type),
                    }
                });
            }
        });

        Some(TestExecServer {
            _tmp: tmp,
            socket_path,
        })
    }

    async fn spawn_pty_stream_server_with_assert<F>(
        stdout: &'static [u8],
        exit_code: i32,
        exit_delay: Duration,
        assert_request: F,
    ) -> Option<TestPtyServer>
    where
        F: FnOnce(&a3s_box_core::pty::PtyRequest) + Send + 'static,
    {
        let tmp = tempfile::Builder::new()
            .prefix("a3s-cri-pty-test")
            .tempdir_in("/private/tmp")
            .unwrap();
        let exec_socket_path = tmp.path().join("exec.sock");
        let pty_socket_path = tmp.path().join("pty.sock");
        let Some(listener) = bind_test_exec_listener(&pty_socket_path) else {
            return None;
        };

        tokio::spawn(async move {
            let mut assert_request = Some(assert_request);
            let (stream, _) = listener.accept().await.unwrap();
            let (r, w) = tokio::io::split(stream);
            let mut reader = a3s_transport::FrameReader::new(r);
            let mut writer = a3s_transport::FrameWriter::new(w);

            let frame = reader.read_frame().await.unwrap().unwrap();
            assert_eq!(frame.frame_type as u8, a3s_box_core::pty::FRAME_PTY_REQUEST);
            let request: a3s_box_core::pty::PtyRequest =
                serde_json::from_slice(&frame.payload).unwrap();
            let Some(assert_request) = assert_request.take() else {
                panic!("PTY stream server received more than one request");
            };
            assert_request(&request);

            if !stdout.is_empty() {
                writer
                    .write_frame(&a3s_transport::Frame {
                        frame_type: a3s_transport::FrameType::Control,
                        payload: stdout.to_vec(),
                    })
                    .await
                    .unwrap();
            }

            sleep(exit_delay).await;

            let exit = a3s_box_core::pty::PtyExit { exit_code };
            writer
                .write_frame(&a3s_transport::Frame {
                    frame_type: a3s_transport::FrameType::Error,
                    payload: serde_json::to_vec(&exit).unwrap(),
                })
                .await
                .unwrap();
        });

        Some(TestPtyServer {
            _tmp: tmp,
            exec_socket_path,
            pty_socket_path,
        })
    }

    async fn spawn_cancelable_exec_stream_server() -> Option<TestExecServer> {
        let tmp = tempfile::Builder::new()
            .prefix("a3s-cri-exec-cancel-test")
            .tempdir_in("/private/tmp")
            .unwrap();
        let socket_path = tmp.path().join("exec.sock");
        let Some(listener) = bind_test_exec_listener(&socket_path) else {
            return None;
        };

        tokio::spawn(async move {
            loop {
                let (stream, _) = listener.accept().await.unwrap();
                let (r, w) = tokio::io::split(stream);
                let mut reader = a3s_transport::FrameReader::new(r);
                let mut writer = a3s_transport::FrameWriter::new(w);

                match reader.read_frame().await.unwrap() {
                    None => continue,
                    Some(frame) if frame.frame_type == a3s_transport::FrameType::Heartbeat => {
                        let heartbeat = a3s_transport::Frame::heartbeat();
                        let encoded = heartbeat.encode().unwrap();
                        writer.into_inner().write_all(&encoded).await.unwrap();
                    }
                    Some(frame) if frame.frame_type == a3s_transport::FrameType::Data => {
                        let request: a3s_box_core::exec::ExecRequest =
                            serde_json::from_slice(&frame.payload).unwrap();
                        assert!(request.streaming);

                        let cancel = reader.read_frame().await.unwrap().unwrap();
                        assert_eq!(cancel.frame_type, a3s_transport::FrameType::Control);
                        assert_eq!(cancel.payload, b"cancel");

                        let exit = a3s_box_core::exec::ExecExit { exit_code: 137 };
                        writer
                            .write_control(&serde_json::to_vec(&exit).unwrap())
                            .await
                            .unwrap();
                        break;
                    }
                    Some(frame) => {
                        panic!("unexpected frame type: {:?}", frame.frame_type);
                    }
                }
            }
        });

        Some(TestExecServer {
            _tmp: tmp,
            socket_path,
        })
    }

    async fn attach_ready_test_vm(box_id: &str, exec_socket_path: &Path) -> VmManager {
        let mut vm = VmManager::with_box_id(
            a3s_box_core::config::BoxConfig::default(),
            EventEmitter::new(16),
            box_id.to_string(),
        );
        vm.attach_running_process(
            std::process::id(),
            exec_socket_path.to_path_buf(),
            Some(exec_socket_path.with_file_name("pty.sock")),
        )
        .await
        .unwrap();
        vm
    }

    async fn put_test_oci_image(store: &ImageStore, reference: &str) {
        let tmp = tempfile::tempdir().unwrap();
        let blobs = tmp.path().join("blobs").join("sha256");
        std::fs::create_dir_all(&blobs).unwrap();
        std::fs::write(
            tmp.path().join("oci-layout"),
            r#"{"imageLayoutVersion":"1.0.0"}"#,
        )
        .unwrap();

        let config_content = r#"{
            "architecture": "amd64",
            "os": "linux",
            "config": {
                "Entrypoint": ["/usr/local/bin/app"],
                "Cmd": ["serve"],
                "Env": ["PATH=/usr/local/bin:/usr/bin:/bin", "ENV=image"],
                "WorkingDir": "/image",
                "User": "2000:2000"
            },
            "rootfs": {
                "type": "layers",
                "diff_ids": []
            },
            "history": []
        }"#;
        let config_hash = "config456";
        std::fs::write(blobs.join(config_hash), config_content).unwrap();

        let manifest_content = format!(
            r#"{{
                "schemaVersion": 2,
                "mediaType": "application/vnd.oci.image.manifest.v1+json",
                "config": {{
                    "mediaType": "application/vnd.oci.image.config.v1+json",
                    "digest": "sha256:{}",
                    "size": {}
                }},
                "layers": []
            }}"#,
            config_hash,
            config_content.len()
        );
        let manifest_hash = "manifest789";
        std::fs::write(blobs.join(manifest_hash), &manifest_content).unwrap();

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
        std::fs::write(tmp.path().join("index.json"), index_content).unwrap();

        store
            .put(reference, "sha256:imageconfigtest", tmp.path())
            .await
            .unwrap();
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
            network_ip: String::new(),
            additional_ips: vec![],
        }
    }

    fn test_networked_sandbox(id: &str) -> PodSandbox {
        let mut sandbox = test_sandbox(id);
        sandbox
            .annotations
            .insert(ANN_NETWORK.to_string(), "cri-net".to_string());
        sandbox
    }

    fn add_test_network_endpoint(svc: &BoxRuntimeService, sandbox: &mut PodSandbox) {
        if svc.network_store.get("cri-net").unwrap().is_none() {
            svc.network_store
                .create(a3s_box_core::NetworkConfig::new("cri-net", "10.244.0.0/24").unwrap())
                .unwrap();
        }

        let allocation = connect_sandbox_to_network_store(
            &svc.network_store,
            "cri-net",
            &sandbox.id,
            &sandbox.name,
        )
        .unwrap();
        sandbox.network_ip = allocation.ip;
    }

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
            created_at: 1_000_000_000,
            started_at: 0,
            finished_at: 0,
            exit_code: 0,
            labels: HashMap::from([("app".to_string(), "test".to_string())]),
            annotations: HashMap::new(),
            log_path: String::new(),
            rootfs_path: "/".to_string(),
            rootfs_guest_path: format!("/run/a3s/cri/container-rootfs/{sandbox_id}/{id}/rootfs"),
        }
    }

    #[tokio::test]
    async fn test_cri_log_writer_flushes_partial_lines() {
        let dir = tempfile::tempdir().unwrap();
        let log_path = dir.path().join("nested").join("container.log");
        let log_path_string = log_path.to_string_lossy().to_string();
        let mut writer = CriLogWriter::open(&log_path_string).await.unwrap().unwrap();

        writer
            .write_chunk(a3s_box_core::exec::StreamType::Stdout, b"hello ")
            .await
            .unwrap();
        writer
            .write_chunk(a3s_box_core::exec::StreamType::Stdout, b"world")
            .await
            .unwrap();
        writer
            .write_chunk(a3s_box_core::exec::StreamType::Stderr, b"warn\n")
            .await
            .unwrap();
        writer.flush_partials().await.unwrap();

        let log = tokio::fs::read_to_string(&log_path).await.unwrap();
        assert!(log.contains(" stdout F hello world\n"));
        assert!(log.contains(" stderr F warn\n"));
    }

    #[test]
    fn test_runtime_options_resolve_runtime_handler_agent_image() {
        let options = CriRuntimeOptions {
            default_agent_image: "ghcr.io/a3s-box/default:v1".to_string(),
            runtime_handler_agent_images: HashMap::from([(
                "a3s-secure".to_string(),
                "ghcr.io/a3s-box/secure:v1".to_string(),
            )]),
        };

        assert_eq!(
            options.agent_image_for("a3s-secure"),
            "ghcr.io/a3s-box/secure:v1"
        );
        assert_eq!(options.agent_image_for("a3s"), "ghcr.io/a3s-box/default:v1");
    }

    #[tokio::test]
    async fn test_runtime_config_reports_cgroupfs() {
        let svc = make_test_service();
        let resp = svc
            .runtime_config(Request::new(RuntimeConfigRequest {}))
            .await
            .unwrap()
            .into_inner();

        assert_eq!(
            resp.linux.unwrap().cgroup_driver,
            CgroupDriver::Cgroupfs as i32
        );
    }

    #[tokio::test]
    async fn test_container_stats_returns_container_attributes() {
        let svc = make_test_service();
        let mut container = test_container("c-1", "sb-1");
        container.state = ContainerState::Running;
        svc.store.containers.add(container).await;

        let resp = svc
            .container_stats(Request::new(ContainerStatsRequest {
                container_id: "c-1".to_string(),
            }))
            .await
            .unwrap()
            .into_inner();

        let stats = resp.stats.unwrap();
        let attrs = stats.attributes.unwrap();
        assert_eq!(attrs.id, "c-1");
        assert_eq!(attrs.metadata.unwrap().name, "container-c-1");
        assert!(stats.cpu.is_some());
        assert!(stats.memory.is_some());
    }

    #[tokio::test]
    async fn test_container_stats_reports_rootfs_writable_layer_usage() {
        let svc = make_test_service();
        let dir = tempfile::tempdir().unwrap();
        let nested = dir.path().join("nested");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(dir.path().join("file.txt"), b"hello").unwrap();
        std::fs::write(nested.join("child.txt"), b"world!").unwrap();

        let mut container = test_container("c-usage", "sb-1");
        container.state = ContainerState::Running;
        container.rootfs_path = dir.path().to_string_lossy().to_string();
        svc.store.containers.add(container).await;

        let resp = svc
            .container_stats(Request::new(ContainerStatsRequest {
                container_id: "c-usage".to_string(),
            }))
            .await
            .unwrap()
            .into_inner();

        let writable_layer = resp.stats.unwrap().writable_layer.unwrap();
        assert_eq!(
            writable_layer.fs_id.unwrap().mountpoint,
            dir.path().to_string_lossy()
        );
        assert!(writable_layer.used_bytes.unwrap().value >= 11);
        assert!(writable_layer.inodes_used.unwrap().value >= 4);
    }

    #[tokio::test]
    async fn test_list_container_stats_only_reports_running_containers() {
        let svc = make_test_service();
        let mut running = test_container("c-running", "sb-1");
        running.state = ContainerState::Running;
        let mut exited = test_container("c-exited", "sb-1");
        exited.state = ContainerState::Exited;
        svc.store.containers.add(running).await;
        svc.store.containers.add(exited).await;

        let resp = svc
            .list_container_stats(Request::new(ListContainerStatsRequest { filter: None }))
            .await
            .unwrap()
            .into_inner();

        assert_eq!(resp.stats.len(), 1);
        assert_eq!(resp.stats[0].attributes.as_ref().unwrap().id, "c-running");
    }

    fn pod_metric_value(metrics: &PodSandboxMetrics, name: &str) -> f64 {
        metrics
            .metrics
            .iter()
            .find(|metric| metric.name == name)
            .unwrap_or_else(|| panic!("missing pod sandbox metric {name}"))
            .value
    }

    #[tokio::test]
    async fn test_list_metric_descriptors_reports_pod_sandbox_metrics() {
        let svc = make_test_service();
        let resp = svc
            .list_metric_descriptors(Request::new(ListMetricDescriptorsRequest {}))
            .await
            .unwrap()
            .into_inner();

        let names: Vec<_> = resp
            .descriptors
            .iter()
            .map(|descriptor| descriptor.name.as_str())
            .collect();
        assert!(names.contains(&"a3s_box_pod_sandbox_ready"));
        assert!(names.contains(&"a3s_box_pod_sandbox_vm_manager_present"));
        assert!(names.contains(&"a3s_box_pod_sandbox_containers_running"));
    }

    #[tokio::test]
    async fn test_list_pod_sandbox_metrics_returns_lifecycle_snapshot() {
        let svc = make_test_service();
        svc.store.sandboxes.add(test_sandbox("sb-1")).await;

        let mut running = test_container("c-running", "sb-1");
        running.state = ContainerState::Running;
        let mut exited = test_container("c-exited", "sb-1");
        exited.state = ContainerState::Exited;
        let created = test_container("c-created", "sb-1");
        svc.store.containers.add(running).await;
        svc.store.containers.add(exited).await;
        svc.store.containers.add(created).await;
        let vm = VmManager::with_box_id(
            a3s_box_core::config::BoxConfig::default(),
            EventEmitter::new(16),
            "sb-1".to_string(),
        );
        svc.vm_managers.write().await.insert("sb-1".to_string(), vm);

        let resp = svc
            .list_pod_sandbox_metrics(Request::new(ListPodSandboxMetricsRequest {
                filter: Some(PodSandboxStatsFilter {
                    id: "sb-1".to_string(),
                    label_selector: HashMap::new(),
                }),
            }))
            .await
            .unwrap()
            .into_inner();

        assert_eq!(resp.pod_sandbox_metrics.len(), 1);
        let metrics = &resp.pod_sandbox_metrics[0];
        assert_eq!(metrics.pod_sandbox_id, "sb-1");
        assert_eq!(pod_metric_value(metrics, "a3s_box_pod_sandbox_ready"), 1.0);
        assert_eq!(
            pod_metric_value(metrics, "a3s_box_pod_sandbox_vm_manager_present"),
            1.0
        );
        assert_eq!(
            pod_metric_value(metrics, "a3s_box_pod_sandbox_containers_total"),
            3.0
        );
        assert_eq!(
            pod_metric_value(metrics, "a3s_box_pod_sandbox_containers_running"),
            1.0
        );
        assert_eq!(
            pod_metric_value(metrics, "a3s_box_pod_sandbox_containers_exited"),
            1.0
        );
        let first_metric = metrics.metrics.first().unwrap();
        assert_eq!(
            first_metric.labels.get("pod_sandbox_id"),
            Some(&"sb-1".to_string())
        );
        assert_eq!(
            first_metric.labels.get("namespace"),
            Some(&"default".to_string())
        );
    }

    #[tokio::test]
    async fn test_stream_pod_sandbox_metrics_returns_snapshot() {
        let svc = make_test_service();
        svc.store.sandboxes.add(test_sandbox("sb-1")).await;

        let mut stream = svc
            .stream_pod_sandbox_metrics(Request::new(StreamPodSandboxMetricsRequest {
                filter: None,
            }))
            .await
            .unwrap()
            .into_inner();

        let response = stream.next().await.unwrap().unwrap();
        assert_eq!(response.pod_sandbox_metrics.len(), 1);
        assert_eq!(response.pod_sandbox_metrics[0].pod_sandbox_id, "sb-1");
        assert!(stream.next().await.is_none());
    }

    #[tokio::test]
    async fn test_stream_containers_returns_snapshot() {
        let svc = make_test_service();
        svc.store
            .containers
            .add(test_container("c-1", "sb-1"))
            .await;

        let mut stream = svc
            .stream_containers(Request::new(StreamContainersRequest { filter: None }))
            .await
            .unwrap()
            .into_inner();

        let response = stream.next().await.unwrap().unwrap();
        assert_eq!(response.containers.len(), 1);
        assert_eq!(response.containers[0].id, "c-1");
        assert!(stream.next().await.is_none());
    }

    #[tokio::test]
    async fn test_checkpoint_container_is_explicitly_unsupported() {
        let svc = make_test_service();
        let result = svc
            .checkpoint_container(Request::new(CheckpointContainerRequest {
                container_id: "c-1".to_string(),
                location: "/tmp/checkpoints".to_string(),
                timeout: 0,
            }))
            .await;

        assert!(result.is_err());
        assert_eq!(result.unwrap_err().code(), tonic::Code::Unimplemented);
    }

    #[tokio::test]
    async fn test_get_container_events_streams_lifecycle_events() {
        let svc = make_test_service();
        svc.store.sandboxes.add(test_sandbox("sb-1")).await;

        let mut events = svc
            .get_container_events(Request::new(GetEventsRequest {}))
            .await
            .unwrap()
            .into_inner();

        let created = svc
            .create_container(Request::new(CreateContainerRequest {
                pod_sandbox_id: "sb-1".to_string(),
                config: Some(ContainerConfig {
                    metadata: Some(ContainerMetadata {
                        name: "evented".to_string(),
                        attempt: 0,
                    }),
                    command: vec!["/bin/true".to_string()],
                    ..Default::default()
                }),
                sandbox_config: None,
            }))
            .await
            .unwrap()
            .into_inner();

        let event = tokio::time::timeout(Duration::from_secs(1), events.next())
            .await
            .expect("created event should be published")
            .unwrap()
            .unwrap();
        assert_eq!(event.container_id, created.container_id);
        assert_eq!(event.pod_sandbox_id, "sb-1");
        assert_eq!(
            event.container_event_type,
            ContainerEventType::ContainerCreatedEvent as i32
        );
        assert_eq!(event.reason, "ContainerCreated");

        svc.stop_container(Request::new(StopContainerRequest {
            container_id: created.container_id.clone(),
            timeout: 0,
        }))
        .await
        .unwrap();
        let event = tokio::time::timeout(Duration::from_secs(1), events.next())
            .await
            .expect("stopped event should be published")
            .unwrap()
            .unwrap();
        assert_eq!(event.container_id, created.container_id);
        assert_eq!(
            event.container_event_type,
            ContainerEventType::ContainerStoppedEvent as i32
        );
        assert_eq!(event.reason, "StopContainer");

        svc.remove_container(Request::new(RemoveContainerRequest {
            container_id: created.container_id.clone(),
        }))
        .await
        .unwrap();
        let event = tokio::time::timeout(Duration::from_secs(1), events.next())
            .await
            .expect("deleted event should be published")
            .unwrap()
            .unwrap();
        assert_eq!(event.container_id, created.container_id);
        assert_eq!(
            event.container_event_type,
            ContainerEventType::ContainerDeletedEvent as i32
        );
        assert_eq!(event.reason, "ContainerDeleted");
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
        assert!(resp.info.is_empty());
    }

    #[tokio::test]
    async fn test_status_verbose_info() {
        let svc = make_test_service();
        let mut sandbox = test_sandbox("sb-2");
        sandbox.state = SandboxState::NotReady;
        svc.store.sandboxes.add(test_sandbox("sb-1")).await;
        svc.store.sandboxes.add(sandbox).await;

        let mut running = test_container("c-running", "sb-1");
        running.state = ContainerState::Running;
        running.started_at = 2_000_000_000;
        let mut exited = test_container("c-exited", "sb-2");
        exited.state = ContainerState::Exited;
        exited.finished_at = 3_000_000_000;
        exited.exit_code = 42;

        svc.store
            .containers
            .add(test_container("c-created", "sb-1"))
            .await;
        svc.store.containers.add(running).await;
        svc.store.containers.add(exited).await;

        let resp = svc
            .status(Request::new(StatusRequest { verbose: true }))
            .await
            .unwrap()
            .into_inner();

        assert_eq!(resp.info.get("sandbox_count"), Some(&"2".to_string()));
        assert_eq!(resp.info.get("sandbox_ready_count"), Some(&"1".to_string()));
        assert_eq!(
            resp.info.get("sandbox_not_ready_count"),
            Some(&"1".to_string())
        );
        assert_eq!(resp.info.get("container_count"), Some(&"3".to_string()));
        assert_eq!(
            resp.info.get("container_created_count"),
            Some(&"1".to_string())
        );
        assert_eq!(
            resp.info.get("container_running_count"),
            Some(&"1".to_string())
        );
        assert_eq!(
            resp.info.get("container_exited_count"),
            Some(&"1".to_string())
        );
        assert_eq!(resp.info.get("vm_manager_count"), Some(&"0".to_string()));
        assert_eq!(
            resp.info.get("warm_pool_enabled"),
            Some(&"false".to_string())
        );
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
    async fn test_pod_sandbox_status_reports_network_ips() {
        let svc = make_test_service();
        let mut sandbox = test_sandbox("sb-1");
        sandbox.network_ip = "10.244.0.12".to_string();
        sandbox.additional_ips = vec!["fd00::12".to_string()];
        svc.store.sandboxes.add(sandbox).await;

        let resp = svc
            .pod_sandbox_status(Request::new(PodSandboxStatusRequest {
                pod_sandbox_id: "sb-1".to_string(),
                verbose: true,
            }))
            .await
            .unwrap()
            .into_inner();

        let network = resp.status.unwrap().network.unwrap();
        assert_eq!(network.ip, "10.244.0.12");
        assert_eq!(network.additional_ips.len(), 1);
        assert_eq!(network.additional_ips[0].ip, "fd00::12");
        assert_eq!(
            resp.info.get("network_ip"),
            Some(&"10.244.0.12".to_string())
        );
        assert_eq!(resp.info.get("additional_ip_count"), Some(&"1".to_string()));
    }

    #[tokio::test]
    async fn test_pod_sandbox_status_verbose_info() {
        let svc = make_test_service();
        let mut sandbox = test_sandbox("sb-1");
        sandbox.state = SandboxState::NotReady;
        svc.store.sandboxes.add(sandbox).await;
        svc.store
            .containers
            .add(test_container("c-1", "sb-1"))
            .await;

        let resp = svc
            .pod_sandbox_status(Request::new(PodSandboxStatusRequest {
                pod_sandbox_id: "sb-1".to_string(),
                verbose: true,
            }))
            .await
            .unwrap()
            .into_inner();

        assert_eq!(
            resp.info.get("sandbox_state"),
            Some(&"not_ready".to_string())
        );
        assert_eq!(resp.info.get("vm_present"), Some(&"false".to_string()));
        assert_eq!(resp.info.get("container_count"), Some(&"1".to_string()));
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
                    command: vec!["nginx".to_string()],
                    args: vec!["-g".to_string(), "daemon off;".to_string()],
                    working_dir: "/app".to_string(),
                    envs: vec![KeyValue {
                        key: "ENV".to_string(),
                        value: "prod".to_string(),
                    }],
                    stdin: true,
                    stdin_once: true,
                    tty: true,
                    linux: Some(LinuxContainerConfig {
                        security_context: Some(LinuxContainerSecurityContext {
                            run_as_user: Some(Int64Value { value: 1000 }),
                            run_as_group: Some(Int64Value { value: 1001 }),
                            ..Default::default()
                        }),
                        ..Default::default()
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
    async fn test_create_container_requires_ready_sandbox() {
        let svc = make_test_service();
        let mut sandbox = test_sandbox("sb-1");
        sandbox.state = SandboxState::NotReady;
        svc.store.sandboxes.add(sandbox).await;

        let result = svc
            .create_container(Request::new(CreateContainerRequest {
                pod_sandbox_id: "sb-1".to_string(),
                config: Some(ContainerConfig {
                    metadata: Some(ContainerMetadata {
                        name: "test".to_string(),
                        attempt: 0,
                    }),
                    image: Some(ImageSpec {
                        image: "nginx:latest".to_string(),
                        annotations: HashMap::new(),
                    }),
                    command: vec!["nginx".to_string()],
                    ..Default::default()
                }),
                sandbox_config: None,
            }))
            .await;

        assert!(result.is_err());
        let err = result.unwrap_err();
        assert_eq!(err.code(), tonic::Code::FailedPrecondition);
        assert!(err.message().contains("requires a ready sandbox"));
    }

    #[tokio::test]
    async fn test_create_container_allows_multi_container_pod() {
        let svc = make_test_service();
        svc.store.sandboxes.add(test_sandbox("sb-1")).await;
        put_test_oci_image(&svc.image_store, "nginx:latest").await;
        svc.store
            .containers
            .add(test_container("existing", "sb-1"))
            .await;

        let response = svc
            .create_container(Request::new(CreateContainerRequest {
                pod_sandbox_id: "sb-1".to_string(),
                config: Some(ContainerConfig {
                    metadata: Some(ContainerMetadata {
                        name: "second-container".to_string(),
                        attempt: 0,
                    }),
                    image: Some(ImageSpec {
                        image: "nginx:latest".to_string(),
                        annotations: HashMap::new(),
                    }),
                    command: vec!["nginx".to_string()],
                    ..Default::default()
                }),
                sandbox_config: None,
            }))
            .await
            .unwrap()
            .into_inner();

        let containers = svc.store.containers.list(Some("sb-1"), None).await;
        assert_eq!(containers.len(), 2);
        let created = svc
            .store
            .containers
            .get(&response.container_id)
            .await
            .unwrap();
        assert_eq!(created.name, "second-container");
        assert_ne!(created.id, "existing");
        assert!(created.rootfs_guest_path.contains(&created.id));
    }

    #[tokio::test]
    async fn test_create_container_success() {
        let svc = make_test_service();
        svc.store.sandboxes.add(test_sandbox("sb-1")).await;
        put_test_oci_image(&svc.image_store, "nginx:latest").await;

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
                    command: vec!["nginx".to_string()],
                    args: vec!["-g".to_string(), "daemon off;".to_string()],
                    working_dir: "/app".to_string(),
                    envs: vec![KeyValue {
                        key: "ENV".to_string(),
                        value: "prod".to_string(),
                    }],
                    stdin: true,
                    stdin_once: true,
                    tty: true,
                    linux: Some(LinuxContainerConfig {
                        security_context: Some(LinuxContainerSecurityContext {
                            run_as_user: Some(Int64Value { value: 1000 }),
                            run_as_group: Some(Int64Value { value: 1001 }),
                            ..Default::default()
                        }),
                        ..Default::default()
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
        assert_eq!(c.resolved_image_digest, "sha256:imageconfigtest");
        assert!(!c.resolved_image_path.is_empty());
        assert!(!c.rootfs_path.is_empty());
        assert!(PathBuf::from(&c.rootfs_path).is_dir());
        assert!(c
            .rootfs_guest_path
            .starts_with(CRI_CONTAINER_ROOTFS_GUEST_BASE));
        assert!(PathBuf::from(&c.rootfs_path).join("tmp").is_dir());
        assert_eq!(c.command, vec!["nginx".to_string()]);
        assert_eq!(c.args, vec!["-g".to_string(), "daemon off;".to_string()]);
        assert_eq!(
            c.env,
            vec![
                (
                    "PATH".to_string(),
                    "/usr/local/bin:/usr/bin:/bin".to_string()
                ),
                ("ENV".to_string(), "prod".to_string()),
            ]
        );
        assert_eq!(c.working_dir, "/app");
        assert_eq!(c.user, Some("1000:1001".to_string()));
        assert!(c.stdin);
        assert!(c.stdin_once);
        assert!(c.tty);
    }

    #[tokio::test]
    async fn test_create_container_materializes_readonly_mount() {
        let svc = make_test_service();
        svc.store.sandboxes.add(test_sandbox("sb-1")).await;
        put_test_oci_image(&svc.image_store, "nginx:latest").await;

        let source = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(source.path().join("nested")).unwrap();
        std::fs::write(source.path().join("config.txt"), b"mounted config").unwrap();
        std::fs::write(source.path().join("nested").join("extra.txt"), b"extra").unwrap();

        let resp = svc
            .create_container(Request::new(CreateContainerRequest {
                pod_sandbox_id: "sb-1".to_string(),
                config: Some(ContainerConfig {
                    metadata: Some(ContainerMetadata {
                        name: "mounted".to_string(),
                        attempt: 0,
                    }),
                    image: Some(ImageSpec {
                        image: "nginx:latest".to_string(),
                        annotations: HashMap::new(),
                    }),
                    command: vec!["nginx".to_string()],
                    mounts: vec![Mount {
                        container_path: "/etc/a3s-config".to_string(),
                        host_path: source.path().to_string_lossy().to_string(),
                        readonly: true,
                        selinux_relabel: false,
                        propagation: crate::cri_api::mount::MountPropagation::PropagationPrivate
                            as i32,
                    }],
                    ..Default::default()
                }),
                sandbox_config: None,
            }))
            .await
            .unwrap()
            .into_inner();

        let container = svc.store.containers.get(&resp.container_id).await.unwrap();
        assert_eq!(container.mounts.len(), 1);
        let mounted_file = PathBuf::from(&container.rootfs_path)
            .join("etc")
            .join("a3s-config")
            .join("config.txt");
        let mounted_nested = PathBuf::from(&container.rootfs_path)
            .join("etc")
            .join("a3s-config")
            .join("nested")
            .join("extra.txt");
        assert_eq!(
            std::fs::read_to_string(mounted_file).unwrap(),
            "mounted config"
        );
        assert_eq!(std::fs::read_to_string(mounted_nested).unwrap(), "extra");

        let status = svc
            .container_status(Request::new(ContainerStatusRequest {
                container_id: resp.container_id,
                verbose: true,
            }))
            .await
            .unwrap()
            .into_inner();
        let status = status.status.unwrap();
        assert_eq!(status.mounts.len(), 1);
        assert_eq!(status.mounts[0].container_path, "/etc/a3s-config");
    }

    #[tokio::test]
    async fn test_create_container_rejects_writable_mounts() {
        let svc = make_test_service();
        svc.store.sandboxes.add(test_sandbox("sb-1")).await;

        let result = svc
            .create_container(Request::new(CreateContainerRequest {
                pod_sandbox_id: "sb-1".to_string(),
                config: Some(ContainerConfig {
                    metadata: Some(ContainerMetadata {
                        name: "mounted".to_string(),
                        attempt: 0,
                    }),
                    command: vec!["/bin/true".to_string()],
                    mounts: vec![Mount {
                        container_path: "/data".to_string(),
                        host_path: "/tmp".to_string(),
                        readonly: false,
                        selinux_relabel: false,
                        propagation: crate::cri_api::mount::MountPropagation::PropagationPrivate
                            as i32,
                    }],
                    ..Default::default()
                }),
                sandbox_config: None,
            }))
            .await;

        assert!(result.is_err());
        let err = result.unwrap_err();
        assert_eq!(err.code(), tonic::Code::Unimplemented);
        assert!(err.message().contains("Writable CRI mounts"));
    }

    #[tokio::test]
    async fn test_create_container_requires_pulled_image() {
        let svc = make_test_service();
        svc.store.sandboxes.add(test_sandbox("sb-1")).await;

        let result = svc
            .create_container(Request::new(CreateContainerRequest {
                pod_sandbox_id: "sb-1".to_string(),
                config: Some(ContainerConfig {
                    metadata: Some(ContainerMetadata {
                        name: "missing-image".to_string(),
                        attempt: 0,
                    }),
                    image: Some(ImageSpec {
                        image: "example.com/missing:latest".to_string(),
                        annotations: HashMap::new(),
                    }),
                    command: vec!["/bin/true".to_string()],
                    ..Default::default()
                }),
                sandbox_config: None,
            }))
            .await;

        assert!(result.is_err());
        let err = result.unwrap_err();
        assert_eq!(err.code(), tonic::Code::NotFound);
        assert!(err.message().contains("Image not found locally"));
        assert!(svc
            .store
            .containers
            .list(Some("sb-1"), None)
            .await
            .is_empty());
    }

    #[tokio::test]
    async fn test_create_container_uses_image_defaults() {
        let svc = make_test_service();
        svc.store.sandboxes.add(test_sandbox("sb-1")).await;
        put_test_oci_image(&svc.image_store, "example.com/app:latest").await;

        let resp = svc
            .create_container(Request::new(CreateContainerRequest {
                pod_sandbox_id: "sb-1".to_string(),
                config: Some(ContainerConfig {
                    metadata: Some(ContainerMetadata {
                        name: "my-container".to_string(),
                        attempt: 0,
                    }),
                    image: Some(ImageSpec {
                        image: "example.com/app:latest".to_string(),
                        annotations: HashMap::new(),
                    }),
                    envs: vec![
                        KeyValue {
                            key: "ENV".to_string(),
                            value: "cri".to_string(),
                        },
                        KeyValue {
                            key: "EXTRA".to_string(),
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
        assert_eq!(c.resolved_image_digest, "sha256:imageconfigtest");
        assert!(!c.resolved_image_path.is_empty());
        assert!(!c.rootfs_path.is_empty());
        assert!(PathBuf::from(&c.rootfs_path).is_dir());
        assert!(c
            .rootfs_guest_path
            .starts_with(CRI_CONTAINER_ROOTFS_GUEST_BASE));
        assert_eq!(c.command, vec!["/usr/local/bin/app".to_string()]);
        assert_eq!(c.args, vec!["serve".to_string()]);
        assert_eq!(
            c.env,
            vec![
                (
                    "PATH".to_string(),
                    "/usr/local/bin:/usr/bin:/bin".to_string()
                ),
                ("ENV".to_string(), "cri".to_string()),
                ("EXTRA".to_string(), "1".to_string()),
            ]
        );
        assert_eq!(c.working_dir, "/image");
        assert_eq!(c.user, Some("2000:2000".to_string()));
    }

    #[tokio::test]
    async fn test_create_then_start_container_uses_image_defaults_and_rootfs() {
        let svc = make_test_service();
        svc.store.sandboxes.add(test_sandbox("sb-1")).await;
        put_test_oci_image(&svc.image_store, "example.com/app:latest").await;

        let resp = svc
            .create_container(Request::new(CreateContainerRequest {
                pod_sandbox_id: "sb-1".to_string(),
                config: Some(ContainerConfig {
                    metadata: Some(ContainerMetadata {
                        name: "image-defaults".to_string(),
                        attempt: 0,
                    }),
                    image: Some(ImageSpec {
                        image: "example.com/app:latest".to_string(),
                        annotations: HashMap::new(),
                    }),
                    ..Default::default()
                }),
                sandbox_config: None,
            }))
            .await
            .unwrap()
            .into_inner();

        let container = svc.store.containers.get(&resp.container_id).await.unwrap();
        assert_eq!(container.command, vec!["/usr/local/bin/app".to_string()]);
        assert_eq!(container.args, vec!["serve".to_string()]);
        assert!(PathBuf::from(&container.rootfs_path).is_dir());
        assert!(container
            .rootfs_guest_path
            .starts_with(CRI_CONTAINER_ROOTFS_GUEST_BASE));

        let expected_cmd = vec!["/usr/local/bin/app".to_string(), "serve".to_string()];
        let expected_env = vec![
            "PATH=/usr/local/bin:/usr/bin:/bin".to_string(),
            "ENV=image".to_string(),
        ];
        let expected_rootfs = container.rootfs_guest_path.clone();
        let Some(exec_server) = spawn_exec_stream_server_with_assert(
            b"ready\n",
            b"",
            0,
            Duration::from_millis(100),
            move |request| {
                assert_eq!(request.cmd.as_slice(), expected_cmd.as_slice());
                assert_eq!(request.env.as_slice(), expected_env.as_slice());
                assert_eq!(request.working_dir.as_deref(), Some("/image"));
                assert_eq!(request.user.as_deref(), Some("2000:2000"));
                assert_eq!(request.rootfs.as_deref(), Some(expected_rootfs.as_str()));
            },
        )
        .await
        else {
            return;
        };

        let vm = attach_ready_test_vm("sb-1", &exec_server.socket_path).await;
        svc.vm_managers.write().await.insert("sb-1".to_string(), vm);

        svc.start_container(Request::new(StartContainerRequest {
            container_id: resp.container_id.clone(),
        }))
        .await
        .unwrap();

        let running = svc.store.containers.get(&resp.container_id).await.unwrap();
        assert_eq!(running.state, ContainerState::Running);
        assert!(running.started_at > 0);

        let exited = tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                let container = svc.store.containers.get(&resp.container_id).await.unwrap();
                if container.state == ContainerState::Exited {
                    break container;
                }
                sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("container should exit under background supervision");

        assert_eq!(exited.exit_code, 0);
        assert!(exited.finished_at >= exited.started_at);
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
    async fn test_start_container_supports_tty_workload() {
        let svc = make_test_service();
        svc.store.sandboxes.add(test_sandbox("sb-1")).await;
        let mut container = test_container("c-1", "sb-1");
        container.command = vec!["/bin/sh".to_string()];
        container.args = vec![];
        container.stdin = true;
        container.tty = true;
        svc.store.containers.add(container).await;

        let Some(pty_server) = spawn_pty_stream_server_with_assert(
            b"tty ready\n",
            0,
            Duration::from_secs(1),
            |request| {
                assert_eq!(request.cmd, vec!["/bin/sh".to_string()]);
                assert_eq!(
                    request.rootfs.as_deref(),
                    Some("/run/a3s/cri/container-rootfs/sb-1/c-1/rootfs")
                );
            },
        )
        .await
        else {
            return;
        };
        let vm = attach_ready_test_vm("sb-1", &pty_server.exec_socket_path).await;
        svc.vm_managers.write().await.insert("sb-1".to_string(), vm);

        svc.start_container(Request::new(StartContainerRequest {
            container_id: "c-1".to_string(),
        }))
        .await
        .unwrap();

        let c = svc.store.containers.get("c-1").await.unwrap();
        assert_eq!(c.state, ContainerState::Running);
        assert!(svc.attach_streams.read().await.contains_key("c-1"));
        assert!(svc.workload_stdins.read().await.contains_key("c-1"));
        assert_eq!(
            pty_server.pty_socket_path,
            pty_server.exec_socket_path.with_file_name("pty.sock")
        );

        let attach = svc
            .attach(Request::new(AttachRequest {
                container_id: "c-1".to_string(),
                stdin: false,
                tty: true,
                stdout: true,
                stderr: false,
            }))
            .await
            .unwrap()
            .into_inner();
        assert!(attach.url.contains("/attach/"));
    }

    #[tokio::test]
    async fn test_start_container_registers_non_tty_stdin_handle() {
        let svc = make_test_service();
        svc.store.sandboxes.add(test_sandbox("sb-1")).await;
        let mut container = test_container("c-1", "sb-1");
        container.command = vec!["cat".to_string()];
        container.args = vec![];
        container.stdin = true;
        container.stdin_once = true;
        svc.store.containers.add(container).await;

        let Some(exec_server) =
            spawn_exec_stream_server_with_assert(b"", b"", 0, Duration::from_secs(1), |request| {
                assert!(request.stdin_streaming);
            })
            .await
        else {
            return;
        };
        let vm = attach_ready_test_vm("sb-1", &exec_server.socket_path).await;
        svc.vm_managers.write().await.insert("sb-1".to_string(), vm);

        svc.start_container(Request::new(StartContainerRequest {
            container_id: "c-1".to_string(),
        }))
        .await
        .unwrap();

        let running = svc.store.containers.get("c-1").await.unwrap();
        assert_eq!(running.state, ContainerState::Running);
        assert!(svc.attach_streams.read().await.contains_key("c-1"));
        assert!(svc.workload_stdins.read().await.contains_key("c-1"));
        assert!(svc.workload_stops.read().await.contains_key("c-1"));
    }

    #[tokio::test]
    async fn test_start_container_requires_resolved_image_metadata() {
        let svc = make_test_service();
        let mut container = test_container("c-1", "sb-1");
        container.resolved_image_digest.clear();
        container.resolved_image_path.clear();
        svc.store.containers.add(container).await;

        let result = svc
            .start_container(Request::new(StartContainerRequest {
                container_id: "c-1".to_string(),
            }))
            .await;

        assert!(result.is_err());
        let err = result.unwrap_err();
        assert_eq!(err.code(), tonic::Code::FailedPrecondition);
        assert!(err.message().contains("without resolved image metadata"));

        let c = svc.store.containers.get("c-1").await.unwrap();
        assert_eq!(c.state, ContainerState::Created);
        assert_eq!(c.started_at, 0);
    }

    #[tokio::test]
    async fn test_start_container_requires_resolved_image_path() {
        let svc = make_test_service();
        let dir = tempfile::tempdir().unwrap();
        let missing_path = dir.path().join("missing-image");
        let mut container = test_container("c-1", "sb-1");
        container.resolved_image_path = missing_path.to_string_lossy().to_string();
        svc.store.containers.add(container).await;

        let result = svc
            .start_container(Request::new(StartContainerRequest {
                container_id: "c-1".to_string(),
            }))
            .await;

        assert!(result.is_err());
        let err = result.unwrap_err();
        assert_eq!(err.code(), tonic::Code::FailedPrecondition);
        assert!(err.message().contains("Resolved image path"));

        let c = svc.store.containers.get("c-1").await.unwrap();
        assert_eq!(c.state, ContainerState::Created);
        assert_eq!(c.started_at, 0);
    }

    #[tokio::test]
    async fn test_start_container_requires_prepared_rootfs_metadata() {
        let svc = make_test_service();
        let mut container = test_container("c-1", "sb-1");
        container.rootfs_path.clear();
        container.rootfs_guest_path.clear();
        svc.store.containers.add(container).await;

        let result = svc
            .start_container(Request::new(StartContainerRequest {
                container_id: "c-1".to_string(),
            }))
            .await;

        assert!(result.is_err());
        let err = result.unwrap_err();
        assert_eq!(err.code(), tonic::Code::FailedPrecondition);
        assert!(err.message().contains("without prepared rootfs metadata"));

        let c = svc.store.containers.get("c-1").await.unwrap();
        assert_eq!(c.state, ContainerState::Created);
        assert_eq!(c.started_at, 0);
    }

    #[tokio::test]
    async fn test_start_container_requires_prepared_rootfs_path() {
        let svc = make_test_service();
        let dir = tempfile::tempdir().unwrap();
        let missing_path = dir.path().join("missing-rootfs");
        let mut container = test_container("c-1", "sb-1");
        container.rootfs_path = missing_path.to_string_lossy().to_string();
        svc.store.containers.add(container).await;

        let result = svc
            .start_container(Request::new(StartContainerRequest {
                container_id: "c-1".to_string(),
            }))
            .await;

        assert!(result.is_err());
        let err = result.unwrap_err();
        assert_eq!(err.code(), tonic::Code::FailedPrecondition);
        assert!(err.message().contains("Prepared rootfs"));

        let c = svc.store.containers.get("c-1").await.unwrap();
        assert_eq!(c.state, ContainerState::Created);
        assert_eq!(c.started_at, 0);
    }

    #[tokio::test]
    async fn test_start_container_rejects_already_running() {
        let svc = make_test_service();
        let mut container = test_container("c-1", "sb-1");
        container.state = ContainerState::Running;
        container.started_at = 2_000_000_000;
        svc.store.containers.add(container).await;

        let result = svc
            .start_container(Request::new(StartContainerRequest {
                container_id: "c-1".to_string(),
            }))
            .await;

        assert!(result.is_err());
        let err = result.unwrap_err();
        assert_eq!(err.code(), tonic::Code::FailedPrecondition);
        assert!(err.message().contains("already running"));
    }

    #[tokio::test]
    async fn test_start_container_rejects_exited_container() {
        let svc = make_test_service();
        let mut container = test_container("c-1", "sb-1");
        container.state = ContainerState::Exited;
        container.finished_at = 3_000_000_000;
        svc.store.containers.add(container).await;

        let result = svc
            .start_container(Request::new(StartContainerRequest {
                container_id: "c-1".to_string(),
            }))
            .await;

        assert!(result.is_err());
        let err = result.unwrap_err();
        assert_eq!(err.code(), tonic::Code::FailedPrecondition);
        assert!(err.message().contains("already exited"));
    }

    #[tokio::test]
    async fn test_start_container_requires_running_sandbox_vm() {
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
        let err = result.unwrap_err();
        assert_eq!(err.code(), tonic::Code::FailedPrecondition);
        assert!(err.message().contains("VM not found"));

        let c = svc.store.containers.get("c-1").await.unwrap();
        assert_eq!(c.state, ContainerState::Created);
        assert_eq!(c.started_at, 0);
    }

    #[tokio::test]
    async fn test_start_container_requires_ready_sandbox_vm() {
        let svc = make_test_service();
        svc.store
            .containers
            .add(test_container("c-1", "sb-1"))
            .await;

        let vm = VmManager::with_box_id(
            a3s_box_core::config::BoxConfig::default(),
            EventEmitter::new(16),
            "sb-1".to_string(),
        );
        svc.vm_managers.write().await.insert("sb-1".to_string(), vm);

        let result = svc
            .start_container(Request::new(StartContainerRequest {
                container_id: "c-1".to_string(),
            }))
            .await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert_eq!(err.code(), tonic::Code::FailedPrecondition);
        assert!(err.message().contains("VM is not ready"));

        let c = svc.store.containers.get("c-1").await.unwrap();
        assert_eq!(c.state, ContainerState::Created);
        assert_eq!(c.started_at, 0);
    }

    #[tokio::test]
    async fn test_start_container_transitions_running_then_exited() {
        let svc = make_test_service();
        svc.store.sandboxes.add(test_sandbox("sb-1")).await;
        let log_dir = tempfile::tempdir().unwrap();
        let log_path = log_dir.path().join("container.log");
        let mut container = test_container("c-1", "sb-1");
        container.command = vec!["/bin/test-app".to_string()];
        container.args = vec!["serve".to_string()];
        container.log_path = log_path.to_string_lossy().to_string();
        svc.store.containers.add(container).await;

        let Some(exec_server) =
            spawn_exec_stream_server(b"booted\n", b"warn\n", 17, Duration::from_millis(100)).await
        else {
            return;
        };
        let vm = attach_ready_test_vm("sb-1", &exec_server.socket_path).await;
        svc.vm_managers.write().await.insert("sb-1".to_string(), vm);

        svc.start_container(Request::new(StartContainerRequest {
            container_id: "c-1".to_string(),
        }))
        .await
        .unwrap();

        let running = svc.store.containers.get("c-1").await.unwrap();
        assert_eq!(running.state, ContainerState::Running);
        assert!(running.started_at > 0);
        assert_eq!(running.finished_at, 0);
        assert!(svc.attach_streams.read().await.contains_key("c-1"));

        let exited = tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                let container = svc.store.containers.get("c-1").await.unwrap();
                if container.state == ContainerState::Exited {
                    break container;
                }
                sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("container should exit under background supervision");

        assert_eq!(exited.exit_code, 17);
        assert!(exited.finished_at >= exited.started_at);
        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if !svc.attach_streams.read().await.contains_key("c-1") {
                    break;
                }
                sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("attach stream should be removed after workload exit");

        let log = tokio::fs::read_to_string(&log_path).await.unwrap();
        assert!(log.contains(" stdout F booted\n"));
        assert!(log.contains(" stderr F warn\n"));
    }

    #[tokio::test]
    async fn test_start_container_supervises_multiple_containers_in_same_sandbox() {
        let svc = make_test_service();
        svc.store.sandboxes.add(test_sandbox("sb-1")).await;
        let log_dir = tempfile::tempdir().unwrap();

        let mut first = test_container("c-1", "sb-1");
        first.command = vec!["app-one".to_string()];
        first.args = vec![];
        first.log_path = log_dir
            .path()
            .join("first.log")
            .to_string_lossy()
            .to_string();
        svc.store.containers.add(first).await;

        let mut second = test_container("c-2", "sb-1");
        second.command = vec!["app-two".to_string()];
        second.args = vec![];
        second.log_path = log_dir
            .path()
            .join("second.log")
            .to_string_lossy()
            .to_string();
        svc.store.containers.add(second).await;

        let Some(exec_server) = spawn_multi_exec_stream_server(vec![
            ("app-one", b"one\n", 0, Duration::from_millis(100)),
            ("app-two", b"two\n", 2, Duration::from_millis(50)),
        ])
        .await
        else {
            return;
        };
        let vm = attach_ready_test_vm("sb-1", &exec_server.socket_path).await;
        svc.vm_managers.write().await.insert("sb-1".to_string(), vm);

        svc.start_container(Request::new(StartContainerRequest {
            container_id: "c-1".to_string(),
        }))
        .await
        .unwrap();
        svc.start_container(Request::new(StartContainerRequest {
            container_id: "c-2".to_string(),
        }))
        .await
        .unwrap();

        assert!(svc.attach_streams.read().await.contains_key("c-1"));
        assert!(svc.attach_streams.read().await.contains_key("c-2"));

        let (first, second) = tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                let first = svc.store.containers.get("c-1").await.unwrap();
                let second = svc.store.containers.get("c-2").await.unwrap();
                if first.state == ContainerState::Exited && second.state == ContainerState::Exited {
                    break (first, second);
                }
                sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("both containers should exit under independent supervision");

        assert_eq!(first.exit_code, 0);
        assert_eq!(second.exit_code, 2);
        assert!(first.finished_at >= first.started_at);
        assert!(second.finished_at >= second.started_at);

        let first_log = tokio::fs::read_to_string(&first.log_path).await.unwrap();
        let second_log = tokio::fs::read_to_string(&second.log_path).await.unwrap();
        assert!(first_log.contains(" stdout F one\n"));
        assert!(second_log.contains(" stdout F two\n"));
    }

    #[tokio::test]
    async fn test_stop_container() {
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
        let vm = VmManager::with_box_id(
            a3s_box_core::config::BoxConfig::default(),
            EventEmitter::new(16),
            "sb-1".to_string(),
        );
        svc.vm_managers.write().await.insert("sb-1".to_string(), vm);

        svc.stop_container(Request::new(StopContainerRequest {
            container_id: "c-1".to_string(),
            timeout: 0,
        }))
        .await
        .unwrap();

        let c = svc.store.containers.get("c-1").await.unwrap();
        assert_eq!(c.state, ContainerState::Exited);
        assert!(c.finished_at > 0);
        assert_eq!(c.exit_code, 137);
        assert!(!svc.vm_managers.read().await.contains_key("sb-1"));

        let sandbox = svc.store.sandboxes.get("sb-1").await.unwrap();
        assert_eq!(sandbox.state, SandboxState::NotReady);
    }

    #[tokio::test]
    async fn test_stop_container_stops_workload_without_tearing_down_sandbox_vm() {
        let svc = make_test_service();
        svc.store.sandboxes.add(test_sandbox("sb-1")).await;
        let mut container = test_container("c-1", "sb-1");
        container.command = vec!["sleep".to_string(), "60".to_string()];
        svc.store.containers.add(container).await;

        let Some(exec_server) = spawn_cancelable_exec_stream_server().await else {
            return;
        };
        let vm = attach_ready_test_vm("sb-1", &exec_server.socket_path).await;
        svc.vm_managers.write().await.insert("sb-1".to_string(), vm);

        svc.start_container(Request::new(StartContainerRequest {
            container_id: "c-1".to_string(),
        }))
        .await
        .unwrap();
        assert!(svc.workload_stops.read().await.contains_key("c-1"));

        svc.stop_container(Request::new(StopContainerRequest {
            container_id: "c-1".to_string(),
            timeout: 1,
        }))
        .await
        .unwrap();

        let c = svc.store.containers.get("c-1").await.unwrap();
        assert_eq!(c.state, ContainerState::Exited);
        assert_eq!(c.exit_code, 137);
        assert!(c.finished_at > 0);
        assert!(svc.vm_managers.read().await.contains_key("sb-1"));
        assert!(!svc.workload_stops.read().await.contains_key("c-1"));
        assert!(!svc.attach_streams.read().await.contains_key("c-1"));

        let sandbox = svc.store.sandboxes.get("sb-1").await.unwrap();
        assert_eq!(sandbox.state, SandboxState::Ready);
    }

    #[tokio::test]
    async fn test_stop_container_refuses_vm_teardown_with_other_running_containers() {
        let svc = make_test_service();
        svc.store.sandboxes.add(test_sandbox("sb-1")).await;
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
            .mark_started("c-1", 2_000_000_000)
            .await;
        svc.store
            .containers
            .mark_started("c-2", 2_000_000_001)
            .await;

        let result = svc
            .stop_container(Request::new(StopContainerRequest {
                container_id: "c-1".to_string(),
                timeout: 0,
            }))
            .await;

        assert!(result.is_err());
        let err = result.unwrap_err();
        assert_eq!(err.code(), tonic::Code::FailedPrecondition);
        assert!(err.message().contains("other containers"));
        let first = svc.store.containers.get("c-1").await.unwrap();
        let second = svc.store.containers.get("c-2").await.unwrap();
        assert_eq!(first.state, ContainerState::Running);
        assert_eq!(second.state, ContainerState::Running);
        let sandbox = svc.store.sandboxes.get("sb-1").await.unwrap();
        assert_eq!(sandbox.state, SandboxState::Ready);
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
    async fn test_stop_container_preserves_exited_state() {
        let svc = make_test_service();
        let mut container = test_container("c-1", "sb-1");
        container.state = ContainerState::Exited;
        container.finished_at = 3_000_000_000;
        container.exit_code = 42;
        svc.store.containers.add(container).await;

        svc.stop_container(Request::new(StopContainerRequest {
            container_id: "c-1".to_string(),
            timeout: 0,
        }))
        .await
        .unwrap();

        let c = svc.store.containers.get("c-1").await.unwrap();
        assert_eq!(c.state, ContainerState::Exited);
        assert_eq!(c.finished_at, 3_000_000_000);
        assert_eq!(c.exit_code, 42);
    }

    #[tokio::test]
    async fn test_stop_container_created_does_not_stop_sandbox() {
        let svc = make_test_service();
        svc.store.sandboxes.add(test_sandbox("sb-1")).await;
        svc.store
            .containers
            .add(test_container("c-1", "sb-1"))
            .await;
        let vm = VmManager::with_box_id(
            a3s_box_core::config::BoxConfig::default(),
            EventEmitter::new(16),
            "sb-1".to_string(),
        );
        svc.vm_managers.write().await.insert("sb-1".to_string(), vm);

        svc.stop_container(Request::new(StopContainerRequest {
            container_id: "c-1".to_string(),
            timeout: 0,
        }))
        .await
        .unwrap();

        let c = svc.store.containers.get("c-1").await.unwrap();
        assert_eq!(c.state, ContainerState::Exited);
        assert_eq!(c.exit_code, 0);

        let sandbox = svc.store.sandboxes.get("sb-1").await.unwrap();
        assert_eq!(sandbox.state, SandboxState::Ready);
        assert!(svc.vm_managers.read().await.contains_key("sb-1"));
    }

    #[tokio::test]
    async fn test_stop_container_running_without_vm_reconciles_state() {
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

        svc.stop_container(Request::new(StopContainerRequest {
            container_id: "c-1".to_string(),
            timeout: 0,
        }))
        .await
        .unwrap();

        let c = svc.store.containers.get("c-1").await.unwrap();
        assert_eq!(c.state, ContainerState::Exited);
        assert_eq!(c.exit_code, 137);

        let sandbox = svc.store.sandboxes.get("sb-1").await.unwrap();
        assert_eq!(sandbox.state, SandboxState::NotReady);
    }

    #[tokio::test]
    async fn test_stop_container_running_disconnects_network_endpoint() {
        let svc = make_test_service();
        let mut sandbox = test_networked_sandbox("sb-1");
        add_test_network_endpoint(&svc, &mut sandbox);
        svc.store.sandboxes.add(sandbox).await;
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

        let network = svc.network_store.get("cri-net").unwrap().unwrap();
        assert!(network.endpoints.is_empty());

        let sandbox = svc.store.sandboxes.get("sb-1").await.unwrap();
        assert_eq!(sandbox.state, SandboxState::NotReady);
    }

    #[tokio::test]
    async fn test_remove_container() {
        let svc = make_test_service();
        let rootfs_path = svc
            .container_rootfs_base()
            .join("sb-1")
            .join("c-1")
            .join("rootfs");
        std::fs::create_dir_all(&rootfs_path).unwrap();
        let mut container = test_container("c-1", "sb-1");
        container.rootfs_path = rootfs_path.to_string_lossy().to_string();
        svc.store.containers.add(container).await;

        svc.remove_container(Request::new(RemoveContainerRequest {
            container_id: "c-1".to_string(),
        }))
        .await
        .unwrap();

        assert!(svc.store.containers.get("c-1").await.is_none());
        assert!(!rootfs_path.exists());
    }

    #[tokio::test]
    async fn test_remove_container_missing_is_idempotent() {
        let svc = make_test_service();

        let result = svc
            .remove_container(Request::new(RemoveContainerRequest {
                container_id: "missing".to_string(),
            }))
            .await;

        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_remove_container_rejects_running_container() {
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
        let err = result.unwrap_err();
        assert_eq!(err.code(), tonic::Code::FailedPrecondition);
        assert!(err.message().contains("requires a stopped container"));
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
        assert_eq!(status.image_ref, "sha256:test");
        assert!(resp.info.is_empty());
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

    #[tokio::test]
    async fn test_container_status_exited_success_reason() {
        let svc = make_test_service();
        svc.store
            .containers
            .add(test_container("c-1", "sb-1"))
            .await;
        svc.store
            .containers
            .mark_exited("c-1", 3_000_000_000, 0)
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
            crate::cri_api::ContainerState::ContainerExited
        );
        assert_eq!(status.reason, "Completed");
        assert_eq!(status.message, "Container exited successfully");
        assert_eq!(status.exit_code, 0);
    }

    #[tokio::test]
    async fn test_container_status_exited_error_reason() {
        let svc = make_test_service();
        svc.store
            .containers
            .add(test_container("c-1", "sb-1"))
            .await;
        svc.store
            .containers
            .mark_exited("c-1", 3_000_000_000, 42)
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
            crate::cri_api::ContainerState::ContainerExited
        );
        assert_eq!(status.reason, "Error");
        assert_eq!(status.message, "Container exited with code 42");
        assert_eq!(status.exit_code, 42);
    }

    #[tokio::test]
    async fn test_container_status_verbose_info() {
        let svc = make_test_service();
        let mut container = test_container("c-1", "sb-1");
        container.tty = true;
        container.stdin = true;
        container.stdin_once = true;
        svc.store.containers.add(container).await;

        let resp = svc
            .container_status(Request::new(ContainerStatusRequest {
                container_id: "c-1".to_string(),
                verbose: true,
            }))
            .await
            .unwrap()
            .into_inner();

        assert_eq!(
            resp.info.get("container_state"),
            Some(&"created".to_string())
        );
        assert_eq!(resp.info.get("sandbox_id"), Some(&"sb-1".to_string()));
        assert_eq!(
            resp.info.get("image_ref"),
            Some(&"nginx:latest".to_string())
        );
        assert_eq!(
            resp.info.get("resolved_image_digest"),
            Some(&"sha256:test".to_string())
        );
        assert_eq!(resp.info.get("resolved_image_path"), Some(&"/".to_string()));
        assert_eq!(resp.info.get("rootfs_path"), Some(&"/".to_string()));
        assert_eq!(
            resp.info.get("rootfs_guest_path"),
            Some(&"/run/a3s/cri/container-rootfs/sb-1/c-1/rootfs".to_string())
        );
        assert_eq!(resp.info.get("vm_present"), Some(&"false".to_string()));
        assert_eq!(resp.info.get("command_count"), Some(&"1".to_string()));
        assert_eq!(resp.info.get("arg_count"), Some(&"2".to_string()));
        assert_eq!(resp.info.get("env_count"), Some(&"1".to_string()));
        assert_eq!(resp.info.get("tty"), Some(&"true".to_string()));
        assert_eq!(resp.info.get("stdin"), Some(&"true".to_string()));
        assert_eq!(resp.info.get("stdin_once"), Some(&"true".to_string()));
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

    #[tokio::test]
    async fn test_list_containers_filter_by_state() {
        let svc = make_test_service();
        let mut running = test_container("c-running", "sb-1");
        running.state = ContainerState::Running;
        running.started_at = 2_000_000_000;

        let mut exited = test_container("c-exited", "sb-1");
        exited.state = ContainerState::Exited;
        exited.finished_at = 3_000_000_000;
        exited.exit_code = 7;

        svc.store
            .containers
            .add(test_container("c-created", "sb-1"))
            .await;
        svc.store.containers.add(running).await;
        svc.store.containers.add(exited).await;

        let resp = svc
            .list_containers(Request::new(ListContainersRequest {
                filter: Some(ContainerFilter {
                    id: String::new(),
                    state: Some(ContainerStateValue {
                        state: crate::cri_api::ContainerState::ContainerRunning as i32,
                    }),
                    pod_sandbox_id: String::new(),
                    label_selector: HashMap::new(),
                }),
            }))
            .await
            .unwrap()
            .into_inner();

        assert_eq!(resp.containers.len(), 1);
        assert_eq!(resp.containers[0].id, "c-running");
        assert_eq!(
            resp.containers[0].state(),
            crate::cri_api::ContainerState::ContainerRunning
        );
    }

    #[tokio::test]
    async fn test_list_containers_filter_by_label_selector() {
        let svc = make_test_service();
        let mut api = test_container("c-api", "sb-1");
        api.labels.insert("app".to_string(), "api".to_string());
        api.labels.insert("tier".to_string(), "backend".to_string());

        let mut worker = test_container("c-worker", "sb-1");
        worker
            .labels
            .insert("app".to_string(), "worker".to_string());
        worker
            .labels
            .insert("tier".to_string(), "backend".to_string());

        svc.store.containers.add(api).await;
        svc.store.containers.add(worker).await;

        let resp = svc
            .list_containers(Request::new(ListContainersRequest {
                filter: Some(ContainerFilter {
                    id: String::new(),
                    state: None,
                    pod_sandbox_id: String::new(),
                    label_selector: HashMap::from([
                        ("app".to_string(), "api".to_string()),
                        ("tier".to_string(), "backend".to_string()),
                    ]),
                }),
            }))
            .await
            .unwrap()
            .into_inner();

        assert_eq!(resp.containers.len(), 1);
        assert_eq!(resp.containers[0].id, "c-api");
        assert_eq!(
            resp.containers[0].labels.get("app"),
            Some(&"api".to_string())
        );
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
    async fn test_update_container_resources_requires_running_container() {
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
                    ..Default::default()
                }),
                annotations: HashMap::new(),
            }))
            .await;

        assert!(result.is_err());
        let err = result.unwrap_err();
        assert_eq!(err.code(), tonic::Code::FailedPrecondition);
        assert!(err.message().contains("requires a running container"));
    }

    #[tokio::test]
    async fn test_update_container_resources_rejects_exited_container() {
        let svc = make_test_service();
        let mut container = test_container("c-1", "sb-1");
        container.state = ContainerState::Exited;
        container.finished_at = 3_000_000_000;
        container.exit_code = 42;
        svc.store.containers.add(container).await;

        let result = svc
            .update_container_resources(Request::new(UpdateContainerResourcesRequest {
                container_id: "c-1".to_string(),
                linux: Some(LinuxContainerResources {
                    cpu_quota: 100_000,
                    ..Default::default()
                }),
                annotations: HashMap::new(),
            }))
            .await;

        assert!(result.is_err());
        let err = result.unwrap_err();
        assert_eq!(err.code(), tonic::Code::FailedPrecondition);
        assert!(err.message().contains("requires a running container"));
    }

    #[tokio::test]
    async fn test_update_container_resources_requires_ready_vm() {
        let svc = make_test_service();
        svc.store
            .containers
            .add(test_container("c-1", "sb-1"))
            .await;
        svc.store
            .containers
            .mark_started("c-1", 2_000_000_000)
            .await;
        let vm = VmManager::with_box_id(
            a3s_box_core::config::BoxConfig::default(),
            EventEmitter::new(16),
            "sb-1".to_string(),
        );
        svc.vm_managers.write().await.insert("sb-1".to_string(), vm);

        let result = svc
            .update_container_resources(Request::new(UpdateContainerResourcesRequest {
                container_id: "c-1".to_string(),
                linux: Some(LinuxContainerResources {
                    cpu_quota: 100_000,
                    ..Default::default()
                }),
                annotations: HashMap::new(),
            }))
            .await;

        assert!(result.is_err());
        let err = result.unwrap_err();
        assert_eq!(err.code(), tonic::Code::FailedPrecondition);
        assert!(err.message().contains("VM is not ready"));
    }

    #[tokio::test]
    async fn test_update_container_resources_linux_rejected() {
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
        assert_eq!(c.exit_code, 137);
    }

    #[tokio::test]
    async fn test_stop_pod_sandbox_uses_workload_stop_controls_for_running_containers() {
        let svc = make_test_service();
        svc.store.sandboxes.add(test_sandbox("sb-1")).await;
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
            .mark_started("c-1", 2_000_000_000)
            .await;
        svc.store
            .containers
            .mark_started("c-2", 2_000_000_001)
            .await;

        let (first_stop_tx, first_stop_rx) = tokio::sync::oneshot::channel();
        let (second_stop_tx, second_stop_rx) = tokio::sync::oneshot::channel();
        {
            let mut stops = svc.workload_stops.write().await;
            stops.insert("c-1".to_string(), first_stop_tx);
            stops.insert("c-2".to_string(), second_stop_tx);
        }

        let store = svc.store.clone();
        tokio::spawn(async move {
            first_stop_rx.await.unwrap();
            sleep(Duration::from_millis(25)).await;
            let now_ns = chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0);
            store
                .mark_container_exited_if_running("c-1", now_ns, 143)
                .await;
        });

        let store = svc.store.clone();
        tokio::spawn(async move {
            second_stop_rx.await.unwrap();
            let now_ns = chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0);
            store
                .mark_container_exited_if_running("c-2", now_ns, 144)
                .await;
        });

        tokio::time::timeout(Duration::from_secs(1), async {
            svc.stop_pod_sandbox(Request::new(StopPodSandboxRequest {
                pod_sandbox_id: "sb-1".to_string(),
            }))
            .await
        })
        .await
        .expect("StopPodSandbox should wait for workload stop controls")
        .unwrap();

        let sb = svc.store.sandboxes.get("sb-1").await.unwrap();
        assert_eq!(sb.state, SandboxState::NotReady);

        let first = svc.store.containers.get("c-1").await.unwrap();
        let second = svc.store.containers.get("c-2").await.unwrap();
        assert_eq!(first.state, ContainerState::Exited);
        assert_eq!(second.state, ContainerState::Exited);
        assert_eq!(first.exit_code, 143);
        assert_eq!(second.exit_code, 144);
        assert!(!svc.workload_stops.read().await.contains_key("c-1"));
        assert!(!svc.workload_stops.read().await.contains_key("c-2"));
    }

    #[tokio::test]
    async fn test_stop_pod_sandbox_disconnects_network_endpoint() {
        let svc = make_test_service();
        let mut sandbox = test_networked_sandbox("sb-1");
        add_test_network_endpoint(&svc, &mut sandbox);
        svc.store.sandboxes.add(sandbox).await;

        svc.stop_pod_sandbox(Request::new(StopPodSandboxRequest {
            pod_sandbox_id: "sb-1".to_string(),
        }))
        .await
        .unwrap();

        let network = svc.network_store.get("cri-net").unwrap().unwrap();
        assert!(network.endpoints.is_empty());

        let sandbox = svc.store.sandboxes.get("sb-1").await.unwrap();
        assert_eq!(sandbox.state, SandboxState::NotReady);
    }

    #[tokio::test]
    async fn test_stop_pod_sandbox_removes_vm_manager() {
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
        let vm = VmManager::with_box_id(
            a3s_box_core::config::BoxConfig::default(),
            EventEmitter::new(16),
            "sb-1".to_string(),
        );
        svc.vm_managers.write().await.insert("sb-1".to_string(), vm);

        svc.stop_pod_sandbox(Request::new(StopPodSandboxRequest {
            pod_sandbox_id: "sb-1".to_string(),
        }))
        .await
        .unwrap();

        assert!(!svc.vm_managers.read().await.contains_key("sb-1"));

        let sb = svc.store.sandboxes.get("sb-1").await.unwrap();
        assert_eq!(sb.state, SandboxState::NotReady);

        let c = svc.store.containers.get("c-1").await.unwrap();
        assert_eq!(c.state, ContainerState::Exited);
        assert_eq!(c.exit_code, 137);
    }

    #[tokio::test]
    async fn test_stop_pod_sandbox_not_found() {
        let svc = make_test_service();

        let result = svc
            .stop_pod_sandbox(Request::new(StopPodSandboxRequest {
                pod_sandbox_id: "missing".to_string(),
            }))
            .await;

        assert!(result.is_err());
        assert_eq!(result.unwrap_err().code(), tonic::Code::NotFound);
    }

    #[tokio::test]
    async fn test_stop_pod_sandbox_not_ready_is_idempotent() {
        let svc = make_test_service();
        let mut sandbox = test_sandbox("sb-1");
        sandbox.state = SandboxState::NotReady;
        svc.store.sandboxes.add(sandbox).await;

        let result = svc
            .stop_pod_sandbox(Request::new(StopPodSandboxRequest {
                pod_sandbox_id: "sb-1".to_string(),
            }))
            .await;

        assert!(result.is_ok());
        let sb = svc.store.sandboxes.get("sb-1").await.unwrap();
        assert_eq!(sb.state, SandboxState::NotReady);
    }

    #[tokio::test]
    async fn test_remove_pod_sandbox_no_vm() {
        let svc = make_test_service();
        let mut sandbox = test_sandbox("sb-1");
        sandbox.state = SandboxState::NotReady;
        svc.store.sandboxes.add(sandbox).await;
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

    #[tokio::test]
    async fn test_remove_pod_sandbox_disconnects_network_endpoint() {
        let svc = make_test_service();
        let mut sandbox = test_networked_sandbox("sb-1");
        sandbox.state = SandboxState::NotReady;
        add_test_network_endpoint(&svc, &mut sandbox);
        svc.store.sandboxes.add(sandbox).await;

        svc.remove_pod_sandbox(Request::new(RemovePodSandboxRequest {
            pod_sandbox_id: "sb-1".to_string(),
        }))
        .await
        .unwrap();

        let network = svc.network_store.get("cri-net").unwrap().unwrap();
        assert!(network.endpoints.is_empty());
        assert!(svc.store.sandboxes.get("sb-1").await.is_none());
    }

    #[tokio::test]
    async fn test_remove_pod_sandbox_removes_lingering_vm_manager() {
        let svc = make_test_service();
        let mut sandbox = test_sandbox("sb-1");
        sandbox.state = SandboxState::NotReady;
        svc.store.sandboxes.add(sandbox).await;
        let vm = VmManager::with_box_id(
            a3s_box_core::config::BoxConfig::default(),
            EventEmitter::new(16),
            "sb-1".to_string(),
        );
        svc.vm_managers.write().await.insert("sb-1".to_string(), vm);

        svc.remove_pod_sandbox(Request::new(RemovePodSandboxRequest {
            pod_sandbox_id: "sb-1".to_string(),
        }))
        .await
        .unwrap();

        assert!(!svc.vm_managers.read().await.contains_key("sb-1"));
        assert!(svc.store.sandboxes.get("sb-1").await.is_none());
    }

    #[tokio::test]
    async fn test_remove_pod_sandbox_missing_is_idempotent() {
        let svc = make_test_service();

        let result = svc
            .remove_pod_sandbox(Request::new(RemovePodSandboxRequest {
                pod_sandbox_id: "missing".to_string(),
            }))
            .await;

        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_remove_pod_sandbox_rejects_ready_sandbox() {
        let svc = make_test_service();
        svc.store.sandboxes.add(test_sandbox("sb-1")).await;
        svc.store
            .containers
            .add(test_container("c-1", "sb-1"))
            .await;

        let result = svc
            .remove_pod_sandbox(Request::new(RemovePodSandboxRequest {
                pod_sandbox_id: "sb-1".to_string(),
            }))
            .await;

        assert!(result.is_err());
        let err = result.unwrap_err();
        assert_eq!(err.code(), tonic::Code::FailedPrecondition);
        assert!(err.message().contains("requires a stopped sandbox"));
        assert!(svc.store.sandboxes.get("sb-1").await.is_some());
        assert!(svc.store.containers.get("c-1").await.is_some());
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
        svc.store
            .containers
            .mark_started("c-1", 2_000_000_000)
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
    async fn test_exec_sync_rejects_empty_command() {
        let svc = make_test_service();
        let result = svc
            .exec_sync(Request::new(ExecSyncRequest {
                container_id: "c-1".to_string(),
                cmd: vec![],
                timeout: 0,
            }))
            .await;

        assert!(result.is_err());
        let err = result.unwrap_err();
        assert_eq!(err.code(), tonic::Code::InvalidArgument);
        assert!(err.message().contains("at least one argument"));
    }

    #[tokio::test]
    async fn test_exec_sync_requires_running_container() {
        let svc = make_test_service();
        svc.store
            .containers
            .add(test_container("c-1", "sb-1"))
            .await;

        let result = svc
            .exec_sync(Request::new(ExecSyncRequest {
                container_id: "c-1".to_string(),
                cmd: vec!["ls".to_string()],
                timeout: 0,
            }))
            .await;

        assert!(result.is_err());
        let err = result.unwrap_err();
        assert_eq!(err.code(), tonic::Code::FailedPrecondition);
        assert!(err.message().contains("requires a running container"));
    }

    #[tokio::test]
    async fn test_exec_sync_requires_ready_vm() {
        let svc = make_test_service();
        svc.store
            .containers
            .add(test_container("c-1", "sb-1"))
            .await;
        svc.store
            .containers
            .mark_started("c-1", 2_000_000_000)
            .await;
        let vm = VmManager::with_box_id(
            a3s_box_core::config::BoxConfig::default(),
            EventEmitter::new(16),
            "sb-1".to_string(),
        );
        svc.vm_managers.write().await.insert("sb-1".to_string(), vm);

        let result = svc
            .exec_sync(Request::new(ExecSyncRequest {
                container_id: "c-1".to_string(),
                cmd: vec!["ls".to_string()],
                timeout: 0,
            }))
            .await;

        assert!(result.is_err());
        let err = result.unwrap_err();
        assert_eq!(err.code(), tonic::Code::FailedPrecondition);
        assert!(err.message().contains("VM is not ready"));
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
    async fn test_exec_rejects_empty_command() {
        let svc = make_test_service();
        let result = svc
            .exec(Request::new(ExecRequest {
                container_id: "c-1".to_string(),
                cmd: vec![],
                tty: false,
                stdin: false,
                stdout: true,
                stderr: true,
            }))
            .await;

        assert!(result.is_err());
        let err = result.unwrap_err();
        assert_eq!(err.code(), tonic::Code::InvalidArgument);
        assert!(err.message().contains("at least one argument"));
    }

    #[tokio::test]
    async fn test_exec_requires_running_container() {
        let svc = make_test_service();
        svc.store
            .containers
            .add(test_container("c-1", "sb-1"))
            .await;

        let result = svc
            .exec(Request::new(ExecRequest {
                container_id: "c-1".to_string(),
                cmd: vec!["sh".to_string()],
                tty: false,
                stdin: false,
                stdout: true,
                stderr: true,
            }))
            .await;

        assert!(result.is_err());
        let err = result.unwrap_err();
        assert_eq!(err.code(), tonic::Code::FailedPrecondition);
        assert!(err.message().contains("requires a running container"));
    }

    #[tokio::test]
    async fn test_exec_requires_ready_vm() {
        let svc = make_test_service();
        svc.store
            .containers
            .add(test_container("c-1", "sb-1"))
            .await;
        svc.store
            .containers
            .mark_started("c-1", 2_000_000_000)
            .await;
        let vm = VmManager::with_box_id(
            a3s_box_core::config::BoxConfig::default(),
            EventEmitter::new(16),
            "sb-1".to_string(),
        );
        svc.vm_managers.write().await.insert("sb-1".to_string(), vm);

        let result = svc
            .exec(Request::new(ExecRequest {
                container_id: "c-1".to_string(),
                cmd: vec!["sh".to_string()],
                tty: false,
                stdin: false,
                stdout: true,
                stderr: true,
            }))
            .await;

        assert!(result.is_err());
        let err = result.unwrap_err();
        assert_eq!(err.code(), tonic::Code::FailedPrecondition);
        assert!(err.message().contains("VM is not ready"));
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
    async fn test_attach_rejects_without_streams() {
        let svc = make_test_service();
        let result = svc
            .attach(Request::new(AttachRequest {
                container_id: "c-1".to_string(),
                stdin: false,
                tty: false,
                stdout: false,
                stderr: false,
            }))
            .await;

        assert!(result.is_err());
        let err = result.unwrap_err();
        assert_eq!(err.code(), tonic::Code::InvalidArgument);
        assert!(err.message().contains("at least one stream"));
    }

    #[tokio::test]
    async fn test_attach_stdin_requires_container_stdin_enabled() {
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
            .attach(Request::new(AttachRequest {
                container_id: "c-1".to_string(),
                stdin: true,
                tty: false,
                stdout: true,
                stderr: true,
            }))
            .await;

        assert!(result.is_err());
        let err = result.unwrap_err();
        assert_eq!(err.code(), tonic::Code::FailedPrecondition);
        assert!(err.message().contains("stdin enabled"));
    }

    #[tokio::test]
    async fn test_attach_rejects_tty_mismatch() {
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
            .attach(Request::new(AttachRequest {
                container_id: "c-1".to_string(),
                stdin: false,
                tty: true,
                stdout: true,
                stderr: true,
            }))
            .await;

        assert!(result.is_err());
        let err = result.unwrap_err();
        assert_eq!(err.code(), tonic::Code::FailedPrecondition);
        assert!(err.message().contains("TTY flag must match"));
    }

    #[tokio::test]
    async fn test_attach_requires_running_container() {
        let svc = make_test_service();
        svc.store
            .containers
            .add(test_container("c-1", "sb-1"))
            .await;

        let result = svc
            .attach(Request::new(AttachRequest {
                container_id: "c-1".to_string(),
                stdin: false,
                tty: false,
                stdout: true,
                stderr: true,
            }))
            .await;

        assert!(result.is_err());
        let err = result.unwrap_err();
        assert_eq!(err.code(), tonic::Code::FailedPrecondition);
        assert!(err.message().contains("requires a running container"));
    }

    #[tokio::test]
    async fn test_attach_requires_ready_vm() {
        let svc = make_test_service();
        svc.store
            .containers
            .add(test_container("c-1", "sb-1"))
            .await;
        svc.store
            .containers
            .mark_started("c-1", 2_000_000_000)
            .await;
        let vm = VmManager::with_box_id(
            a3s_box_core::config::BoxConfig::default(),
            EventEmitter::new(16),
            "sb-1".to_string(),
        );
        svc.vm_managers.write().await.insert("sb-1".to_string(), vm);

        let result = svc
            .attach(Request::new(AttachRequest {
                container_id: "c-1".to_string(),
                stdin: false,
                tty: false,
                stdout: true,
                stderr: true,
            }))
            .await;

        assert!(result.is_err());
        let err = result.unwrap_err();
        assert_eq!(err.code(), tonic::Code::FailedPrecondition);
        assert!(err.message().contains("VM is not ready"));
    }

    #[tokio::test]
    async fn test_attach_requires_active_workload_stream() {
        let svc = make_test_service();
        svc.store
            .containers
            .add(test_container("c-1", "sb-1"))
            .await;
        svc.store
            .containers
            .mark_started("c-1", 2_000_000_000)
            .await;
        let Some(exec_server) = spawn_exec_stream_server(b"", b"", 0, Duration::from_secs(1)).await
        else {
            return;
        };
        let vm = attach_ready_test_vm("sb-1", &exec_server.socket_path).await;
        svc.vm_managers.write().await.insert("sb-1".to_string(), vm);

        let result = svc
            .attach(Request::new(AttachRequest {
                container_id: "c-1".to_string(),
                stdin: false,
                tty: false,
                stdout: true,
                stderr: true,
            }))
            .await;

        assert!(result.is_err());
        let err = result.unwrap_err();
        assert_eq!(err.code(), tonic::Code::FailedPrecondition);
        assert!(err.message().contains("active workload stream"));
    }

    #[tokio::test]
    async fn test_attach_stdin_once_consumes_workload_stdin_handle() {
        let svc = make_test_service();
        svc.store.sandboxes.add(test_sandbox("sb-1")).await;
        let mut container = test_container("c-1", "sb-1");
        container.command = vec!["cat".to_string()];
        container.args = vec![];
        container.stdin = true;
        container.stdin_once = true;
        svc.store.containers.add(container).await;

        let Some(exec_server) =
            spawn_exec_stream_server_with_assert(b"", b"", 0, Duration::from_secs(1), |request| {
                assert!(request.stdin_streaming);
            })
            .await
        else {
            return;
        };
        let vm = attach_ready_test_vm("sb-1", &exec_server.socket_path).await;
        svc.vm_managers.write().await.insert("sb-1".to_string(), vm);

        svc.start_container(Request::new(StartContainerRequest {
            container_id: "c-1".to_string(),
        }))
        .await
        .unwrap();
        assert!(svc.workload_stdins.read().await.contains_key("c-1"));

        let response = svc
            .attach(Request::new(AttachRequest {
                container_id: "c-1".to_string(),
                stdin: true,
                tty: false,
                stdout: true,
                stderr: true,
            }))
            .await
            .unwrap()
            .into_inner();

        assert!(response.url.contains("/attach/"));
        assert!(!svc.workload_stdins.read().await.contains_key("c-1"));
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

    #[tokio::test]
    async fn test_port_forward_rejects_empty_ports() {
        let svc = make_test_service();
        let result = svc
            .port_forward(Request::new(PortForwardRequest {
                pod_sandbox_id: "sb-1".to_string(),
                port: vec![],
            }))
            .await;

        assert!(result.is_err());
        let err = result.unwrap_err();
        assert_eq!(err.code(), tonic::Code::InvalidArgument);
        assert!(err.message().contains("at least one port"));
    }

    #[tokio::test]
    async fn test_port_forward_rejects_multiple_ports() {
        let svc = make_test_service();
        let result = svc
            .port_forward(Request::new(PortForwardRequest {
                pod_sandbox_id: "sb-1".to_string(),
                port: vec![8080, 9090],
            }))
            .await;

        assert!(result.is_err());
        let err = result.unwrap_err();
        assert_eq!(err.code(), tonic::Code::Unimplemented);
        assert!(err.message().contains("exactly one port"));
    }

    #[tokio::test]
    async fn test_port_forward_requires_ready_sandbox() {
        let svc = make_test_service();
        let mut sandbox = test_sandbox("sb-1");
        sandbox.state = SandboxState::NotReady;
        svc.store.sandboxes.add(sandbox).await;

        let result = svc
            .port_forward(Request::new(PortForwardRequest {
                pod_sandbox_id: "sb-1".to_string(),
                port: vec![8080],
            }))
            .await;

        assert!(result.is_err());
        let err = result.unwrap_err();
        assert_eq!(err.code(), tonic::Code::FailedPrecondition);
        assert!(err.message().contains("requires a ready sandbox"));
    }

    #[tokio::test]
    async fn test_port_forward_requires_ready_vm() {
        let svc = make_test_service();
        svc.store.sandboxes.add(test_sandbox("sb-1")).await;
        let vm = VmManager::with_box_id(
            a3s_box_core::config::BoxConfig::default(),
            EventEmitter::new(16),
            "sb-1".to_string(),
        );
        svc.vm_managers.write().await.insert("sb-1".to_string(), vm);

        let result = svc
            .port_forward(Request::new(PortForwardRequest {
                pod_sandbox_id: "sb-1".to_string(),
                port: vec![8080],
            }))
            .await;

        assert!(result.is_err());
        let err = result.unwrap_err();
        assert_eq!(err.code(), tonic::Code::FailedPrecondition);
        assert!(err.message().contains("VM is not ready"));
    }

    #[tokio::test]
    async fn test_port_forward_registers_session_for_recovered_ready_vm() {
        let svc = make_test_service();
        svc.store.sandboxes.add(test_sandbox("sb-1")).await;

        let tmp = tempfile::tempdir().unwrap();
        let exec_socket_path = tmp.path().join("exec.sock");
        let vm = attach_ready_test_vm("sb-1", &exec_socket_path).await;
        assert_eq!(
            vm.port_forward_socket_path(),
            Some(exec_socket_path.with_file_name("portfwd.sock").as_path())
        );
        svc.vm_managers.write().await.insert("sb-1".to_string(), vm);

        let result = svc
            .port_forward(Request::new(PortForwardRequest {
                pod_sandbox_id: "sb-1".to_string(),
                port: vec![8080],
            }))
            .await
            .unwrap();

        let url = result.into_inner().url;
        assert!(url.starts_with("http://127.0.0.1:0/portforward/"));
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
            let svc = make_test_service().with_warm_pool(pool);

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
        // Without a warm pool, CRI sandbox acquisition cold-boots and fails in unit test env.
        let svc = make_test_service();
        let config = a3s_box_core::config::BoxConfig::default();
        let result = svc
            .acquire_vm_with_box_id(config, "test-acquire".to_string())
            .await;
        // Expected: error because no shim binary available
        assert!(result.is_err());
    }
}
