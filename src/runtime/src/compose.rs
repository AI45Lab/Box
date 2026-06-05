//! Compose orchestrator for multi-container workloads.
//!
//! Coordinates the lifecycle of multiple services defined in a compose file:
//! network creation, dependency-ordered boot, and grouped teardown.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use a3s_box_core::compose::ComposeConfig;
use a3s_box_core::config::{BoxConfig, ResourceConfig};
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
    /// Base directory used to resolve relative env_file paths.
    pub base_dir: PathBuf,
}

impl ComposeProject {
    /// Create a new compose project from a config.
    ///
    /// Validates the config and computes the service boot order.
    pub fn new(name: impl Into<String>, config: ComposeConfig) -> Result<Self> {
        let base_dir = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        Self::with_base_dir(name, config, base_dir)
    }

    /// Create a compose project with an explicit base directory.
    pub fn with_base_dir(
        name: impl Into<String>,
        config: ComposeConfig,
        base_dir: impl Into<PathBuf>,
    ) -> Result<Self> {
        let name = name.into();

        // Validate: every service must have an image
        for (svc_name, svc) in &config.services {
            if svc.image.is_none() {
                return Err(BoxError::ConfigError(format!(
                    "Service '{}' has no image specified",
                    svc_name
                )));
            }
            a3s_box_core::normalize_port_maps(&svc.ports).map_err(|e| {
                BoxError::ConfigError(format!(
                    "Service '{}' has invalid port mapping: {}",
                    svc_name, e
                ))
            })?;
            validate_depends_on_conditions(svc_name, &svc.depends_on)?;
        }

        // Compute topological order
        let service_order = config
            .service_order()
            .map_err(|e| BoxError::ConfigError(format!("Invalid compose config: {}", e)))?;

        Ok(Self {
            name,
            config,
            service_order,
            service_boxes: HashMap::new(),
            networks: Vec::new(),
            base_dir: base_dir.into(),
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
            BoxError::ConfigError(format!(
                "Service '{}' not found in compose config",
                service_name
            ))
        })?;

        let image = svc.image.as_deref().ok_or_else(|| {
            BoxError::ConfigError(format!("Service '{}' has no image", service_name))
        })?;

        // Parse memory limit
        let memory_mb = match &svc.mem_limit {
            Some(mem_str) => parse_compose_memory(mem_str)?,
            None => 512, // default
        };

        // Build environment: env_file first, environment overrides.
        let extra_env = self.service_env(svc)?;

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
        if let Some(hostname) = svc.hostname.as_deref() {
            a3s_box_core::dns::validate_hostname(hostname)
                .map_err(|e| BoxError::ConfigError(format!("Invalid hostname: {e}")))?;
        }
        let add_hosts = svc.extra_hosts.to_vec();
        a3s_box_core::dns::parse_add_host_entries(&add_hosts)
            .map_err(|e| BoxError::ConfigError(format!("Invalid extra_hosts entry: {e}")))?;
        let port_map = a3s_box_core::normalize_port_maps(&svc.ports).map_err(|e| {
            BoxError::ConfigError(format!(
                "Service '{}' has invalid port mapping: {}",
                service_name, e
            ))
        })?;

        let config = BoxConfig {
            image: image.to_string(),
            resources: ResourceConfig {
                vcpus: svc.cpus.unwrap_or(2),
                memory_mb,
                ..Default::default()
            },
            cmd,
            entrypoint_override,
            workdir: svc.working_dir.clone(),
            hostname: svc.hostname.clone(),
            volumes: svc.volumes.clone(),
            extra_env,
            port_map,
            dns: svc.dns.to_vec(),
            add_hosts,
            network: network_mode,
            tmpfs: svc.tmpfs.to_vec(),
            cap_add: svc.cap_add.clone(),
            cap_drop: svc.cap_drop.clone(),
            privileged: svc.privileged,
            ..Default::default()
        };

        Ok(config)
    }

