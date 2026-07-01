//! Conversion, validation, and label helpers for the CRI runtime service.
//!
//! Free functions that translate between CRI protobuf types and the internal
//! [`crate::container`]/[`crate::sandbox`] representations, plus small
//! precondition guards used by [`super::BoxRuntimeService`].

use std::path::Path;

use tonic::Status;

use a3s_box_runtime::oci::OciImageConfig;
use a3s_box_runtime::vm::VmManager;

use crate::container::{Container, ContainerMount, ContainerState};
use crate::cri_api::*;
use crate::sandbox::{PodSandbox, SandboxState};

pub(super) const ANN_POD_IP: &str = "a3s.box/pod-ip";
pub(super) const ANN_ADDITIONAL_POD_IPS: &str = "a3s.box/additional-pod-ips";
const DEFAULT_STOP_CONTAINER_WAIT_SECS: u64 = 10;

pub(super) struct ResolvedContainerImage {
    pub(super) digest: String,
    pub(super) path: String,
    pub(super) config: OciImageConfig,
}

pub(super) struct ContainerRootfsPaths {
    pub(super) host_path: std::path::PathBuf,
    pub(super) guest_path: String,
}

pub(super) fn sanitize_path_component(value: &str) -> String {
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

pub(super) fn container_user_from_linux_config(
    linux: Option<&LinuxContainerConfig>,
) -> Option<String> {
    let security_context = linux.and_then(|linux| linux.security_context.as_ref())?;

    if !security_context.run_as_username.is_empty() {
        // Preserve an explicitly-requested run_as_group for the named-user path
        // too (the guest resolves `name:gid`), otherwise the requested gid is
        // silently dropped and the process falls back to the passwd primary gid.
        return Some(match security_context.run_as_group.as_ref() {
            Some(group) => format!("{}:{}", security_context.run_as_username, group.value),
            None => security_context.run_as_username.clone(),
        });
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

pub(super) fn container_mount_to_cri(mount: &ContainerMount) -> Mount {
    Mount {
        container_path: mount.container_path.clone(),
        host_path: mount.host_path.clone(),
        readonly: mount.readonly,
        selinux_relabel: mount.selinux_relabel,
        propagation: mount.propagation,
    }
}

/// Reject CRI namespace options that a microVM-per-pod runtime cannot honor.
///
/// Each pod is an isolated microVM with its own kernel and namespaces, so it
/// cannot share the *host's* network/IPC/user namespace (`NamespaceMode::NODE`
/// — i.e. HostNetwork / HostIpc): there is no host network or IPC namespace
/// inside the guest. Rather than silently running such a pod fully isolated
/// (the wrong semantics, and a fail-open surprise for the workload), reject it
/// with a clear error, matching the fail-closed handling of unsupported mount
/// propagation above.
///
/// `HostPID` (`pid == NODE`) is NOT rejected: all of a pod's processes already
/// share the single VM-wide PID namespace (incl. the VM's PID 1), which is the
/// broadest PID namespace available in the guest — there is no separate host
/// PID namespace to be denied, so HostPID is legitimately satisfied. `POD`/
/// `CONTAINER`/`TARGET` are likewise accepted (one shared VM namespace set).
pub(super) fn validate_namespace_options(
    options: Option<&NamespaceOption>,
    context: &str,
) -> Result<(), Status> {
    let Some(options) = options else {
        return Ok(());
    };
    let host = crate::cri_api::namespace_option::NamespaceMode::Node as i32;
    for (mode, kind) in [
        (options.network, "network (HostNetwork)"),
        (options.ipc, "IPC (HostIpc)"),
        (options.user, "user"),
    ] {
        if mode == host {
            return Err(Status::unimplemented(format!(
                "{context}: host {kind} namespace (NamespaceMode::NODE) is not supported by the \
                 microVM-per-pod runtime — each pod runs in an isolated VM and cannot share the \
                 host's namespaces"
            )));
        }
    }
    Ok(())
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
    // Writable mounts are accepted but materialized by COPYING the source into
    // the container rootfs (microVM-backed containers cannot bind-mount host
    // paths post-boot). The container sees the contents and may write to its
    // copy, but writes do NOT propagate back to the host source — sufficient for
    // read-oriented volumes (configMap/secret/downwardAPI) and the basic volume
    // conformance; true host propagation (and the propagation modes below) needs
    // a shared mount and is intentionally still rejected.
    //
    // SELinux relabeling is a no-op on this non-SELinux runtime; accept it
    // rather than failing the container so labeled volumes still work.

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

pub(super) fn resolve_container_mounts(mounts: &[Mount]) -> Result<Vec<ContainerMount>, Status> {
    mounts
        .iter()
        .map(|mount| {
            validate_container_mount(mount)?;
            Ok(container_mount_from_cri(mount))
        })
        .collect()
}

pub(super) fn merge_env(
    image_env: &[(String, String)],
    cri_env: &[KeyValue],
) -> Vec<(String, String)> {
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

pub(super) fn resolve_command_and_args(
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

pub(super) fn container_exit_reason(exit_code: i32, oom_killed: bool) -> (&'static str, String) {
    if oom_killed {
        (
            "OOMKilled",
            format!("Container was killed by the out-of-memory killer (exit code {exit_code})"),
        )
    } else if exit_code == 0 {
        ("Completed", "Container exited successfully".to_string())
    } else {
        ("Error", format!("Container exited with code {exit_code}"))
    }
}

pub(super) fn ensure_container_running(
    container: &Container,
    operation: &str,
) -> Result<(), Status> {
    if container.state == ContainerState::Running {
        return Ok(());
    }

    Err(Status::failed_precondition(format!(
        "{operation} requires a running container; container {} is {:?}",
        container.id, container.state
    )))
}

pub(super) fn ensure_sandbox_ready(sandbox: &PodSandbox, operation: &str) -> Result<(), Status> {
    if sandbox.state == SandboxState::Ready {
        return Ok(());
    }

    Err(Status::failed_precondition(format!(
        "{operation} requires a ready sandbox; sandbox {} is {:?}",
        sandbox.id, sandbox.state
    )))
}

pub(super) async fn ensure_container_image_available(container: &Container) -> Result<(), Status> {
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

pub(super) fn sandbox_state_label(state: SandboxState) -> &'static str {
    match state {
        SandboxState::Ready => "ready",
        SandboxState::NotReady => "not_ready",
        SandboxState::Removed => "removed",
    }
}

pub(super) fn container_state_label(state: ContainerState) -> &'static str {
    match state {
        ContainerState::Created => "created",
        ContainerState::Running => "running",
        ContainerState::Exited => "exited",
    }
}

pub(super) fn container_state_to_cri(state: ContainerState) -> crate::cri_api::ContainerState {
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

pub(super) fn container_summary(container: Container) -> crate::cri_api::Container {
    let status_image_ref = container.status_image_ref().to_string();

    crate::cri_api::Container {
        id: container.id,
        pod_sandbox_id: container.sandbox_id,
        metadata: Some(ContainerMetadata {
            name: container.name,
            attempt: container.attempt,
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

pub(super) fn sandbox_summary(sandbox: PodSandbox) -> crate::cri_api::PodSandbox {
    crate::cri_api::PodSandbox {
        id: sandbox.id,
        metadata: Some(PodSandboxMetadata {
            name: sandbox.name,
            uid: sandbox.uid,
            namespace: sandbox.namespace,
            attempt: sandbox.attempt,
        }),
        state: sandbox_state_to_cri(sandbox.state).into(),
        created_at: sandbox.created_at,
        labels: sandbox.labels,
        annotations: sandbox.annotations,
        runtime_handler: sandbox.runtime_handler,
    }
}

pub(super) async fn ensure_vm_ready(
    vm: &VmManager,
    operation: &str,
    sandbox_id: &str,
) -> Result<(), Status> {
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

pub(super) fn stop_container_timeout_ms(timeout_seconds: i64) -> Option<u64> {
    if timeout_seconds <= 0 {
        return None;
    }

    Some((timeout_seconds as u64).saturating_mul(1_000))
}

pub(super) fn stop_container_wait_duration(timeout_seconds: i64) -> tokio::time::Duration {
    if timeout_seconds <= 0 {
        return tokio::time::Duration::from_secs(DEFAULT_STOP_CONTAINER_WAIT_SECS);
    }

    tokio::time::Duration::from_secs(timeout_seconds as u64)
}

// ── Container event helpers ──────────────────────────────────────────

pub(super) fn container_event_response(
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cri_api::namespace_option::NamespaceMode;
    use crate::cri_api::NamespaceOption;

    #[test]
    fn test_container_user_named_preserves_run_as_group() {
        use crate::cri_api::{Int64Value, LinuxContainerConfig, LinuxContainerSecurityContext};
        let with_group = LinuxContainerConfig {
            security_context: Some(LinuxContainerSecurityContext {
                run_as_username: "appuser".to_string(),
                run_as_group: Some(Int64Value { value: 2000 }),
                ..Default::default()
            }),
            ..Default::default()
        };
        // The explicitly-requested gid must not be dropped for the named-user path.
        assert_eq!(
            container_user_from_linux_config(Some(&with_group)),
            Some("appuser:2000".to_string())
        );

        let no_group = LinuxContainerConfig {
            security_context: Some(LinuxContainerSecurityContext {
                run_as_username: "appuser".to_string(),
                ..Default::default()
            }),
            ..Default::default()
        };
        assert_eq!(
            container_user_from_linux_config(Some(&no_group)),
            Some("appuser".to_string())
        );
    }

    #[test]
    fn test_container_exit_reason_oom_killed_overrides_code() {
        // OOMKilled wins regardless of exit code (the OOM killer SIGKILLs → 137).
        let (reason, message) = container_exit_reason(137, true);
        assert_eq!(reason, "OOMKilled");
        assert!(message.contains("out-of-memory"));
        // A zero exit code that was still an OOM is reported as OOMKilled.
        assert_eq!(container_exit_reason(0, true).0, "OOMKilled");
        // Non-OOM exits keep Completed / Error.
        assert_eq!(container_exit_reason(0, false).0, "Completed");
        assert_eq!(container_exit_reason(1, false).0, "Error");
    }

    fn ns(network: NamespaceMode, pid: NamespaceMode, ipc: NamespaceMode) -> NamespaceOption {
        NamespaceOption {
            network: network as i32,
            pid: pid as i32,
            ipc: ipc as i32,
            target_id: String::new(),
            user: NamespaceMode::Pod as i32,
        }
    }

    #[test]
    fn test_validate_namespace_options_accepts_default_and_none() {
        assert!(validate_namespace_options(None, "X").is_ok());
        // POD/CONTAINER (the kubelet default for ordinary pods) are accepted.
        assert!(validate_namespace_options(
            Some(&ns(
                NamespaceMode::Pod,
                NamespaceMode::Container,
                NamespaceMode::Pod
            )),
            "X"
        )
        .is_ok());
    }

    #[test]
    fn test_validate_namespace_options_accepts_host_pid() {
        // HostPID is satisfied by the pod's shared VM-wide PID namespace — must
        // NOT be rejected (regression guard for "runtime should support HostPID").
        assert!(validate_namespace_options(
            Some(&ns(
                NamespaceMode::Pod,
                NamespaceMode::Node,
                NamespaceMode::Pod
            )),
            "X"
        )
        .is_ok());
    }

    #[test]
    fn test_validate_namespace_options_rejects_host_network_and_ipc() {
        for opts in [
            ns(
                NamespaceMode::Node,
                NamespaceMode::Container,
                NamespaceMode::Pod,
            ), // HostNetwork
            ns(
                NamespaceMode::Pod,
                NamespaceMode::Container,
                NamespaceMode::Node,
            ), // HostIpc
        ] {
            let err = validate_namespace_options(Some(&opts), "RunPodSandbox").unwrap_err();
            assert_eq!(err.code(), tonic::Code::Unimplemented);
        }
    }
}
