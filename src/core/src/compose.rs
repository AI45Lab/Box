//! Compose file types for multi-container orchestration.
//!
//! Defines a docker-compose-compatible YAML schema for declaring
//! multi-service workloads. Each service maps to a single MicroVM.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Top-level compose file configuration.
///
/// Compatible with a subset of docker-compose v3 syntax:
/// ```yaml
/// version: "3"
/// services:
///   web:
///     image: nginx:latest
///     ports: ["8080:80"]
///     depends_on: [db]
///   db:
///     image: postgres:16
///     environment:
///       POSTGRES_PASSWORD: secret
///     volumes: ["pgdata:/var/lib/postgresql/data"]
/// volumes:
///   pgdata:
/// networks:
///   default:
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ComposeConfig {
    /// Compose file version (informational, not enforced).
    #[serde(default)]
    pub version: Option<String>,

    /// Service definitions keyed by name.
    pub services: HashMap<String, ServiceConfig>,

    /// Named volume declarations (value is currently unused, reserved for driver options).
    #[serde(default)]
    pub volumes: HashMap<String, Option<VolumeDeclaration>>,

    /// Named network declarations.
    #[serde(default)]
    pub networks: HashMap<String, Option<NetworkDeclaration>>,
}

/// A single service in a compose file.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ServiceConfig {
    /// OCI image reference (e.g., "nginx:latest").
    #[serde(default)]
    pub image: Option<String>,

    /// Override the container entrypoint.
    #[serde(default)]
    pub entrypoint: Option<StringOrList>,

    /// Override the container command.
    #[serde(default)]
    pub command: Option<StringOrList>,

    /// Environment variables.
    #[serde(default)]
    pub environment: EnvVars,

    /// Port mappings ("host:container").
    #[serde(default)]
    pub ports: Vec<String>,

    /// Volume mounts ("name:/path" or "/host:/container").
    #[serde(default)]
    pub volumes: Vec<String>,

    /// Services this service depends on (started first).
    #[serde(default)]
    pub depends_on: DependsOn,

    /// Networks to connect to.
    #[serde(default)]
    pub networks: ServiceNetworks,

    /// Number of CPUs.
    #[serde(default)]
    pub cpus: Option<u32>,

    /// Memory limit (e.g., "512m", "1g").
    #[serde(default)]
    pub mem_limit: Option<String>,

    /// Restart policy: "no", "always", "on-failure", "unless-stopped".
    #[serde(default)]
    pub restart: Option<String>,

    /// Custom DNS servers.
    #[serde(default)]
    pub dns: DnsConfig,

    /// tmpfs mounts.
    #[serde(default)]
    pub tmpfs: StringOrList,

    /// Linux capabilities to add.
    #[serde(default)]
    pub cap_add: Vec<String>,

    /// Linux capabilities to drop.
    #[serde(default)]
    pub cap_drop: Vec<String>,

    /// Privileged mode.
    #[serde(default)]
    pub privileged: bool,

    /// Custom labels.
    #[serde(default)]
    pub labels: Labels,

    /// Health check configuration.
    #[serde(default)]
    pub healthcheck: Option<HealthcheckConfig>,

    /// Working directory inside the container.
    #[serde(default)]
    pub working_dir: Option<String>,
}

/// Health check configuration for a service.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HealthcheckConfig {
    /// Command to run (e.g., ["CMD", "curl", "-f", "http://localhost/"]).
    pub test: StringOrList,
    /// Interval between checks (e.g., "30s").
    #[serde(default)]
    pub interval: Option<String>,
    /// Timeout for each check (e.g., "5s").
    #[serde(default)]
    pub timeout: Option<String>,
    /// Number of retries before unhealthy.
    #[serde(default)]
    pub retries: Option<u32>,
    /// Start period before health checks count (e.g., "10s").
    #[serde(default)]
    pub start_period: Option<String>,
}

/// Volume declaration.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct VolumeDeclaration {
    /// Volume driver (default: "local").
    #[serde(default)]
    pub driver: Option<String>,
}

/// Network declaration.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct NetworkDeclaration {
    /// Network driver (default: "bridge").
    #[serde(default)]
    pub driver: Option<String>,
}

/// A value that can be either a string or a list of strings.
///
/// Handles both `command: "echo hello"` and `command: ["echo", "hello"]`.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(untagged)]
pub enum StringOrList {
    #[default]
    Empty,
    Single(String),
    List(Vec<String>),
}