    fn service_env(
        &self,
        svc: &a3s_box_core::compose::ServiceConfig,
    ) -> Result<Vec<(String, String)>> {
        let mut env = Vec::new();
        for env_file in svc.env_file.to_vec() {
            let path = resolve_compose_path(&self.base_dir, &env_file);
            let entries = a3s_box_core::env::parse_env_file(&path).map_err(|e| {
                BoxError::ConfigError(format!("Invalid env_file '{}': {}", path.display(), e))
            })?;
            a3s_box_core::env::merge_env_pairs(&mut env, &entries);
        }
        let inline_env = svc.environment.to_pairs();
        a3s_box_core::env::merge_env_pairs(&mut env, &inline_env);
        Ok(env)
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

    /// Check if a service requires its dependencies to be healthy before starting.
    ///
    /// Returns the list of dependency service names that must reach "healthy" status.
    pub fn health_wait_deps(&self, service_name: &str) -> Vec<String> {
        let Some(svc) = self.config.services.get(service_name) else {
            return vec![];
        };

        match &svc.depends_on {
            a3s_box_core::compose::DependsOn::Map(map) => map
                .iter()
                .filter(|(_, cond)| cond.condition == "service_healthy")
                .map(|(name, _)| name.clone())
                .collect(),
            _ => vec![],
        }
    }

    /// Dependencies this service must wait to run to completion (exit 0) before
    /// starting — `depends_on: { dep: { condition: service_completed_successfully } }`.
    pub fn completed_wait_deps(&self, service_name: &str) -> Vec<String> {
        let Some(svc) = self.config.services.get(service_name) else {
            return vec![];
        };

        match &svc.depends_on {
            a3s_box_core::compose::DependsOn::Map(map) => map
                .iter()
                .filter(|(_, cond)| cond.condition == "service_completed_successfully")
                .map(|(name, _)| name.clone())
                .collect(),
            _ => vec![],
        }
    }

    /// Get the health check config for a service, if defined.
    pub fn healthcheck(&self, service_name: &str) -> Option<HealthCheckSpec> {
        let svc = self.config.services.get(service_name)?;
        let hc = svc.healthcheck.as_ref()?;
        if hc.disable {
            return None;
        }

        let cmd = healthcheck_command(&hc.test)?;

        Some(HealthCheckSpec {
            cmd,
            interval_secs: hc
                .interval
                .as_deref()
                .and_then(parse_duration_secs)
                .unwrap_or(30),
            timeout_secs: hc
                .timeout
                .as_deref()
                .and_then(parse_duration_secs)
                .unwrap_or(30),
            retries: hc.retries.unwrap_or(3),
            start_period_secs: hc
                .start_period
                .as_deref()
                .and_then(parse_duration_secs)
                .unwrap_or(0),
        })
    }

    /// Return true when a service explicitly disables its health check.
    pub fn healthcheck_disabled(&self, service_name: &str) -> bool {
        self.config
            .services
            .get(service_name)
            .and_then(|svc| svc.healthcheck.as_ref())
            .is_some_and(|hc| {
                hc.disable
                    || matches!(
                        &hc.test,
                        a3s_box_core::compose::StringOrList::List(items)
                            if items.first().is_some_and(|value| value.eq_ignore_ascii_case("NONE"))
                    )
                    || matches!(
                        &hc.test,
                        a3s_box_core::compose::StringOrList::Single(value)
                            if value.trim().eq_ignore_ascii_case("NONE")
                    )
            })
    }
}

/// Parsed health check specification (runtime-friendly).
#[derive(Debug, Clone)]
pub struct HealthCheckSpec {
    /// Command to run.
    pub cmd: Vec<String>,
    /// Interval between checks in seconds.
    pub interval_secs: u64,
    /// Per-check timeout in seconds.
    pub timeout_secs: u64,
    /// Consecutive failures before unhealthy.
    pub retries: u32,
    /// Grace period before checks start counting.
    pub start_period_secs: u64,
}

/// Parse a compose duration string (e.g., "30s", "1m", "500ms") into seconds.
fn parse_duration_secs(s: &str) -> Option<u64> {
    let s = s.trim().to_lowercase();
    if s.ends_with("ms") {
        let n: u64 = s.trim_end_matches("ms").parse().ok()?;
        Some(n.div_ceil(1000))
    } else if s.ends_with('s') {
        s.trim_end_matches('s').parse().ok()
    } else if s.ends_with('m') {
        let n: u64 = s.trim_end_matches('m').parse().ok()?;
        Some(n * 60)
    } else if s.ends_with('h') {
        let n: u64 = s.trim_end_matches('h').parse().ok()?;
        Some(n * 3600)
    } else {
        // Assume seconds
        s.parse().ok()
    }
}

fn healthcheck_command(test: &a3s_box_core::compose::StringOrList) -> Option<Vec<String>> {
    use a3s_box_core::compose::StringOrList;

    match test {
        StringOrList::Empty => None,
        StringOrList::Single(command) => {
            let command = command.trim();
            if command.is_empty() || command.eq_ignore_ascii_case("NONE") {
                None
            } else {
                Some(vec![
                    "sh".to_string(),
                    "-c".to_string(),
                    command.to_string(),
                ])
            }
        }
        StringOrList::List(items) => {
            let marker = items.first()?;
            if marker.eq_ignore_ascii_case("NONE") {
                return None;
            }
            if marker.eq_ignore_ascii_case("CMD") {
                let cmd = items.get(1..)?.to_vec();
                return (!cmd.is_empty()).then_some(cmd);
            }
            if marker.eq_ignore_ascii_case("CMD-SHELL") {
                let shell_cmd = items.get(1..)?.join(" ");
                return (!shell_cmd.is_empty()).then_some(vec![
                    "sh".to_string(),
                    "-c".to_string(),
                    shell_cmd,
                ]);
            }

            Some(items.clone()).filter(|cmd| !cmd.is_empty())
        }
    }
}

fn validate_depends_on_conditions(
    service_name: &str,
    depends_on: &a3s_box_core::compose::DependsOn,
) -> Result<()> {
    let a3s_box_core::compose::DependsOn::Map(map) = depends_on else {
        return Ok(());
    };

    for (dep_name, condition) in map {
        match condition.condition.as_str() {
            "service_started" | "service_healthy" | "service_completed_successfully" => {}
            other => {
                return Err(BoxError::ConfigError(format!(
                    "Service '{}' depends on '{}' with unsupported condition '{}' (supported: service_started, service_healthy, service_completed_successfully)",
                    service_name, dep_name, other
                )));
            }
        }
    }

    Ok(())
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
            .map_err(|_| BoxError::ConfigError(format!("Invalid memory value: {}", s)));
    } else {
        // Assume bytes
        return s
            .parse::<u64>()
            .map(|v| v.div_ceil(1024 * 1024) as u32)
            .map_err(|_| BoxError::ConfigError(format!("Invalid memory value: {}", s)));
    };

