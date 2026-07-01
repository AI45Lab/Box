//! Map Kubernetes CRI config to A3S Box config.
//!
//! Reads A3S-specific annotations from pod/container configs:
//! - `a3s.box/agent-image` → optional sandbox VM agent/rootfs image override
//! - `a3s.box/vcpus`, `a3s.box/memory-mb` → ResourceConfig
//! - `a3s.box/tee` → TeeConfig

use std::collections::HashMap;

use a3s_box_core::error::{BoxError, Result};
use a3s_box_core::{
    config::{BoxConfig, ResourceConfig, TeeConfig},
    NetworkMode,
};

use crate::cri_api::{port_mapping, PodSandboxConfig};

/// Annotation keys for A3S Box configuration.
pub const ANN_AGENT_IMAGE: &str = "a3s.box/agent-image";
pub const ANN_NETWORK: &str = "a3s.box/network";
pub const DEFAULT_AGENT_IMAGE: &str = "ghcr.io/a3s-box/code:v0.1.0";
const ANN_VCPUS: &str = "a3s.box/vcpus";
const ANN_MEMORY_MB: &str = "a3s.box/memory-mb";
const ANN_DISK_MB: &str = "a3s.box/disk-mb";
const ANN_TEE: &str = "a3s.box/tee";
const ANN_TEE_WORKLOAD_ID: &str = "a3s.box/tee-workload-id";

/// Convert a CRI PodSandboxConfig to an A3S BoxConfig.
pub fn pod_sandbox_config_to_box_config(
    config: &PodSandboxConfig,
    default_agent_image: &str,
) -> Result<BoxConfig> {
    let annotations = &config.annotations;
    let image = resolve_agent_image(annotations, default_agent_image)?;

    let resources = parse_resources(annotations);
    let tee = parse_tee_config(annotations)?;
    let port_map = parse_port_mappings(config)?;
    let network = parse_network_mode(annotations)?;
    let hostname = parse_hostname(config)?;

    Ok(BoxConfig {
        image,
        resources,
        tee,
        port_map,
        network,
        hostname,
        sysctls: parse_sysctls(config),
        ..Default::default()
    })
}

/// Extract pod-level sysctls from the CRI sandbox config.
///
/// Sorted by name for deterministic ordering (the guest applies them in order).
fn parse_sysctls(config: &PodSandboxConfig) -> Vec<(String, String)> {
    let Some(linux) = config.linux.as_ref() else {
        return Vec::new();
    };
    let mut sysctls: Vec<(String, String)> = linux
        .sysctls
        .iter()
        .filter(|(name, _)| {
            let safe = is_safe_sysctl_name(name);
            if !safe {
                tracing::warn!(sysctl = %name, "Dropping sysctl with an unsafe name");
            }
            safe
        })
        .map(|(name, value)| (name.clone(), value.clone()))
        .collect();
    sysctls.sort();
    sysctls
}

