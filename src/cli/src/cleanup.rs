//! Shared cleanup utilities for box resource teardown.

use std::path::Path;

use crate::state::{BoxRecord, StateFile};

pub(crate) fn record_network_name(record: &BoxRecord) -> Option<&str> {
    record
        .network_name
        .as_deref()
        .or(match &record.network_mode {
            a3s_box_core::NetworkMode::Bridge { network } => Some(network.as_str()),
            _ => None,
        })
}

/// Detach named volumes and disconnect from network for a box.
pub fn cleanup_box_resources(box_id: &str, volume_names: &[String], network_name: Option<&str>) {
    // Detach named volumes
    super::commands::volume::detach_volumes(volume_names, box_id);

    // Disconnect from network if connected. Release the endpoint under the
    // store's cross-process lock with a fresh read, so a concurrent connect to
    // the same network is not lost (a get → disconnect → update reads outside
    // the lock and would clobber it).
    if let Some(net_name) = network_name {
        if let Ok(net_store) = a3s_box_runtime::NetworkStore::default_path() {
            let _ = net_store.with_write_lock(
                |networks| -> Result<(), a3s_box_core::error::BoxError> {
                    if let Some(net_config) = networks.get_mut(net_name) {
                        net_config.disconnect(box_id).ok();
                    }
                    Ok(())
                },
            );
        }
    }
}

/// Detach named volumes and disconnect the persisted network for a box record.
pub fn cleanup_record_resources(record: &BoxRecord) {
    cleanup_box_resources(
        &record.id,
        &record.volume_names,
        record_network_name(record),
    );
}

/// Remove the box's host cgroup `/sys/fs/cgroup/a3s-box/<id>`. The shim creates
/// it for `--cpu-shares`/`--cpu-quota`/`--memory-reservation`/`--memory-swap`
/// and — taking over the process via libkrun — can never remove it; an empty-dir
/// `rmdir` on cgroupfs removes the cgroup once the shim PID is gone. Best-effort:
/// absent (no host cgroup limits were set) or non-empty is fine.
pub(crate) fn remove_host_cgroup(box_id: &str) {
    #[cfg(target_os = "linux")]
    {
        let _ = std::fs::remove_dir(format!("/sys/fs/cgroup/a3s-box/{box_id}"));
    }
    #[cfg(not(target_os = "linux"))]
    let _ = box_id;
}

/// Remove transient host resources for a stopped box while keeping its state.
pub fn cleanup_stopped_box(record: &BoxRecord) {
    cleanup_record_resources(record);
    // Release the overlayfs mount so a stopped box never leaves a live mount
    // (and a later restart re-mounts cleanly instead of stacking).
    a3s_box_runtime::rootfs::unmount_box_overlay(&record.box_dir.join("merged"));
    cleanup_external_socket_dir(&record.box_dir, &record.exec_socket_path);
    remove_host_cgroup(&record.id);
}

/// Remove anonymous volumes created from OCI `VOLUME` declarations.
pub fn cleanup_anonymous_volumes(anonymous_volumes: &[String]) {
    if anonymous_volumes.is_empty() {
        return;
    }

    if let Ok(vol_store) = a3s_box_runtime::VolumeStore::default_path() {
        for volume_name in anonymous_volumes {
            if let Err(err) = vol_store.remove(volume_name, true) {
                tracing::debug!(
                    volume = volume_name,
                    error = %err,
                    "Failed to remove anonymous volume"
                );
            }
        }
    }
}

/// Remove the host-side socket directory when it lives outside the box dir.
pub fn cleanup_external_socket_dir(box_dir: &Path, exec_socket_path: &Path) {
    let Some(socket_dir) = exec_socket_path.parent() else {
        return;
    };
    // Reap the box's passt daemon (Linux bridge mode). passt outlives the
    // process that launched it, so box teardown terminates it via its PID file.
    #[cfg(target_os = "linux")]
    a3s_box_runtime::network::terminate_passt(socket_dir);
    if socket_dir.starts_with(box_dir) {
        return;
    }
    if let Err(err) = std::fs::remove_dir_all(socket_dir) {
        tracing::debug!(
            path = %socket_dir.display(),
            error = %err,
            "Failed to remove external socket directory"
        );
    }
}