impl StringOrList {
    /// Convert to a Vec<String>, splitting a single string on whitespace.
    pub fn to_vec(&self) -> Vec<String> {
        match self {
            Self::Empty => vec![],
            Self::Single(s) => s.split_whitespace().map(String::from).collect(),
            Self::List(v) => v.clone(),
        }
    }

    /// Returns true if empty.
    pub fn is_empty(&self) -> bool {
        match self {
            Self::Empty => true,
            Self::Single(s) => s.is_empty(),
            Self::List(v) => v.is_empty(),
        }
    }
}

/// Environment variables: supports both map and list format.
///
/// Map: `environment: { KEY: value }`
/// List: `environment: ["KEY=value"]`
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(untagged)]
pub enum EnvVars {
    #[default]
    Empty,
    Map(HashMap<String, String>),
    List(Vec<String>),
}

impl EnvVars {
    /// Convert to a list of (key, value) pairs.
    pub fn to_pairs(&self) -> Vec<(String, String)> {
        match self {
            Self::Empty => vec![],
            Self::Map(m) => m.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
            Self::List(list) => list
                .iter()
                .filter_map(|s| {
                    let (k, v) = s.split_once('=')?;
                    Some((k.to_string(), v.to_string()))
                })
                .collect(),
        }
    }
}

/// depends_on: supports both simple list and extended syntax.
///
/// Simple: `depends_on: [db, redis]`
/// Extended: `depends_on: { db: { condition: service_healthy } }`
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(untagged)]
pub enum DependsOn {
    #[default]
    Empty,
    List(Vec<String>),
    Map(HashMap<String, DependsOnCondition>),
}

impl DependsOn {
    /// Get the list of dependency service names.
    pub fn services(&self) -> Vec<String> {
        match self {
            Self::Empty => vec![],
            Self::List(v) => v.clone(),
            Self::Map(m) => m.keys().cloned().collect(),
        }
    }
}

/// Condition for a depends_on entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DependsOnCondition {
    /// Condition: "service_started" (default) or "service_healthy".
    #[serde(default = "default_condition")]
    pub condition: String,
}

fn default_condition() -> String {
    "service_started".to_string()
}

/// Service networks: supports both list and map format.
///
/// List: `networks: [frontend, backend]`
/// Map: `networks: { frontend: {} }`
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(untagged)]
pub enum ServiceNetworks {
    #[default]
    Empty,
    List(Vec<String>),
    Map(HashMap<String, Option<ServiceNetworkConfig>>),
}

impl ServiceNetworks {
    /// Get the list of network names.
    pub fn names(&self) -> Vec<String> {
        match self {
            Self::Empty => vec![],
            Self::List(v) => v.clone(),
            Self::Map(m) => m.keys().cloned().collect(),
        }
    }
}

/// Per-service network configuration.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ServiceNetworkConfig {
    /// Network aliases for this service.
    #[serde(default)]
    pub aliases: Vec<String>,
}

/// Labels: supports both map and list format.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(untagged)]
pub enum Labels {
    #[default]
    Empty,
    Map(HashMap<String, String>),
    List(Vec<String>),
}

/// DNS config: supports both single string and list.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(untagged)]
pub enum DnsConfig {
    #[default]
    Empty,
    Single(String),
    List(Vec<String>),
}

impl DnsConfig {
    /// Convert to a list of DNS server addresses.
    pub fn to_vec(&self) -> Vec<String> {
        match self {
            Self::Empty => vec![],
            Self::Single(s) => vec![s.clone()],
            Self::List(v) => v.clone(),
        }
    }
}

impl ComposeConfig {
    /// Parse a compose config from YAML bytes.
    pub fn from_yaml(yaml: &[u8]) -> Result<Self, serde_yaml::Error> {
        serde_yaml::from_slice(yaml)
    }

    /// Parse a compose config from a YAML string.
    pub fn from_yaml_str(yaml: &str) -> Result<Self, serde_yaml::Error> {
        serde_yaml::from_str(yaml)
    }

    /// Compute a topological ordering of services based on depends_on.
    ///
    /// Returns an error if there is a dependency cycle.
    pub fn service_order(&self) -> Result<Vec<String>, String> {
        let mut order = Vec::new();
        // 0 = unvisited, 1 = in-progress, 2 = done
        let mut state: HashMap<String, u8> = HashMap::new();

        for name in self.services.keys() {
            if !state.contains_key(name) {
                self.topo_visit(name, &mut state, &mut order)?;
            }
        }

        Ok(order)
    }