/// A sysctl name is safe to map onto `/proc/sys/<name with '.'→'/'>` only if it
/// is a non-empty dot-separated key with no path-traversal characters. Guards
/// against a crafted name (e.g. `../../proc/sysrq-trigger`) escaping
/// `/proc/sys` when the guest substitutes `.` for `/`.
fn is_safe_sysctl_name(name: &str) -> bool {
    !name.is_empty()
        && !name.contains('/')
        && !name.split('.').any(|seg| seg.is_empty() || seg == "..")
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

fn parse_hostname(config: &PodSandboxConfig) -> Result<Option<String>> {
    let hostname = config.hostname.trim();
    if hostname.is_empty() {
        return Ok(None);
    }
    a3s_box_core::dns::validate_hostname(hostname)
        .map_err(|e| BoxError::ConfigError(format!("Invalid CRI sandbox hostname: {e}")))?;
    Ok(Some(hostname.to_string()))
}

fn resolve_agent_image(
    annotations: &HashMap<String, String>,
    default_agent_image: &str,
) -> Result<String> {
    if let Some(image) = annotations
        .get(ANN_AGENT_IMAGE)
        .map(|image| image.trim())
        .filter(|image| !image.is_empty())
    {
        return Ok(image.to_string());
    }

    let default_agent_image = default_agent_image.trim();
    if default_agent_image.is_empty() {
        return Err(BoxError::ConfigError(format!(
            "No CRI agent image configured; set runtime default agent image or annotation '{}'",
            ANN_AGENT_IMAGE
        )));
    }

    Ok(default_agent_image.to_string())
}

/// Parse resource configuration from annotations.
fn parse_resources(annotations: &HashMap<String, String>) -> ResourceConfig {
    // Clamp to the VM spec's valid vCPU range. Downstream the count narrows to a
    // u8, so an out-of-range annotation (e.g. 256) would wrap to 0 vCPUs and
    // silently fail to boot; clamp to 1..=255 instead.
    let vcpus = annotations
        .get(ANN_VCPUS)
        .and_then(|v| v.parse::<u32>().ok())
        .unwrap_or(2)
        .clamp(1, 255);

    let memory_mb = annotations
        .get(ANN_MEMORY_MB)
        .and_then(|v| v.parse::<u32>().ok())
        .unwrap_or(1024);

    let disk_mb = annotations
        .get(ANN_DISK_MB)
        .and_then(|v| v.parse::<u32>().ok())
        .unwrap_or(4096);

    ResourceConfig {
        vcpus,
        memory_mb,
        disk_mb,
        ..Default::default()
    }
}

/// Parse TEE configuration from annotations.
fn parse_tee_config(annotations: &HashMap<String, String>) -> Result<TeeConfig> {
    match annotations.get(ANN_TEE).map(|s| s.as_str()) {
        Some("sev-snp") => {
            let workload_id = annotations
                .get(ANN_TEE_WORKLOAD_ID)
                .cloned()
                .unwrap_or_else(|| "default".to_string());
            Ok(TeeConfig::SevSnp {
                workload_id,
                generation: Default::default(),
                simulate: false,
            })
        }
        Some("tdx") => {
            let workload_id = annotations
                .get(ANN_TEE_WORKLOAD_ID)
                .cloned()
                .unwrap_or_else(|| "default".to_string());
            Ok(TeeConfig::Tdx {
                workload_id,
                simulate: false,
            })
        }
        Some("none") | None => Ok(TeeConfig::None),
        Some(other) => Err(BoxError::ConfigError(format!(
            "Unknown TEE type: '{}'. Expected: none, sev-snp, tdx",
            other
        ))),
    }
}

fn parse_network_mode(annotations: &HashMap<String, String>) -> Result<NetworkMode> {
    let Some(network) = annotations.get(ANN_NETWORK).map(|network| network.trim()) else {
        return Ok(NetworkMode::Tsi);
    };

    if network.is_empty() {
        return Ok(NetworkMode::Tsi);
    }

    if network.contains('/') || network.contains('\0') {
        return Err(BoxError::ConfigError(format!(
            "Invalid CRI network annotation '{}': network names must not contain '/' or NUL",
            ANN_NETWORK
        )));
    }

    Ok(NetworkMode::Bridge {
        network: network.to_string(),
    })
}

fn parse_port_mappings(config: &PodSandboxConfig) -> Result<Vec<String>> {
    let mut port_map = Vec::with_capacity(config.port_mappings.len());

    for mapping in &config.port_mappings {
        let protocol = port_mapping::Protocol::try_from(mapping.protocol).map_err(|_| {
            BoxError::ConfigError(format!(
                "Unknown CRI port mapping protocol value {} for container port {}",
                mapping.protocol, mapping.container_port
            ))
        })?;
        if protocol != port_mapping::Protocol::Tcp {
            return Err(BoxError::ConfigError(format!(
                "Unsupported CRI port mapping protocol {} for container port {}; only TCP is supported",
                protocol.as_str_name(),
                mapping.container_port
            )));
        }

        if !(1..=u16::MAX as i32).contains(&mapping.container_port) {
            return Err(BoxError::ConfigError(format!(
                "Invalid CRI container port {}; expected 1..=65535",
                mapping.container_port
            )));
        }

        if !(0..=u16::MAX as i32).contains(&mapping.host_port) {
            return Err(BoxError::ConfigError(format!(
                "Invalid CRI host port {}; expected 0..=65535",
                mapping.host_port
            )));
        }

        let host_ip = mapping.host_ip.trim();
        if !host_ip.is_empty() && host_ip != "0.0.0.0" && host_ip != "::" {
            return Err(BoxError::ConfigError(format!(
                "Unsupported CRI host_ip '{}' for port mapping {}:{}; bind-specific host IPs are not supported",
                host_ip, mapping.host_port, mapping.container_port
            )));
        }

        // A port mapping with only a container port (host_port == 0) publishes
        // the container port on the same host port (Docker/containerd style), so
        // the pod's port becomes reachable at the node — TSI binds 0.0.0.0:<port>
        // and forwards to the guest. Without this the entry is dropped and the
        // port is never published.
        let host_port = if mapping.host_port == 0 {
            mapping.container_port
        } else {
            mapping.host_port
        };
        port_map.push(format!("{}:{}", host_port, mapping.container_port));
    }

    Ok(port_map)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cri_api::PortMapping;

    fn make_config(annotations: HashMap<String, String>) -> PodSandboxConfig {
        PodSandboxConfig {
            metadata: None,
            hostname: String::new(),
            log_directory: "/tmp/logs".to_string(),
            dns_config: None,
            port_mappings: vec![],
            labels: HashMap::new(),
            annotations,
            linux: None,
        }
    }

    #[test]
    fn test_missing_image_annotation_uses_default_agent_image() {
        let config = make_config(HashMap::new());
        let box_config = pod_sandbox_config_to_box_config(&config, DEFAULT_AGENT_IMAGE).unwrap();
        assert_eq!(box_config.image, DEFAULT_AGENT_IMAGE);
    }

    #[test]
    fn test_sysctls_extracted_and_sorted() {
        use crate::cri_api::LinuxPodSandboxConfig;
        let mut config = make_config(HashMap::new());
        config.linux = Some(LinuxPodSandboxConfig {
            sysctls: HashMap::from([
                (
                    "net.ipv4.ip_local_port_range".to_string(),
                    "1024 65000".to_string(),
                ),
                ("kernel.shm_rmid_forced".to_string(), "1".to_string()),
            ]),
            ..Default::default()
        });
        let box_config = pod_sandbox_config_to_box_config(&config, DEFAULT_AGENT_IMAGE).unwrap();
        assert_eq!(
            box_config.sysctls,
            vec![
                ("kernel.shm_rmid_forced".to_string(), "1".to_string()),
                (
                    "net.ipv4.ip_local_port_range".to_string(),
                    "1024 65000".to_string()
                ),
            ]
        );
    }

    #[test]
    fn test_sysctls_empty_without_linux_config() {
        let config = make_config(HashMap::new());
        let box_config = pod_sandbox_config_to_box_config(&config, DEFAULT_AGENT_IMAGE).unwrap();
        assert!(box_config.sysctls.is_empty());
    }

    #[test]
    fn test_empty_default_agent_image_without_annotation_is_rejected() {
        let config = make_config(HashMap::new());
        assert!(pod_sandbox_config_to_box_config(&config, "").is_err());
    }

    #[test]
    fn test_annotation_overrides_default_agent_image() {
        let annotations = HashMap::from([(
            ANN_AGENT_IMAGE.to_string(),
            "ghcr.io/a3s-box/code:v0.1.0".to_string(),
        )]);
        let config = make_config(annotations);
        let box_config =
            pod_sandbox_config_to_box_config(&config, "ghcr.io/a3s-box/default:v1").unwrap();
        assert_eq!(box_config.image, "ghcr.io/a3s-box/code:v0.1.0");
    }

    #[test]
    fn test_network_annotation_sets_bridge_network() {
        let annotations = HashMap::from([(ANN_NETWORK.to_string(), "cri-net".to_string())]);
        let config = make_config(annotations);

        let box_config = pod_sandbox_config_to_box_config(&config, DEFAULT_AGENT_IMAGE).unwrap();

        assert!(matches!(
            box_config.network,
            NetworkMode::Bridge { ref network } if network == "cri-net"
        ));
    }

    #[test]
    fn test_sandbox_hostname_sets_box_hostname() {
        let mut config = make_config(HashMap::new());
        config.hostname = "pod-web".to_string();

        let box_config = pod_sandbox_config_to_box_config(&config, DEFAULT_AGENT_IMAGE).unwrap();

        assert_eq!(box_config.hostname.as_deref(), Some("pod-web"));
    }

    #[test]
    fn test_invalid_sandbox_hostname_is_rejected() {
        let mut config = make_config(HashMap::new());
        config.hostname = "bad_host".to_string();

        let err = pod_sandbox_config_to_box_config(&config, DEFAULT_AGENT_IMAGE).unwrap_err();

        assert!(err.to_string().contains("Invalid CRI sandbox hostname"));
    }

    #[test]
    fn test_invalid_network_annotation_is_rejected() {
        let annotations = HashMap::from([(ANN_NETWORK.to_string(), "bad/name".to_string())]);
        let config = make_config(annotations);

        let err = pod_sandbox_config_to_box_config(&config, DEFAULT_AGENT_IMAGE).unwrap_err();

        assert!(err.to_string().contains("Invalid CRI network annotation"));
    }

    #[test]
    fn test_port_mappings_become_box_port_map() {
        let mut config = make_config(HashMap::new());
        config.port_mappings = vec![
            PortMapping {
                protocol: port_mapping::Protocol::Tcp.into(),
                container_port: 80,
                host_port: 8080,
                host_ip: String::new(),
            },
            PortMapping {
                protocol: port_mapping::Protocol::Tcp.into(),
                container_port: 8080,
                host_port: 0,
                host_ip: "0.0.0.0".to_string(),
            },
        ];

        let box_config = pod_sandbox_config_to_box_config(&config, DEFAULT_AGENT_IMAGE).unwrap();

        // host_port == 0 publishes the container port on the same host port.
        assert_eq!(box_config.port_map, vec!["8080:80", "8080:8080"]);
    }

    #[test]
    fn test_udp_port_mapping_is_rejected() {
        let mut config = make_config(HashMap::new());
        config.port_mappings = vec![PortMapping {
            protocol: port_mapping::Protocol::Udp.into(),
            container_port: 53,
            host_port: 5353,
            host_ip: String::new(),
        }];

        let err = pod_sandbox_config_to_box_config(&config, DEFAULT_AGENT_IMAGE).unwrap_err();

        assert!(err.to_string().contains("only TCP is supported"));
    }

    #[test]
    fn test_bind_specific_host_ip_is_rejected() {
        let mut config = make_config(HashMap::new());
        config.port_mappings = vec![PortMapping {
            protocol: port_mapping::Protocol::Tcp.into(),
            container_port: 80,
            host_port: 8080,
            host_ip: "127.0.0.1".to_string(),
        }];

        let err = pod_sandbox_config_to_box_config(&config, DEFAULT_AGENT_IMAGE).unwrap_err();

        assert!(err.to_string().contains("host_ip"));
    }

    #[test]
    fn test_invalid_port_mapping_port_is_rejected() {
        let mut config = make_config(HashMap::new());
        config.port_mappings = vec![PortMapping {
            protocol: port_mapping::Protocol::Tcp.into(),
            container_port: 0,
            host_port: 8080,
            host_ip: String::new(),
        }];

        let err = pod_sandbox_config_to_box_config(&config, DEFAULT_AGENT_IMAGE).unwrap_err();

        assert!(err.to_string().contains("Invalid CRI container port"));
    }

    #[test]
    fn test_custom_resources() {
        let annotations = HashMap::from([
            (ANN_AGENT_IMAGE.to_string(), "alpine:latest".to_string()),
            (ANN_VCPUS.to_string(), "4".to_string()),
            (ANN_MEMORY_MB.to_string(), "2048".to_string()),
        ]);
        let config = make_config(annotations);
        let box_config = pod_sandbox_config_to_box_config(&config, DEFAULT_AGENT_IMAGE).unwrap();

        assert_eq!(box_config.resources.vcpus, 4);
        assert_eq!(box_config.resources.memory_mb, 2048);
    }

    #[test]
    fn test_tee_sev_snp() {
        let annotations = HashMap::from([
            (ANN_AGENT_IMAGE.to_string(), "alpine:latest".to_string()),
            (ANN_TEE.to_string(), "sev-snp".to_string()),
            (ANN_TEE_WORKLOAD_ID.to_string(), "my-workload".to_string()),
        ]);
        let config = make_config(annotations);
        let box_config = pod_sandbox_config_to_box_config(&config, DEFAULT_AGENT_IMAGE).unwrap();

        match box_config.tee {
            TeeConfig::SevSnp { workload_id, .. } => {
                assert_eq!(workload_id, "my-workload");
            }
            _ => panic!("Expected SevSnp"),
        }
    }

    #[test]
    fn test_unknown_tee_type() {
        let annotations = HashMap::from([
            (ANN_AGENT_IMAGE.to_string(), "alpine:latest".to_string()),
            (ANN_TEE.to_string(), "unknown".to_string()),
        ]);
        let config = make_config(annotations);
        assert!(pod_sandbox_config_to_box_config(&config, DEFAULT_AGENT_IMAGE).is_err());
    }
}
