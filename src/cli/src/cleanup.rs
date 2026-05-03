//! Shared cleanup utilities for box resource teardown.

use std::path::Path;

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
