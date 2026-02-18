//! Compose orchestrator for multi-container workloads.
//!
//! Coordinates the lifecycle of multiple services defined in a compose file:
//! network creation, dependency-ordered boot, and grouped teardown.

use std::collections::HashMap;

use a3s_box_core::compose::ComposeConfig;
use a3s_box_core::config::{AgentType, BoxConfig, ResourceConfig};
use a3s_box_core::error::{BoxError, Result};
use a3s_box_core::network::NetworkMode;

/// State of a compose project (group of services).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProjectState {
    /// All services are running.
    Running,
    /// Some services are running.
    Partial,
    /// All services are stopped.
    Stopped,
}

/// A running compose project.
#[derive(Debug, Clone)]
pub struct ComposeProject {
    /// Project name (derived from directory name or --project-name).
    pub name: String,
    /// The parsed compose config.
    pub config: ComposeConfig,
    /// Service boot order (topologically sorted).
    pub service_order: Vec<String>,
    /// Map of service name → box ID (once started).
    pub service_boxes: HashMap<String, String>,
    /// Networks created for this project.
    pub networks: Vec<String>,
}

impl ComposeProject {
    /// Create a new compose project from a config.
    ///
    /// Validates the config and computes the service boot order.
    pub fn new(name: impl Into<String>, config: ComposeConfig) -> Result<Self> {
        let name = name.into();

        // Validate: every service must have an image
        for (svc_name, svc) in &config.services {
            if svc.image.is_none() {
                return Err(BoxError::Other(format!(
                    "Service '{}' has no image specified",
                    svc_name
                )));
            }
        }

        // Compute topological order
        let service_order = config
            .service_order()
            .map_err(|e| BoxError::Other(format!("Invalid compose config: {}", e)))?;

        Ok(Self {
            name,
            config,
            service_order,
            service_boxes: HashMap::new(),
            networks: Vec::new(),
        })
    }

    /// Get the project state based on which services have box IDs.
    pub fn state(&self) -> ProjectState {
        if self.service_boxes.is_empty() {
            ProjectState::Stopped
        } else if self.service_boxes.len() == self.config.services.len() {
            ProjectState::Running
        } else {
            ProjectState::Partial
        }
    }

    /// Build a BoxConfig for a single service.
    ///
    /// Translates compose service fields into the BoxConfig used by VmManager.
    pub fn build_box_config(
        &self,
        service_name: &str,
        default_network: Option<&str>,
    ) -> Result<BoxConfig> {
        let svc = self.config.services.get(service_name).ok_or_else(|| {
            BoxError::Other(format!(
                "Service '{}' not found in compose config",
                service_name
            ))
        })?;

        let image = svc
            .image
            .as_deref()
            .ok_or_else(|| BoxError::Other(format!("Service '{}' has no image", service_name)))?;

        // Parse memory limit
        let memory_mb = match &svc.mem_limit {
            Some(mem_str) => parse_compose_memory(mem_str)?,
            None => 512, // default
        };

        // Build environment
        let extra_env = svc.environment.to_pairs();

        // Determine network mode
        let network_mode = {
            let nets = svc.networks.names();
            if !nets.is_empty() {
                // Use the first declared network, prefixed with project name
                let net_name = format!("{}_{}", self.name, nets[0]);
                NetworkMode::Bridge { network: net_name }
            } else if let Some(default_net) = default_network {
                NetworkMode::Bridge {
                    network: default_net.to_string(),
                }
            } else {
                NetworkMode::Tsi
            }
        };

        // Build command and entrypoint
        let cmd = svc.command.as_ref().map(|c| c.to_vec()).unwrap_or_default();
        let entrypoint_override = svc.entrypoint.as_ref().and_then(|e| {
            let v = e.to_vec();
            if v.is_empty() {
                None
            } else {
                Some(v)
            }
        });

        let config = BoxConfig {
            agent: AgentType::OciRegistry {
                reference: image.to_string(),
            },
            resources: ResourceConfig {
                vcpus: svc.cpus.unwrap_or(2),
                memory_mb,
                ..Default::default()
            },
            cmd,
            entrypoint_override,
            volumes: svc.volumes.clone(),
            extra_env,
            port_map: svc.ports.clone(),
            dns: svc.dns.to_vec(),
            network: network_mode,
            tmpfs: svc.tmpfs.to_vec(),
            cap_add: svc.cap_add.clone(),
            cap_drop: svc.cap_drop.clone(),
            privileged: svc.privileged,
            ..Default::default()
        };

        Ok(config)
    }