    let num: f64 = num_str
        .parse()
        .map_err(|_| BoxError::ConfigError(format!("Invalid memory value: {}", s)))?;

    Ok((num * multiplier as f64) as u32)
}

fn resolve_compose_path(base_dir: &Path, path: &str) -> PathBuf {
    let path = PathBuf::from(path);
    if path.is_absolute() {
        path
    } else {
        base_dir.join(path)
    }
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

        assert_eq!(box_config.image, "postgres:16");
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
    fn test_build_box_config_env_file_with_environment_override() {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::write(dir.path().join("app.env"), "FOO=file\nBAR=file\n").unwrap();
        let yaml = r#"
services:
  api:
    image: myapi
    env_file:
      - app.env
    environment:
      FOO: inline
      BAZ: inline
"#;
        let config = ComposeConfig::from_yaml_str(yaml).unwrap();
        let project = ComposeProject::with_base_dir("myapp", config, dir.path()).unwrap();
        let box_config = project
            .build_box_config("api", Some("myapp_default"))
            .unwrap();

        assert_eq!(
            box_config.extra_env,
            vec![
                ("FOO".to_string(), "inline".to_string()),
                ("BAR".to_string(), "file".to_string()),
                ("BAZ".to_string(), "inline".to_string())
            ]
        );
    }

    #[test]
    fn test_build_box_config_missing_env_file_is_rejected() {
        let dir = tempfile::TempDir::new().unwrap();
        let yaml = r#"
services:
  api:
    image: myapi
    env_file: missing.env
"#;
        let config = ComposeConfig::from_yaml_str(yaml).unwrap();
        let project = ComposeProject::with_base_dir("myapp", config, dir.path()).unwrap();

        let err = project
            .build_box_config("api", Some("myapp_default"))
            .unwrap_err();

        assert!(err.to_string().contains("Invalid env_file"));
    }

    #[test]
    fn test_build_box_config_working_dir() {
        let yaml = r#"
services:
  worker:
    image: myworker
    working_dir: /srv/app
"#;
        let config = ComposeConfig::from_yaml_str(yaml).unwrap();
        let project = ComposeProject::new("myapp", config).unwrap();
        let box_config = project
            .build_box_config("worker", Some("myapp_default"))
            .unwrap();

        assert_eq!(box_config.workdir.as_deref(), Some("/srv/app"));
    }

    #[test]
    fn test_build_box_config_hostname_and_extra_hosts() {
        let yaml = r#"
services:
  web:
    image: nginx
    hostname: web-1
    extra_hosts:
      - "db.local:10.88.0.10"
"#;
        let config = ComposeConfig::from_yaml_str(yaml).unwrap();
        let project = ComposeProject::new("myapp", config).unwrap();
        let box_config = project
            .build_box_config("web", Some("myapp_default"))
            .unwrap();

        assert_eq!(box_config.hostname.as_deref(), Some("web-1"));
        assert_eq!(box_config.add_hosts, vec!["db.local:10.88.0.10"]);
    }

    #[test]
    fn test_build_box_config_rejects_invalid_extra_hosts() {
        let yaml = r#"
services:
  web:
    image: nginx
    extra_hosts:
      - "db.local:not-an-ip"
"#;
        let config = ComposeConfig::from_yaml_str(yaml).unwrap();
        let project = ComposeProject::new("myapp", config).unwrap();

        let err = project
            .build_box_config("web", Some("myapp_default"))
            .unwrap_err();

        assert!(err.to_string().contains("Invalid extra_hosts"));
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
    fn test_build_box_config_normalizes_tcp_port_suffix() {
        let yaml = r#"
services:
  web:
    image: nginx
    ports:
      - "8080:80/tcp"
"#;
        let config = ComposeConfig::from_yaml_str(yaml).unwrap();
        let project = ComposeProject::new("myapp", config).unwrap();
        let box_config = project
            .build_box_config("web", Some("myapp_default"))
            .unwrap();

        assert_eq!(box_config.port_map, vec!["8080:80"]);
    }

    #[test]
    fn test_compose_project_rejects_udp_ports() {
        let yaml = r#"
services:
  web:
    image: nginx
    ports:
      - "8080:80/udp"
"#;
        let config = ComposeConfig::from_yaml_str(yaml).unwrap();

        let err = ComposeProject::new("myapp", config).unwrap_err();

        assert!(err.to_string().contains("only TCP is supported"));
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

    #[test]
    fn test_parse_duration_secs() {
        assert_eq!(parse_duration_secs("30s"), Some(30));
        assert_eq!(parse_duration_secs("1m"), Some(60));
        assert_eq!(parse_duration_secs("2h"), Some(7200));
        assert_eq!(parse_duration_secs("500ms"), Some(1));
        assert_eq!(parse_duration_secs("5000ms"), Some(5));
        assert_eq!(parse_duration_secs("10"), Some(10));
        assert_eq!(parse_duration_secs("abc"), None);
    }

    #[test]
    fn test_health_wait_deps_simple() {
        let yaml = r#"
services:
  web:
    image: nginx
    depends_on:
      - db
  db:
    image: postgres
"#;
        let config = ComposeConfig::from_yaml_str(yaml).unwrap();
        let project = ComposeProject::new("myapp", config).unwrap();
        // Simple depends_on → no health wait (condition defaults to service_started)
        assert!(project.health_wait_deps("web").is_empty());
    }

    #[test]
    fn test_health_wait_deps_service_healthy() {
        let yaml = r#"
services:
  web:
    image: nginx
    depends_on:
      db:
        condition: service_healthy
      redis:
        condition: service_started
  db:
    image: postgres
    healthcheck:
      test: ["CMD", "pg_isready"]
      interval: 10s
      timeout: 5s
      retries: 5
  redis:
    image: redis
"#;
        let config = ComposeConfig::from_yaml_str(yaml).unwrap();
        let project = ComposeProject::new("myapp", config).unwrap();
        let deps = project.health_wait_deps("web");
        assert_eq!(deps, vec!["db".to_string()]);
    }

    #[test]
    fn test_healthcheck_spec() {
        let yaml = r#"
services:
  web:
    image: nginx
    healthcheck:
      test: ["CMD", "curl", "-f", "http://localhost/"]
      interval: 10s
      timeout: 3s
      retries: 5
      start_period: 30s
"#;
        let config = ComposeConfig::from_yaml_str(yaml).unwrap();
        let project = ComposeProject::new("myapp", config).unwrap();
        let hc = project.healthcheck("web").unwrap();
        assert_eq!(hc.cmd, vec!["curl", "-f", "http://localhost/"]);
        assert_eq!(hc.interval_secs, 10);
        assert_eq!(hc.timeout_secs, 3);
        assert_eq!(hc.retries, 5);
        assert_eq!(hc.start_period_secs, 30);
    }

    #[test]
    fn test_healthcheck_spec_defaults() {
        let yaml = r#"
services:
  web:
    image: nginx
    healthcheck:
      test: ["CMD", "true"]
"#;
        let config = ComposeConfig::from_yaml_str(yaml).unwrap();
        let project = ComposeProject::new("myapp", config).unwrap();
        let hc = project.healthcheck("web").unwrap();
        assert_eq!(hc.cmd, vec!["true"]);
        assert_eq!(hc.interval_secs, 30);
        assert_eq!(hc.timeout_secs, 30);
        assert_eq!(hc.retries, 3);
        assert_eq!(hc.start_period_secs, 0);
    }

    #[test]
    fn test_healthcheck_cmd_shell() {
        let yaml = r#"
services:
  web:
    image: nginx
    healthcheck:
      test: ["CMD-SHELL", "curl -f http://localhost/ || exit 1"]
"#;
        let config = ComposeConfig::from_yaml_str(yaml).unwrap();
        let project = ComposeProject::new("myapp", config).unwrap();
        let hc = project.healthcheck("web").unwrap();
        assert_eq!(
            hc.cmd,
            vec!["sh", "-c", "curl -f http://localhost/ || exit 1"]
        );
    }

    #[test]
    fn test_healthcheck_single_string_uses_shell() {
        let yaml = r#"
services:
  web:
    image: nginx
    healthcheck:
      test: curl -f http://localhost/ || exit 1
"#;
        let config = ComposeConfig::from_yaml_str(yaml).unwrap();
        let project = ComposeProject::new("myapp", config).unwrap();
        let hc = project.healthcheck("web").unwrap();
        assert_eq!(
            hc.cmd,
            vec!["sh", "-c", "curl -f http://localhost/ || exit 1"]
        );
    }

    #[test]
    fn test_healthcheck_none_and_disable() {
        let yaml = r#"
services:
  none:
    image: nginx
    healthcheck:
      test: ["NONE"]
  disabled:
    image: redis
    healthcheck:
      disable: true
"#;
        let config = ComposeConfig::from_yaml_str(yaml).unwrap();
        let project = ComposeProject::new("myapp", config).unwrap();
        assert!(project.healthcheck("none").is_none());
        assert!(project.healthcheck_disabled("none"));
        assert!(project.healthcheck("disabled").is_none());
        assert!(project.healthcheck_disabled("disabled"));
    }

    #[test]
    fn test_healthcheck_none() {
        let config = sample_config();
        let project = ComposeProject::new("myapp", config).unwrap();
        assert!(project.healthcheck("db").is_none());
    }

    #[test]
    fn test_service_completed_successfully_condition_accepted() {
        let yaml = r#"
services:
  web:
    image: nginx
    depends_on:
      init:
        condition: service_completed_successfully
  init:
    image: busybox
"#;
        let config = ComposeConfig::from_yaml_str(yaml).unwrap();
        let project = ComposeProject::new("myapp", config).unwrap();
        assert_eq!(project.completed_wait_deps("web"), vec!["init".to_string()]);
        // `init` itself has no completion wait.
        assert!(project.completed_wait_deps("init").is_empty());
    }

    #[test]
    fn test_unsupported_depends_on_condition_rejected() {
        let yaml = r#"
services:
  web:
    image: nginx
    depends_on:
      db:
        condition: service_bogus_condition
  db:
    image: postgres
"#;
        let config = ComposeConfig::from_yaml_str(yaml).unwrap();
        let err = ComposeProject::new("myapp", config).unwrap_err();
        assert!(err.to_string().contains("unsupported condition"));
    }
}