    fn topo_visit(
        &self,
        name: &str,
        state: &mut HashMap<String, u8>,
        order: &mut Vec<String>,
    ) -> Result<(), String> {
        match state.get(name) {
            Some(1) => {
                return Err(format!(
                    "Dependency cycle detected involving service '{}'",
                    name
                ));
            }
            Some(2) => return Ok(()), // already fully visited
            _ => {}
        }

        state.insert(name.to_string(), 1); // in-progress

        if let Some(svc) = self.services.get(name) {
            let deps = svc.depends_on.services();
            for dep in &deps {
                if !self.services.contains_key(dep) {
                    return Err(format!(
                        "Service '{}' depends on '{}' which is not defined",
                        name, dep
                    ));
                }
                self.topo_visit(dep, state, order)?;
            }
        }

        state.insert(name.to_string(), 2); // done
        order.push(name.to_string());
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_minimal_compose() {
        let yaml = r#"
services:
  web:
    image: nginx:latest
"#;
        let config = ComposeConfig::from_yaml_str(yaml).unwrap();
        assert_eq!(config.services.len(), 1);
        assert_eq!(
            config.services["web"].image.as_deref(),
            Some("nginx:latest")
        );
    }

    #[test]
    fn test_parse_full_compose() {
        let yaml = r#"
version: "3"
services:
  web:
    image: nginx:latest
    ports:
      - "8080:80"
    depends_on:
      - db
    environment:
      APP_ENV: production
    volumes:
      - "static:/usr/share/nginx/html"
  db:
    image: postgres:16
    environment:
      - POSTGRES_PASSWORD=secret
    volumes:
      - "pgdata:/var/lib/postgresql/data"
    mem_limit: "1g"
    cpus: 2
volumes:
  pgdata:
  static:
networks:
  default:
"#;
        let config = ComposeConfig::from_yaml_str(yaml).unwrap();
        assert_eq!(config.services.len(), 2);
        assert_eq!(config.volumes.len(), 2);
        assert!(config.services["web"]
            .depends_on
            .services()
            .contains(&"db".to_string()));
        assert_eq!(config.services["web"].ports, vec!["8080:80"]);
        assert_eq!(config.services["db"].cpus, Some(2));
        assert_eq!(config.services["db"].mem_limit.as_deref(), Some("1g"));
    }

    #[test]
    fn test_service_order_simple() {
        let yaml = r#"
services:
  web:
    image: nginx
    depends_on: [api]
  api:
    image: myapi
    depends_on: [db]
  db:
    image: postgres
"#;
        let config = ComposeConfig::from_yaml_str(yaml).unwrap();
        let order = config.service_order().unwrap();
        let db_pos = order.iter().position(|s| s == "db").unwrap();
        let api_pos = order.iter().position(|s| s == "api").unwrap();
        let web_pos = order.iter().position(|s| s == "web").unwrap();
        assert!(db_pos < api_pos);
        assert!(api_pos < web_pos);
    }

    #[test]
    fn test_service_order_cycle_detected() {
        let yaml = r#"
services:
  a:
    image: img
    depends_on: [b]
  b:
    image: img
    depends_on: [a]
"#;
        let config = ComposeConfig::from_yaml_str(yaml).unwrap();
        let result = config.service_order();
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("cycle"));
    }