/// Remove all host-side resources owned by a box record.
pub fn cleanup_removed_box(record: &BoxRecord) {
    cleanup_record_resources(record);
    cleanup_anonymous_volumes(&record.anonymous_volumes);
    remove_host_cgroup(&record.id);

    if record.box_dir.exists() {
        // Release the overlayfs mount FIRST: otherwise remove_dir_all deletes
        // into the live mount ("Stale file handle") and leaks it.
        a3s_box_runtime::rootfs::unmount_box_overlay(&record.box_dir.join("merged"));
        if let Err(err) = std::fs::remove_dir_all(&record.box_dir) {
            tracing::debug!(
                path = %record.box_dir.display(),
                error = %err,
                "Failed to remove box directory"
            );
        }
    }
    cleanup_external_socket_dir(&record.box_dir, &record.exec_socket_path);

    // The shim stages single-file bind mounts in $TMPDIR/a3s-fs-mount-<box_id>
    // and can never clean it up itself (it takes over the process via libkrun
    // and never returns). Remove it here on box teardown.
    let fs_mount_dir = std::env::temp_dir().join(format!("a3s-fs-mount-{}", record.id));
    if fs_mount_dir.exists() {
        let _ = std::fs::remove_dir_all(&fs_mount_dir);
    }
}

/// Roll back a box record that was partially created.
pub fn cleanup_partial_box_record(record: &BoxRecord, state: Option<&mut StateFile>) {
    cleanup_removed_box(record);

    if let Some(state) = state {
        if let Err(err) = state.remove(&record.id) {
            tracing::debug!(
                box_id = %record.id,
                error = %err,
                "Failed to remove partial box state"
            );
        }
    }
}

/// Removes a box directory tree on drop unless [`disarm`](Self::disarm)ed.
///
/// `create`/`snapshot restore` materialize the box dir (sockets/, logs/,
/// rootfs/) BEFORE the box is registered in the state file. If any step in
/// between fails with `?`, an unregistered box dir is left behind — invisible to
/// `prune`/`rm` (which only see boxes in state) and leaking on disk. Arm this
/// guard before the first fallible filesystem step and `disarm()` it the moment
/// the box is registered; any early return in between cleans the dir up.
pub(crate) struct BoxDirGuard {
    path: std::path::PathBuf,
    armed: bool,
}

impl BoxDirGuard {
    pub(crate) fn new(path: std::path::PathBuf) -> Self {
        Self { path, armed: true }
    }

    pub(crate) fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for BoxDirGuard {
    fn drop(&mut self) {
        if self.armed {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_helpers::fixtures::make_record;

    #[test]
    fn test_box_dir_guard_removes_on_drop_unless_disarmed() {
        let tmp = tempfile::tempdir().unwrap();

        // Armed (create/restore failed before registration) → box dir removed.
        let orphaned = tmp.path().join("boxes").join("failed-box");
        std::fs::create_dir_all(orphaned.join("rootfs")).unwrap();
        {
            let _guard = BoxDirGuard::new(orphaned.clone());
        }
        assert!(
            !orphaned.exists(),
            "an armed guard must remove the orphaned box dir on drop"
        );

        // Disarmed (box registered in state) → dir kept.
        let registered = tmp.path().join("boxes").join("good-box");
        std::fs::create_dir_all(registered.join("rootfs")).unwrap();
        {
            let mut guard = BoxDirGuard::new(registered.clone());
            guard.disarm();
        }
        assert!(
            registered.exists(),
            "a disarmed guard must keep the registered box dir"
        );
    }

    #[test]
    fn test_cleanup_partial_box_record_removes_state_and_box_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let state_path = tmp.path().join("boxes.json");
        let box_dir = tmp.path().join("box-dir");
        std::fs::create_dir_all(box_dir.join("sockets")).unwrap();

        let mut state = StateFile::load(&state_path).unwrap();
        let mut record = make_record("partial-id", "partial_box", "created", None);
        record.box_dir = box_dir.clone();
        record.exec_socket_path = box_dir.join("sockets").join("exec.sock");
        state.add(record.clone()).unwrap();

        cleanup_partial_box_record(&record, Some(&mut state));

        assert!(state.find_by_id("partial-id").is_none());
        assert!(!box_dir.exists());
    }
}
