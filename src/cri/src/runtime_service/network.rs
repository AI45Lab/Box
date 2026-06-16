//! Sandbox network helpers for the CRI runtime service.
//!
//! Bridge-network endpoint allocation, sandbox IP parsing, and
//! NetworkStore connect/disconnect helpers used by [`super::BoxRuntimeService`].

use std::collections::HashMap;

use tonic::Status;

use a3s_box_core::error::BoxError;
use a3s_box_core::NetworkMode;
use a3s_box_runtime::NetworkStore;

use crate::config_mapper::ANN_NETWORK;
use crate::error::box_error_to_status;
use crate::sandbox::PodSandbox;

use super::convert::{ANN_ADDITIONAL_POD_IPS, ANN_POD_IP};

pub(super) struct SandboxNetworkAllocation {
    pub(super) network_name: String,
    pub(super) ip: String,
}

pub(super) fn sandbox_network_status_from_annotations(
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

pub(super) fn bridge_network_name(config: &a3s_box_core::config::BoxConfig) -> Option<String> {
    match &config.network {
        NetworkMode::Bridge { network } if !network.trim().is_empty() => {
            Some(network.trim().to_string())
        }
        _ => None,
    }
}

pub(super) fn sandbox_network_name(sandbox: &PodSandbox) -> Option<String> {
    sandbox
        .annotations
        .get(ANN_NETWORK)
        .map(|network| network.trim())
        .filter(|network| !network.is_empty())
        .map(ToOwned::to_owned)
}

pub(super) fn connect_sandbox_to_network_store(
    store: &NetworkStore,
    network_name: &str,
    sandbox_id: &str,
    pod_name: &str,
) -> Result<SandboxNetworkAllocation, Status> {
    // Allocate the endpoint IP and persist it under the network store's
    // cross-process write lock so two pods joining the same bridge concurrently
    // cannot be handed the same IP/MAC or clobber each other's endpoint. The
    // get → connect → update sequence was previously unlocked; the CLI paths
    // already collapse this under with_write_lock (see boot.rs
    // ensure_network_connected_with_store). The inner Result preserves the
    // original not_found / failed_precondition gRPC codes (BoxError::NetworkError
    // would otherwise map to internal).
    let outcome: Result<String, Status> = store
        .with_write_lock(|networks| {
            let Some(network) = networks.get_mut(network_name) else {
                return Ok(Err(Status::not_found(format!(
                    "Network not found: {network_name}"
                ))));
            };
            match network.connect(sandbox_id, pod_name) {
                Ok(endpoint) => Ok(Ok(endpoint.ip_address.to_string())),
                Err(e) => Ok::<_, BoxError>(Err(Status::failed_precondition(format!(
                    "Failed to connect sandbox {sandbox_id} to network {network_name}: {e}"
                )))),
            }
        })
        .map_err(box_error_to_status)?;
    let ip = outcome?;

    Ok(SandboxNetworkAllocation {
        network_name: network_name.to_string(),
        ip,
    })
}

pub(super) fn disconnect_sandbox_from_network_store(
    store: &NetworkStore,
    network_name: &str,
    sandbox_id: &str,
) -> Result<(), Status> {
    // Disconnect + persist under the same write lock so a teardown can't clobber
    // a concurrent connect's endpoint write (mirrors the connect path's locking).
    store
        .with_write_lock(|networks| {
            if let Some(network) = networks.get_mut(network_name) {
                // Best-effort: a "not connected" disconnect is a no-op on teardown.
                let _ = network.disconnect(sandbox_id);
            }
            Ok::<(), BoxError>(())
        })
        .map_err(box_error_to_status)
}

pub(super) fn default_network_store() -> NetworkStore {
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

#[cfg(test)]
mod tests {
    use super::*;
    use a3s_box_core::network::NetworkConfig;
    use std::collections::HashSet;
    use std::sync::Arc;

    #[test]
    fn concurrent_sandbox_connects_allocate_distinct_ips() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(NetworkStore::new(dir.path().join("networks.json")));
        store
            .create(NetworkConfig::new("dev", "10.88.0.0/24").unwrap())
            .unwrap();

        // kubelet drives RunPodSandbox concurrently, so many sandboxes join the
        // same bridge at once. The previously-unlocked get → connect → update
        // handed two pods the SAME IP/MAC and dropped an endpoint; allocating
        // under with_write_lock makes every assignment atomic and distinct.
        let handles: Vec<_> = (0..16)
            .map(|i| {
                let store = Arc::clone(&store);
                std::thread::spawn(move || {
                    connect_sandbox_to_network_store(
                        &store,
                        "dev",
                        &format!("sb-{i}"),
                        &format!("pod-{i}"),
                    )
                    .unwrap()
                    .ip
                })
            })
            .collect();

        let ips: HashSet<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        assert_eq!(
            ips.len(),
            16,
            "concurrent sandbox connects must allocate distinct IPs (no lost update): {ips:?}"
        );
        assert_eq!(
            store.get("dev").unwrap().unwrap().endpoints.len(),
            16,
            "every concurrent sandbox endpoint must be persisted"
        );
    }
}