    #[test]
    fn test_service_order_missing_dependency() {
        let yaml = r#"
services:
  web:
    image: nginx
    depends_on: [nonexistent]
"#;
        let config = ComposeConfig::from_yaml_str(yaml).unwrap();
        let result = config.service_order();
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("not defined"));
    }

    #[test]
    fn test_service_order_no_deps() {
        let yaml = r#"
services:
  a:
    image: img
  b:
    image: img
  c:
    image: img
"#;
        let config = ComposeConfig::from_yaml_str(yaml).unwrap();
        let order = config.service_order().unwrap();
        assert_eq!(order.len(), 3);
    }

    #[test]
    fn test_env_vars_map() {
        let yaml = r#"
services:
  web:
    image: nginx
    environment:
      KEY1: val1
      KEY2: val2
"#;
        let config = ComposeConfig::from_yaml_str(yaml).unwrap();
        let pairs = config.services["web"].environment.to_pairs();
        assert_eq!(pairs.len(), 2);
    }

    #[test]
    fn test_env_vars_list() {
        let yaml = r#"
services:
  web:
    image: nginx
    environment:
      - KEY1=val1
      - KEY2=val2
"#;
        let config = ComposeConfig::from_yaml_str(yaml).unwrap();
        let pairs = config.services["web"].environment.to_pairs();
        assert_eq!(pairs.len(), 2);
        assert!(pairs.iter().any(|(k, v)| k == "KEY1" && v == "val1"));
    }

    #[test]
    fn test_string_or_list_single() {
        let sol = StringOrList::Single("echo hello world".to_string());
        assert_eq!(sol.to_vec(), vec!["echo", "hello", "world"]);
        assert!(!sol.is_empty());
    }

    #[test]
    fn test_string_or_list_list() {
        let sol = StringOrList::List(vec!["echo".into(), "hello world".into()]);
        assert_eq!(sol.to_vec(), vec!["echo", "hello world"]);
    }

    #[test]
    fn test_string_or_list_empty() {
        let sol = StringOrList::Empty;
        assert!(sol.is_empty());
        assert!(sol.to_vec().is_empty());
    }

    #[test]
    fn test_depends_on_list() {
        let yaml = r#"
services:
  web:
    image: nginx
    depends_on:
      - db
      - redis
"#;
        let config = ComposeConfig::from_yaml_str(yaml).unwrap();
        let deps = config.services["web"].depends_on.services();
        assert_eq!(deps.len(), 2);
        assert!(deps.contains(&"db".to_string()));
        assert!(deps.contains(&"redis".to_string()));
    }

    #[test]
    fn test_depends_on_map() {
        let yaml = r#"
services:
  web:
    image: nginx
    depends_on:
      db:
        condition: service_healthy
"#;
        let config = ComposeConfig::from_yaml_str(yaml).unwrap();
        let deps = config.services["web"].depends_on.services();
        assert_eq!(deps, vec!["db"]);
    }

    #[test]
    fn test_dns_config_single() {
        let dns = DnsConfig::Single("8.8.8.8".to_string());
        assert_eq!(dns.to_vec(), vec!["8.8.8.8"]);
    }

    #[test]
    fn test_dns_config_list() {
        let dns = DnsConfig::List(vec!["8.8.8.8".into(), "1.1.1.1".into()]);
        assert_eq!(dns.to_vec().len(), 2);
    }

    #[test]
    fn test_service_networks_list() {
        let yaml = r#"
services:
  web:
    image: nginx
    networks:
      - frontend
      - backend
"#;
        let config = ComposeConfig::from_yaml_str(yaml).unwrap();
        let nets = config.services["web"].networks.names();
        assert_eq!(nets.len(), 2);
    }

    #[test]
    fn test_healthcheck_config() {
        let yaml = r#"
services:
  web:
    image: nginx
    healthcheck:
      test: ["CMD", "curl", "-f", "http://localhost/"]
      interval: "30s"
      timeout: "5s"
      retries: 3
"#;
        let config = ComposeConfig::from_yaml_str(yaml).unwrap();
        let hc = config.services["web"].healthcheck.as_ref().unwrap();
        assert_eq!(hc.retries, Some(3));
        assert_eq!(hc.interval.as_deref(), Some("30s"));
    }

    #[test]
    fn test_compose_serde_roundtrip() {
        let yaml = r#"
version: "3"
services:
  web:
    image: nginx:latest
    ports:
      - "8080:80"
"#;
        let config = ComposeConfig::from_yaml_str(yaml).unwrap();
        let serialized = serde_yaml::to_string(&config).unwrap();
        let reparsed = ComposeConfig::from_yaml_str(&serialized).unwrap();
        assert_eq!(reparsed.services.len(), 1);
        assert_eq!(
            reparsed.services["web"].image.as_deref(),
            Some("nginx:latest")
        );
    }

    #[test]
    fn test_labels_map() {
        let yaml = r#"
services:
  web:
    image: nginx
    labels:
      com.example.env: production
"#;
        let config = ComposeConfig::from_yaml_str(yaml).unwrap();
        assert!(matches!(config.services["web"].labels, Labels::Map(_)));
    }

    #[test]
    fn test_service_config_defaults() {
        let svc = ServiceConfig::default();
        assert!(svc.image.is_none());
        assert!(svc.ports.is_empty());
        assert!(svc.volumes.is_empty());
        assert!(!svc.privileged);
    }
}