    /// Get the network name for this project's default network.
    pub fn default_network_name(&self) -> String {
        format!("{}_default", self.name)
    }

    /// Get all network names this project needs (project-prefixed).
    pub fn required_networks(&self) -> Vec<String> {
        let mut nets = vec![self.default_network_name()];

        // Add explicitly declared networks
        for net_name in self.config.networks.keys() {
            let prefixed = format!("{}_{}", self.name, net_name);
            if !nets.contains(&prefixed) {
                nets.push(prefixed);
            }
        }

        // Add networks referenced by services
        for svc in self.config.services.values() {
            for net_name in svc.networks.names() {
                let prefixed = format!("{}_{}", self.name, net_name);
                if !nets.contains(&prefixed) {
                    nets.push(prefixed);
                }
            }
        }

        nets
    }

    /// Record that a service has been started with the given box ID.
    pub fn register_service(&mut self, service_name: &str, box_id: String) {
        self.service_boxes.insert(service_name.to_string(), box_id);
    }

    /// Remove a service's box ID (on stop/destroy).
    pub fn unregister_service(&mut self, service_name: &str) {
        self.service_boxes.remove(service_name);
    }

    /// Get the box ID for a service, if running.
    pub fn box_id(&self, service_name: &str) -> Option<&str> {
        self.service_boxes.get(service_name).map(|s| s.as_str())
    }

    /// Get the shutdown order (reverse of boot order).
    pub fn shutdown_order(&self) -> Vec<String> {
        let mut order = self.service_order.clone();
        order.reverse();
        order
    }
}

