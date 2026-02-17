//! Shared cleanup utilities for box resource teardown.

/// Detach named volumes and disconnect from network for a box.
pub fn cleanup_box_resources(
    box_id: &str,
    volume_names: &[String],
    network_name: Option<&str>,
) {
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
