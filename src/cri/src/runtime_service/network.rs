//! Sandbox network helpers for the CRI runtime service.
//!
//! Bridge-network endpoint allocation, sandbox IP parsing, and
//! NetworkStore connect/disconnect helpers used by [`super::BoxRuntimeService`].

use std::collections::HashMap;

use tonic::Status;

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

pub(super) fn disconnect_sandbox_from_network_store(
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