/// Parse a compose memory string (e.g., "512m", "1g", "1024") into MB.
fn parse_compose_memory(s: &str) -> Result<u32> {
    let s = s.trim().to_lowercase();
    let (num_str, multiplier) = if s.ends_with("gb") || s.ends_with('g') {
        let n = s.trim_end_matches("gb").trim_end_matches('g');
        (n, 1024u64)
    } else if s.ends_with("mb") || s.ends_with('m') {
        let n = s.trim_end_matches("mb").trim_end_matches('m');
        (n, 1u64)
    } else if s.ends_with("kb") || s.ends_with('k') {
        let n = s.trim_end_matches("kb").trim_end_matches('k');
        // KB → MB (round up)
        return n
            .parse::<u64>()
            .map(|v| v.div_ceil(1024) as u32)
            .map_err(|_| BoxError::Other(format!("Invalid memory value: {}", s)));
    } else {
        // Assume bytes
        return s
            .parse::<u64>()
            .map(|v| v.div_ceil(1024 * 1024) as u32)
            .map_err(|_| BoxError::Other(format!("Invalid memory value: {}", s)));
    };

    let num: f64 = num_str
        .parse()
        .map_err(|_| BoxError::Other(format!("Invalid memory value: {}", s)))?;

    Ok((num * multiplier as f64) as u32)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_config() -> ComposeConfig {
        let yaml = r#"
services:
  web:
    image: nginx:latest
    ports:
      - "8080:80"
    depends_on:
      - api
  api:
    image: myapi:v1
    depends_on:
      - db
    environment:
      DATABASE_URL: postgres://db:5432/app
  db:
    image: postgres:16
    volumes:
      - "pgdata:/var/lib/postgresql/data"
    mem_limit: "1g"
    cpus: 2
volumes:
  pgdata:
"#;
        ComposeConfig::from_yaml_str(yaml).unwrap()
    }

    #[test]
    fn test_compose_project_new() {
        let config = sample_config();
        let project = ComposeProject::new("myapp", config).unwrap();
        assert_eq!(project.name, "myapp");
        assert_eq!(project.service_order.len(), 3);
        // db must come before api, api before web
        let db_pos = project
            .service_order
            .iter()
            .position(|s| s == "db")
            .unwrap();
        let api_pos = project
            .service_order
            .iter()
            .position(|s| s == "api")
            .unwrap();
        let web_pos = project
            .service_order
            .iter()
            .position(|s| s == "web")
            .unwrap();
        assert!(db_pos < api_pos);
        assert!(api_pos < web_pos);
    }

    #[test]
    fn test_compose_project_state() {
        let config = sample_config();
        let mut project = ComposeProject::new("myapp", config).unwrap();
        assert_eq!(project.state(), ProjectState::Stopped);

        project.register_service("db", "box-1".to_string());
        assert_eq!(project.state(), ProjectState::Partial);

        project.register_service("api", "box-2".to_string());
        project.register_service("web", "box-3".to_string());
        assert_eq!(project.state(), ProjectState::Running);

        project.unregister_service("web");
        assert_eq!(project.state(), ProjectState::Partial);
    }

    #[test]
    fn test_compose_project_box_id() {
        let config = sample_config();
        let mut project = ComposeProject::new("myapp", config).unwrap();
        assert!(project.box_id("db").is_none());

        project.register_service("db", "box-123".to_string());
        assert_eq!(project.box_id("db"), Some("box-123"));
    }

    #[test]
    fn test_compose_project_shutdown_order() {
        let config = sample_config();
        let project = ComposeProject::new("myapp", config).unwrap();
        let shutdown = project.shutdown_order();
        // Shutdown is reverse of boot: web → api → db
        let web_pos = shutdown.iter().position(|s| s == "web").unwrap();
        let api_pos = shutdown.iter().position(|s| s == "api").unwrap();
        let db_pos = shutdown.iter().position(|s| s == "db").unwrap();
        assert!(web_pos < api_pos);
        assert!(api_pos < db_pos);
    }

    #[test]
    fn test_compose_project_default_network() {
        let config = sample_config();
        let project = ComposeProject::new("myapp", config).unwrap();
        assert_eq!(project.default_network_name(), "myapp_default");
    }

    #[test]
    fn test_compose_project_required_networks() {
        let yaml = r#"
services:
  web:
    image: nginx
    networks:
      - frontend
  api:
    image: myapi
    networks:
      - frontend
      - backend
networks:
  frontend:
  backend:
"#;
        let config = ComposeConfig::from_yaml_str(yaml).unwrap();
        let project = ComposeProject::new("myapp", config).unwrap();
        let nets = project.required_networks();
        assert!(nets.contains(&"myapp_default".to_string()));
        assert!(nets.contains(&"myapp_frontend".to_string()));
        assert!(nets.contains(&"myapp_backend".to_string()));
    }

    #[test]
    fn test_build_box_config_basic() {
        let config = sample_config();
        let project = ComposeProject::new("myapp", config).unwrap();
        let box_config = project
            .build_box_config("db", Some("myapp_default"))
            .unwrap();

        assert!(
            matches!(box_config.agent, AgentType::OciRegistry { ref reference } if reference == "postgres:16")
        );
        assert_eq!(box_config.resources.vcpus, 2);
        assert_eq!(box_config.resources.memory_mb, 1024);
        assert_eq!(box_config.volumes, vec!["pgdata:/var/lib/postgresql/data"]);
    }

    #[test]
    fn test_build_box_config_env() {
        let config = sample_config();
        let project = ComposeProject::new("myapp", config).unwrap();
        let box_config = project
            .build_box_config("api", Some("myapp_default"))
            .unwrap();

        assert!(box_config
            .extra_env
            .iter()
            .any(|(k, v)| k == "DATABASE_URL" && v == "postgres://db:5432/app"));
    }

    #[test]
    fn test_build_box_config_ports() {
        let config = sample_config();
        let project = ComposeProject::new("myapp", config).unwrap();
        let box_config = project
            .build_box_config("web", Some("myapp_default"))
            .unwrap();

        assert_eq!(box_config.port_map, vec!["8080:80"]);
    }

    #[test]
    fn test_build_box_config_network_mode() {
        let config = sample_config();
        let project = ComposeProject::new("myapp", config).unwrap();
        let box_config = project
            .build_box_config("web", Some("myapp_default"))
            .unwrap();

        assert!(matches!(
            box_config.network,
            NetworkMode::Bridge { ref network } if network == "myapp_default"
        ));
    }

    #[test]
    fn test_build_box_config_service_not_found() {
        let config = sample_config();
        let project = ComposeProject::new("myapp", config).unwrap();
        let result = project.build_box_config("nonexistent", None);
        assert!(result.is_err());
    }

    #[test]
    fn test_build_box_config_no_network() {
        let config = sample_config();
        let project = ComposeProject::new("myapp", config).unwrap();
        let box_config = project.build_box_config("web", None).unwrap();
        // No default network → falls back to Tsi
        assert!(matches!(box_config.network, NetworkMode::Tsi));
    }

    #[test]
    fn test_compose_project_no_image_error() {
        let yaml = r#"
services:
  web:
    ports:
      - "8080:80"
"#;
        let config = ComposeConfig::from_yaml_str(yaml).unwrap();
        let result = ComposeProject::new("myapp", config);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("no image"));
    }

    #[test]
    fn test_parse_compose_memory_mb() {
        assert_eq!(parse_compose_memory("512m").unwrap(), 512);
        assert_eq!(parse_compose_memory("512M").unwrap(), 512);
        assert_eq!(parse_compose_memory("512mb").unwrap(), 512);
    }

    #[test]
    fn test_parse_compose_memory_gb() {
        assert_eq!(parse_compose_memory("1g").unwrap(), 1024);
        assert_eq!(parse_compose_memory("2G").unwrap(), 2048);
        assert_eq!(parse_compose_memory("1.5g").unwrap(), 1536);
    }

    #[test]
    fn test_parse_compose_memory_bytes() {
        assert_eq!(parse_compose_memory("536870912").unwrap(), 512);
    }

    #[test]
    fn test_parse_compose_memory_invalid() {
        assert!(parse_compose_memory("abc").is_err());
    }

    #[test]
    fn test_build_box_config_with_service_network() {
        let yaml = r#"
services:
  web:
    image: nginx
    networks:
      - frontend
networks:
  frontend:
"#;
        let config = ComposeConfig::from_yaml_str(yaml).unwrap();
        let project = ComposeProject::new("myapp", config).unwrap();
        let box_config = project
            .build_box_config("web", Some("myapp_default"))
            .unwrap();

        // Service-level network takes precedence over default
        assert!(matches!(
            box_config.network,
            NetworkMode::Bridge { ref network } if network == "myapp_frontend"
        ));
    }

    #[test]
    fn test_build_box_config_privileged() {
        let yaml = r#"
services:
  web:
    image: nginx
    privileged: true
    cap_add:
      - NET_ADMIN
    cap_drop:
      - ALL
"#;
        let config = ComposeConfig::from_yaml_str(yaml).unwrap();
        let project = ComposeProject::new("myapp", config).unwrap();
        let box_config = project
            .build_box_config("web", Some("myapp_default"))
            .unwrap();

        assert!(box_config.privileged);
        assert_eq!(box_config.cap_add, vec!["NET_ADMIN"]);
        assert_eq!(box_config.cap_drop, vec!["ALL"]);
    }
}
