//! Shared cleanup utilities for box resource teardown.

use std::path::Path;

use crate::state::{BoxRecord, StateFile};

pub(crate) fn record_network_name(record: &BoxRecord) -> Option<&str> {
    record
        .network_name
        .as_deref()
        .or_else(|| match &record.network_mode {
            a3s_box_core::NetworkMode::Bridge { network } => Some(network.as_str()),
            _ => None,
        })
}

/// Detach named volumes and disconnect from network for a box.
pub fn cleanup_box_resources(box_id: &str, volume_names: &[String], network_name: Option<&str>) {
    // Detach named volumes
    super::commands::volume::detach_volumes(volume_names, box_id);

    // Disconnect from network if connected
    if let Some(net_name) = network_name {
        if let Ok(net_store) = a3s_box_runtime::NetworkStore::default_path() {
            if let Ok(Some(mut net_config)) = net_store.get(net_name) {
                net_config.disconnect(box_id).ok();
                net_store.update(&net_config).ok();
            }
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

/// Remove transient host resources for a stopped box while keeping its state.
pub fn cleanup_stopped_box(record: &BoxRecord) {
    cleanup_record_resources(record);
    cleanup_external_socket_dir(&record.box_dir, &record.exec_socket_path);
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

    if record.box_dir.exists() {
        if let Err(err) = std::fs::remove_dir_all(&record.box_dir) {
            tracing::debug!(
                path = %record.box_dir.display(),
                error = %err,
                "Failed to remove box directory"
            );
        }
    }
    cleanup_external_socket_dir(&record.box_dir, &record.exec_socket_path);
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_helpers::fixtures::make_record;

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
