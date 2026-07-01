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
    use std::collections::HashMap;

    use tempfile::TempDir;

    fn test_image_config() -> OciImageConfig {
        OciImageConfig {
            entrypoint: Some(vec!["/entrypoint".to_string()]),
            cmd: Some(vec!["--serve".to_string()]),
            env: vec![("PATH".to_string(), "/usr/bin".to_string())],
            working_dir: Some("/app".to_string()),
            user: Some("1000".to_string()),
            exposed_ports: vec![],
            labels: HashMap::new(),
            volumes: vec![],
            stop_signal: None,
            health_check: None,
            onbuild: vec![],
        }
    }

    fn test_container(id: &str, state: ContainerState) -> Container {
        Container {
            id: id.to_string(),
            sandbox_id: "sandbox-1".to_string(),
            name: format!("container-{id}"),
            attempt: 2,
            image_ref: "example.com/app:latest".to_string(),
            resolved_image_digest: "sha256:resolved".to_string(),
            resolved_image_path: String::new(),
            command: vec!["/entrypoint".to_string()],
            args: vec!["--serve".to_string()],
            env: vec![],
            working_dir: String::new(),
            user: None,
            stdin: false,
            stdin_once: false,
            tty: false,
            mounts: vec![],
            state,
            created_at: 100,
            started_at: 0,
            finished_at: 0,
            exit_code: 0,
            oom_killed: false,
            labels: HashMap::from([("app".to_string(), "demo".to_string())]),
            annotations: HashMap::from([("anno".to_string(), "value".to_string())]),
            log_path: "/var/log/pods/container.log".to_string(),
            rootfs_path: String::new(),
            rootfs_guest_path: String::new(),
        }
    }

    fn test_sandbox(state: SandboxState) -> PodSandbox {
        PodSandbox {
            id: "sandbox-1".to_string(),
            name: "pod".to_string(),
            namespace: "default".to_string(),
            uid: "uid-1".to_string(),
            attempt: 3,
            state,
            created_at: 200,
            labels: HashMap::from([("tier".to_string(), "backend".to_string())]),
            annotations: HashMap::from([("owner".to_string(), "tests".to_string())]),
            log_directory: "/var/log/pods".to_string(),
            runtime_handler: "a3s".to_string(),
            network_ip: "10.0.0.2".to_string(),
            additional_ips: vec![],
            dns: Default::default(),
            container_ports: vec![],
        }
    }

    fn mount(container_path: &str, host_path: &str) -> Mount {
        Mount {
            container_path: container_path.to_string(),
            host_path: host_path.to_string(),
            readonly: true,
            selinux_relabel: false,
            propagation: crate::cri_api::mount::MountPropagation::PropagationPrivate as i32,
        }
    }

    #[test]
    fn test_sanitize_path_component_replaces_unsafe_chars_and_empty_values() {
        assert_eq!(sanitize_path_component("pod-01_APP"), "pod-01_APP");
        assert_eq!(sanitize_path_component("team/ns:pod.1"), "team_ns_pod_1");
        assert_eq!(sanitize_path_component(""), "unknown");
        assert_eq!(sanitize_path_component("///"), "___");
    }

    #[test]
    fn test_container_user_numeric_and_missing_linux_config() {
        use crate::cri_api::{Int64Value, LinuxContainerConfig, LinuxContainerSecurityContext};

        assert_eq!(container_user_from_linux_config(None), None);
        assert_eq!(
            container_user_from_linux_config(Some(&LinuxContainerConfig::default())),
            None
        );

        let numeric = LinuxContainerConfig {
            security_context: Some(LinuxContainerSecurityContext {
                run_as_user: Some(Int64Value { value: 1000 }),
                ..Default::default()
            }),
            ..Default::default()
        };
        assert_eq!(
            container_user_from_linux_config(Some(&numeric)),
            Some("1000".to_string())
        );

        let numeric_with_group = LinuxContainerConfig {
            security_context: Some(LinuxContainerSecurityContext {
                run_as_user: Some(Int64Value { value: 1000 }),
                run_as_group: Some(Int64Value { value: 2000 }),
                ..Default::default()
            }),
            ..Default::default()
        };
        assert_eq!(
            container_user_from_linux_config(Some(&numeric_with_group)),
            Some("1000:2000".to_string())
        );
    }

    #[test]
    fn test_resolve_container_mounts_accepts_private_mounts_and_round_trips() {
        let mounts = resolve_container_mounts(&[Mount {
            readonly: false,
            selinux_relabel: true,
            ..mount("/data", "/host/data")
        }])
        .unwrap();

        assert_eq!(mounts.len(), 1);
        assert_eq!(mounts[0].container_path, "/data");
        assert_eq!(mounts[0].host_path, "/host/data");
        assert!(!mounts[0].readonly);
        assert!(mounts[0].selinux_relabel);

        let cri_mount = container_mount_to_cri(&mounts[0]);
        assert_eq!(cri_mount.container_path, "/data");
        assert_eq!(cri_mount.host_path, "/host/data");
        assert!(!cri_mount.readonly);
        assert!(cri_mount.selinux_relabel);
    }

    #[test]
    fn test_resolve_container_mounts_rejects_invalid_mounts() {
        for bad_mount in [
            mount("/data", " "),
            mount("relative/path", "/host/data"),
            mount(" ", "/host/data"),
            Mount {
                propagation: 99,
                ..mount("/data", "/host/data")
            },
            Mount {
                propagation: crate::cri_api::mount::MountPropagation::PropagationBidirectional
                    as i32,
                ..mount("/data", "/host/data")
            },
        ] {
            assert!(resolve_container_mounts(&[bad_mount]).is_err());
        }
    }

    #[test]
    fn test_merge_env_overrides_image_values_and_keeps_order() {
        let merged = merge_env(
            &[
                ("PATH".to_string(), "/usr/bin".to_string()),
                ("MODE".to_string(), "prod".to_string()),
            ],
            &[
                KeyValue {
                    key: "MODE".to_string(),
                    value: "debug".to_string(),
                },
                KeyValue {
                    key: "NEW".to_string(),
                    value: "1".to_string(),
                },
                KeyValue {
                    key: "MODE".to_string(),
                    value: "final".to_string(),
                },
            ],
        );

        assert_eq!(
            merged,
            vec![
                ("PATH".to_string(), "/usr/bin".to_string()),
                ("MODE".to_string(), "final".to_string()),
                ("NEW".to_string(), "1".to_string()),
            ]
        );
    }

    #[test]
    fn test_resolve_command_and_args_precedence() {
        let image = test_image_config();

        let explicit = ContainerConfig {
            command: vec!["/override".to_string()],
            args: vec!["--debug".to_string()],
            ..Default::default()
        };
        assert_eq!(
            resolve_command_and_args(&explicit, Some(&image)),
            (vec!["/override".to_string()], vec!["--debug".to_string()])
        );

        let args_only = ContainerConfig {
            args: vec!["--from-cri".to_string()],
            ..Default::default()
        };
        assert_eq!(
            resolve_command_and_args(&args_only, Some(&image)),
            (
                vec!["/entrypoint".to_string()],
                vec!["--from-cri".to_string()]
            )
        );

        assert_eq!(
            resolve_command_and_args(&ContainerConfig::default(), Some(&image)),
            (vec!["/entrypoint".to_string()], vec!["--serve".to_string()])
        );
        assert_eq!(
            resolve_command_and_args(&ContainerConfig::default(), None),
            (Vec::<String>::new(), Vec::<String>::new())
        );
    }

    #[test]
    fn test_precondition_helpers_return_failed_precondition_status() {
        assert!(
            ensure_container_running(&test_container("running", ContainerState::Running), "X")
                .is_ok()
        );
        let container_err =
            ensure_container_running(&test_container("created", ContainerState::Created), "Exec")
                .unwrap_err();
        assert_eq!(container_err.code(), tonic::Code::FailedPrecondition);
        assert!(container_err
            .message()
            .contains("requires a running container"));

        assert!(ensure_sandbox_ready(&test_sandbox(SandboxState::Ready), "X").is_ok());
        let sandbox_err =
            ensure_sandbox_ready(&test_sandbox(SandboxState::NotReady), "Create").unwrap_err();
        assert_eq!(sandbox_err.code(), tonic::Code::FailedPrecondition);
        assert!(sandbox_err.message().contains("requires a ready sandbox"));
    }

    #[tokio::test]
    async fn test_ensure_container_image_available_preconditions() {
        let mut no_image = test_container("scratch", ContainerState::Created);
        no_image.image_ref.clear();
        assert!(ensure_container_image_available(&no_image).await.is_ok());

        let mut missing_metadata = test_container("missing-metadata", ContainerState::Created);
        missing_metadata.resolved_image_digest.clear();
        missing_metadata.resolved_image_path.clear();
        let err = ensure_container_image_available(&missing_metadata)
            .await
            .unwrap_err();
        assert_eq!(err.code(), tonic::Code::FailedPrecondition);
        assert!(err.message().contains("without resolved image metadata"));

        let tmp = TempDir::new().unwrap();
        let image_file = tmp.path().join("image-file");
        std::fs::write(&image_file, b"not a dir").unwrap();
        let mut file_image_path = test_container("file-image", ContainerState::Created);
        file_image_path.resolved_image_path = image_file.to_string_lossy().to_string();
        file_image_path.rootfs_path = tmp.path().join("rootfs").to_string_lossy().to_string();
        file_image_path.rootfs_guest_path = "/run/a3s/rootfs".to_string();
        let err = ensure_container_image_available(&file_image_path)
            .await
            .unwrap_err();
        assert_eq!(err.code(), tonic::Code::FailedPrecondition);
        assert!(err.message().contains("is not a directory"));

        let image_dir = tmp.path().join("image-dir");
        std::fs::create_dir(&image_dir).unwrap();
        let mut missing_rootfs = test_container("missing-rootfs", ContainerState::Created);
        missing_rootfs.resolved_image_path = image_dir.to_string_lossy().to_string();
        missing_rootfs.rootfs_path.clear();
        missing_rootfs.rootfs_guest_path = "/run/a3s/rootfs".to_string();
        let err = ensure_container_image_available(&missing_rootfs)
            .await
            .unwrap_err();
        assert_eq!(err.code(), tonic::Code::FailedPrecondition);
        assert!(err.message().contains("without prepared rootfs metadata"));

        let rootfs_dir = tmp.path().join("rootfs-dir");
        std::fs::create_dir(&rootfs_dir).unwrap();
        let mut available = test_container("available", ContainerState::Created);
        available.resolved_image_path = image_dir.to_string_lossy().to_string();
        available.rootfs_path = rootfs_dir.to_string_lossy().to_string();
        available.rootfs_guest_path = "/run/a3s/rootfs".to_string();
        assert!(ensure_container_image_available(&available).await.is_ok());
    }

    #[test]
    fn test_state_labels_and_cri_state_mappings() {
        assert_eq!(sandbox_state_label(SandboxState::Ready), "ready");
        assert_eq!(sandbox_state_label(SandboxState::NotReady), "not_ready");
        assert_eq!(sandbox_state_label(SandboxState::Removed), "removed");

        assert_eq!(container_state_label(ContainerState::Created), "created");
        assert_eq!(container_state_label(ContainerState::Running), "running");
        assert_eq!(container_state_label(ContainerState::Exited), "exited");

        assert_eq!(
            container_state_to_cri(ContainerState::Created),
            crate::cri_api::ContainerState::ContainerCreated
        );
        assert_eq!(
            container_state_to_cri(ContainerState::Running),
            crate::cri_api::ContainerState::ContainerRunning
        );
        assert_eq!(
            container_state_to_cri(ContainerState::Exited),
            crate::cri_api::ContainerState::ContainerExited
        );
    }

    #[test]
    fn test_container_and_sandbox_summaries_preserve_cri_fields() {
        let container = container_summary(test_container("c1", ContainerState::Running));
        assert_eq!(container.id, "c1");
        assert_eq!(container.pod_sandbox_id, "sandbox-1");
        assert_eq!(container.metadata.unwrap().name, "container-c1");
        assert_eq!(container.image.unwrap().image, "example.com/app:latest");
        assert_eq!(container.image_ref, "sha256:resolved");
        assert_eq!(
            crate::cri_api::ContainerState::try_from(container.state).unwrap(),
            crate::cri_api::ContainerState::ContainerRunning
        );
        assert_eq!(container.created_at, 100);
        assert_eq!(container.labels.get("app"), Some(&"demo".to_string()));
        assert_eq!(
            container.annotations.get("anno"),
            Some(&"value".to_string())
        );

        let ready = sandbox_summary(test_sandbox(SandboxState::Ready));
        assert_eq!(ready.id, "sandbox-1");
        assert_eq!(ready.metadata.unwrap().attempt, 3);
        assert_eq!(
            PodSandboxState::try_from(ready.state).unwrap(),
            PodSandboxState::SandboxReady
        );
        assert_eq!(ready.runtime_handler, "a3s");
        assert_eq!(ready.labels.get("tier"), Some(&"backend".to_string()));
        assert_eq!(ready.annotations.get("owner"), Some(&"tests".to_string()));

        let removed = sandbox_summary(test_sandbox(SandboxState::Removed));
        assert_eq!(
            PodSandboxState::try_from(removed.state).unwrap(),
            PodSandboxState::SandboxNotready
        );
    }

    #[test]
    fn test_stop_timeout_helpers_and_container_event_response() {
        assert_eq!(stop_container_timeout_ms(0), None);
        assert_eq!(stop_container_timeout_ms(-10), None);
        assert_eq!(stop_container_timeout_ms(5), Some(5_000));
        assert_eq!(stop_container_timeout_ms(i64::MAX), Some(u64::MAX));

        assert_eq!(
            stop_container_wait_duration(0),
            tokio::time::Duration::from_secs(DEFAULT_STOP_CONTAINER_WAIT_SECS)
        );
        assert_eq!(
            stop_container_wait_duration(7),
            tokio::time::Duration::from_secs(7)
        );

        let event = container_event_response(
            "c1",
            "sb1",
            ContainerEventType::ContainerStoppedEvent,
            123,
            "Stopped",
            "done",
        );
        assert_eq!(event.container_id, "c1");
        assert_eq!(event.pod_sandbox_id, "sb1");
        assert_eq!(
            event.container_event_type,
            ContainerEventType::ContainerStoppedEvent as i32
        );
        assert_eq!(event.created_at, 123);
        assert_eq!(event.reason, "Stopped");
        assert_eq!(event.message, "done");
    }

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
