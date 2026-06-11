//! Internal sandbox/container helper methods for [`super::BoxRuntimeService`].
//!
//! These inherent methods back the [`super`] CRI trait implementation:
//! network attach/detach, container rootfs preparation and cleanup, VM
//! acquisition/teardown, and workload stop handling. Split out of `mod.rs`
//! to keep the trait implementation readable.

use super::*;

/// Mount points at or under `root`, parsed from `/proc/self/mountinfo` content
/// (space-separated; field index 4 is the mount point). Returned deepest-first
/// so a parent unmount does not `EBUSY` on a child. Pure (string in/out) for
/// testing; managed CRI rootfs paths contain no whitespace, so mountinfo's
/// octal escaping needs no decoding here.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn submounts_under(mountinfo: &str, root: &str) -> Vec<String> {
    let root = root.trim_end_matches('/');
    let with_slash = format!("{root}/");
    let mut mps: Vec<String> = mountinfo
        .lines()
        .filter_map(|line| line.split(' ').nth(4))
        .filter(|mp| *mp == root || mp.starts_with(&with_slash))
        .map(|mp| mp.to_string())
        .collect();
    mps.sort_by_key(|p| std::cmp::Reverse(p.matches('/').count()));
    mps.dedup();
    mps
}

/// Lazy-unmount every bind/submount at or under `root` before the rootfs is
/// removed. CRI mounts are bind-mounted into the container rootfs (which is
/// virtio-fs-shared into the pod VM), so `remove_dir_all` over a live bind would
/// delete the host source through it. `umount -l` (MNT_DETACH) succeeds even if
/// the mount is still busy.
#[cfg(target_os = "linux")]
fn unmount_submounts_under(root: &std::path::Path) {
    let mountinfo = match std::fs::read_to_string("/proc/self/mountinfo") {
        Ok(content) => content,
        Err(_) => return,
    };
    for mp in submounts_under(&mountinfo, &root.to_string_lossy()) {
        if let Err(error) = std::process::Command::new("umount")
            .arg("-l")
            .arg(&mp)
            .status()
        {
            tracing::warn!(
                mount = %mp,
                error = %error,
                "Failed to lazy-unmount CRI bind before rootfs cleanup"
            );
        }
    }
}

#[cfg(not(target_os = "linux"))]
fn unmount_submounts_under(_root: &std::path::Path) {}

