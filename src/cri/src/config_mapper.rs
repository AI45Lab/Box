//! Map Kubernetes CRI config to A3S Box config.
//!
//! Reads A3S-specific annotations from pod/container configs:
//! - `a3s.box/image` → OCI image reference
//! - `a3s.box/vcpus`, `a3s.box/memory-mb` → ResourceConfig
//! - `a3s.box/tee` → TeeConfig

use std::collections::HashMap;

use a3s_box_core::config::{BoxConfig, ResourceConfig, TeeConfig};
use a3s_box_core::error::{BoxError, Result};

use crate::cri_api::PodSandboxConfig;

/// Annotation keys for A3S Box configuration.
const ANN_AGENT_IMAGE: &str = "a3s.box/agent-image";
const ANN_VCPUS: &str = "a3s.box/vcpus";
const ANN_MEMORY_MB: &str = "a3s.box/memory-mb";
const ANN_DISK_MB: &str = "a3s.box/disk-mb";
const ANN_TEE: &str = "a3s.box/tee";
const ANN_TEE_WORKLOAD_ID: &str = "a3s.box/tee-workload-id";

/// Convert a CRI PodSandboxConfig to an A3S BoxConfig.
pub fn pod_sandbox_config_to_box_config(config: &PodSandboxConfig) -> Result<BoxConfig> {
    let annotations = &config.annotations;

    let image = annotations
        .get(ANN_AGENT_IMAGE)
        .cloned()
        .ok_or_else(|| {
            BoxError::ConfigError(format!(
                "Annotation '{}' is required",
                ANN_AGENT_IMAGE
            ))
        })?;

    let resources = parse_resources(annotations);
    let tee = parse_tee_config(annotations)?;

    Ok(BoxConfig {
        image,
        resources,
        tee,
        ..Default::default()
    })
}

/// Parse resource configuration from annotations.
fn parse_resources(annotations: &HashMap<String, String>) -> ResourceConfig {
    let vcpus = annotations
        .get(ANN_VCPUS)
        .and_then(|v| v.parse::<u32>().ok())
        .unwrap_or(2);

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
        Some("none") | None => Ok(TeeConfig::None),
        Some(other) => Err(BoxError::ConfigError(format!(
            "Unknown TEE type: '{}'. Expected: none, sev-snp",
            other
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn test_missing_image_annotation() {
        let config = make_config(HashMap::new());
        assert!(pod_sandbox_config_to_box_config(&config).is_err());
    }

    #[test]
    fn test_oci_image() {
        let annotations = HashMap::from([(
            ANN_AGENT_IMAGE.to_string(),
            "ghcr.io/a3s-box/code:v0.1.0".to_string(),
        )]);
        let config = make_config(annotations);
        let box_config = pod_sandbox_config_to_box_config(&config).unwrap();
        assert_eq!(box_config.image, "ghcr.io/a3s-box/code:v0.1.0");
    }

    #[test]
    fn test_custom_resources() {
        let annotations = HashMap::from([
            (ANN_AGENT_IMAGE.to_string(), "alpine:latest".to_string()),
            (ANN_VCPUS.to_string(), "4".to_string()),
            (ANN_MEMORY_MB.to_string(), "2048".to_string()),
        ]);
        let config = make_config(annotations);
        let box_config = pod_sandbox_config_to_box_config(&config).unwrap();

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
        let box_config = pod_sandbox_config_to_box_config(&config).unwrap();

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
        assert!(pod_sandbox_config_to_box_config(&config).is_err());
    }
}