impl BoxRuntimeService {
    pub(super) async fn connect_sandbox_network(
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

    pub(super) async fn disconnect_sandbox_network_by_name(
        &self,
        network_name: &str,
        sandbox_id: &str,
    ) {
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

    pub(super) async fn disconnect_sandbox_network(&self, sandbox: &PodSandbox) {
        if let Some(network_name) = sandbox_network_name(sandbox) {
            self.disconnect_sandbox_network_by_name(&network_name, &sandbox.id)
                .await;
        }
    }

    pub(super) async fn resolve_container_image(
        &self,
        image_ref: &str,
    ) -> Result<Option<ResolvedContainerImage>, Status> {
        if image_ref.is_empty() {
            return Ok(None);
        }

        let Some(stored) = self.image_store.resolve(image_ref).await else {
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

    pub(super) fn container_rootfs_base(&self) -> PathBuf {
        self.image_store
            .store_dir()
            .join(CRI_CONTAINER_ROOTFS_HOST_DIR)
    }

    pub(super) fn container_rootfs_paths(
        &self,
        sandbox_id: &str,
        container_id: &str,
    ) -> ContainerRootfsPaths {
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

    pub(super) async fn ensure_container_rootfs_mount_base(&self) -> Result<PathBuf, Status> {
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

    pub(super) async fn prepare_container_rootfs(
        &self,
        image: &ResolvedContainerImage,
        paths: &ContainerRootfsPaths,
        resolv_conf: String,
    ) -> Result<(), Status> {
        let image_path = PathBuf::from(&image.path);
        let rootfs_path = paths.host_path.clone();

        tokio::task::spawn_blocking(move || {
            OciRootfsBuilder::new(&rootfs_path)
                .with_image(&image_path)
                .with_resolv_conf(resolv_conf)
                .build()
        })
        .await
        .map_err(|e| Status::internal(format!("Container rootfs build task failed: {e}")))?
        .map_err(|e| {
            Status::failed_precondition(format!("Failed to prepare container rootfs: {e}"))
        })
    }

    pub(super) async fn materialize_container_mounts(
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
                materialize_container_mount(&rootfs, mount)?;
            }
            Ok::<(), Status>(())
        })
        .await
        .map_err(|e| Status::internal(format!("CRI mount materialization task failed: {e}")))?
    }

    pub(super) async fn cleanup_container_rootfs_path(&self, rootfs_path: &str) {
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

        // Lazy-unmount any CRI bind-mounts under the rootfs FIRST; otherwise
        // remove_dir_all would recurse through a live bind and delete the host
        // source. Safe no-op when there are none (the copy/test build path).
        let unmount_path = rootfs_path.clone();
        let _ = tokio::task::spawn_blocking(move || unmount_submounts_under(&unmount_path)).await;

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

    pub(super) async fn cleanup_sandbox_rootfs(&self, sandbox_id: &str) {
        let path = self
            .container_rootfs_base()
            .join(sanitize_path_component(sandbox_id));

        // Lazy-unmount any CRI bind-mounts under the sandbox's container rootfs
        // tree before removing it (see cleanup_container_rootfs_path). This also
        // reclaims binds leaked by a previously crashed CRI on restart.
        let unmount_path = path.clone();
        let _ = tokio::task::spawn_blocking(move || unmount_submounts_under(&unmount_path)).await;

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

    pub(super) async fn stop_container_vm(
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

    pub(super) async fn stop_container_workload(
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

    pub(super) async fn has_other_running_containers(&self, container: &Container) -> bool {
        self.store
            .containers
            .list(Some(&container.sandbox_id), None)
            .await
            .into_iter()
            .any(|other| other.id != container.id && other.state == ContainerState::Running)
    }

    pub(super) fn emit_container_event(
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

    pub(super) async fn acquire_vm_with_box_id(
        &self,
        box_config: a3s_box_core::config::BoxConfig,
        box_id: String,
    ) -> Result<VmManager, Status> {
        self.acquire_vm_inner(box_config, Some(box_id)).await
    }

    pub(super) async fn acquire_vm_inner(
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

    pub(super) async fn destroy_sandbox_vm(
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

#[cfg(test)]
mod cleanup_tests {
    use super::submounts_under;

    #[test]
    fn test_submounts_under_filters_and_orders_deepest_first() {
        // /proc/self/mountinfo lines; field index 4 is the mount point.
        let mountinfo = "\
24 30 0:22 / /proc rw shared:5 - proc proc rw
60 30 0:46 / /root/.a3s/images/cri-container-rootfs/sb/ctr/rootfs/data rw - ext4 /dev/sda1 rw
61 30 0:47 / /root/.a3s/images/cri-container-rootfs/sb/ctr/rootfs/data/deep rw - ext4 /dev/sda1 rw
62 30 0:48 / /root/.a3s/images/cri-container-rootfs/other/x rw - ext4 /dev/sda1 rw
";
        let got = submounts_under(
            mountinfo,
            "/root/.a3s/images/cri-container-rootfs/sb/ctr/rootfs",
        );
        assert_eq!(
            got,
            vec![
                "/root/.a3s/images/cri-container-rootfs/sb/ctr/rootfs/data/deep".to_string(),
                "/root/.a3s/images/cri-container-rootfs/sb/ctr/rootfs/data".to_string(),
            ]
        );
    }

    #[test]
    fn test_submounts_under_includes_root_excludes_prefix_siblings() {
        let mountinfo = "\
1 2 0:1 / /root/x rw - ext4 d rw
1 2 0:1 / /root/xy rw - ext4 d rw
";
        // Exact root match included; a sibling sharing the string prefix is not.
        assert_eq!(
            submounts_under(mountinfo, "/root/x"),
            vec!["/root/x".to_string()]
        );
    }
}
