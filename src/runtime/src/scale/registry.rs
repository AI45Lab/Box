//! Instance registry for multi-node deployments.

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Registry for instance self-registration in standalone multi-node deployments.
///
/// Each Box host registers its instances with the registry so Gateway
/// can discover endpoints for traffic routing.
pub struct InstanceRegistry {
    /// Registered instances: instance_id → registration
    pub(super) entries: HashMap<String, RegistryEntry>,
}

/// A registered instance entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(super) struct RegistryEntry {
    pub(super) instance_id: String,
    pub(super) service: String,
    pub(super) endpoint: String,
    pub(super) host_id: String,
    pub(super) metadata: HashMap<String, String>,
    pub(super) registered_at: DateTime<Utc>,
    pub(super) last_heartbeat: DateTime<Utc>,
}

impl Default for InstanceRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl InstanceRegistry {
    /// Create a new empty registry.
    pub fn new() -> Self {
        Self {
            entries: HashMap::new(),
        }
    }

    /// Register an instance.
    pub fn register(
        &mut self,
        instance_id: &str,
        service: &str,
        endpoint: &str,
        host_id: &str,
        metadata: HashMap<String, String>,
    ) {
        let now = Utc::now();
        self.entries.insert(
            instance_id.to_string(),
            RegistryEntry {
                instance_id: instance_id.to_string(),
                service: service.to_string(),
                endpoint: endpoint.to_string(),
                host_id: host_id.to_string(),
                metadata,
                registered_at: now,
                last_heartbeat: now,
            },
        );
    }

    /// Deregister an instance.
    pub fn deregister(&mut self, instance_id: &str) -> bool {
        self.entries.remove(instance_id).is_some()
    }

    /// Record a heartbeat for an instance.
    pub fn heartbeat(&mut self, instance_id: &str) -> bool {
        if let Some(entry) = self.entries.get_mut(instance_id) {
            entry.last_heartbeat = Utc::now();
            true
        } else {
            false
        }
    }

    /// Get all endpoints for a service (for load balancing).
    pub fn endpoints(&self, service: &str) -> Vec<String> {
        self.entries
            .values()
            .filter(|e| e.service == service)
            .map(|e| e.endpoint.clone())
            .collect()
    }

    /// Get all instances for a service.
    pub fn instances_for_service(&self, service: &str) -> Vec<&str> {
        self.entries
            .values()
            .filter(|e| e.service == service)
            .map(|e| e.instance_id.as_str())
            .collect()
    }

    /// Get all instances on a specific host.
    pub fn instances_on_host(&self, host_id: &str) -> Vec<&str> {
        self.entries
            .values()
            .filter(|e| e.host_id == host_id)
            .map(|e| e.instance_id.as_str())
            .collect()
    }

    /// Remove stale entries that haven't sent a heartbeat within the given duration.
    pub fn evict_stale(&mut self, max_age: chrono::Duration) -> Vec<String> {
        let cutoff = Utc::now() - max_age;
        let stale: Vec<String> = self
            .entries
            .iter()
            .filter(|(_, e)| e.last_heartbeat < cutoff)
            .map(|(id, _)| id.clone())
            .collect();

        for id in &stale {
            self.entries.remove(id);
        }
        stale
    }

    /// Total number of registered instances.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the registry is empty.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// List all unique services in the registry.
    pub fn services(&self) -> Vec<String> {
        let mut svcs: Vec<String> = self.entries.values().map(|e| e.service.clone()).collect();
        svcs.sort();
        svcs.dedup();
        svcs
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn metadata(tier: &str) -> HashMap<String, String> {
        HashMap::from([("tier".to_string(), tier.to_string())])
    }

    #[test]
    fn register_indexes_instances_by_service_and_host() {
        let mut registry = InstanceRegistry::new();

        registry.register(
            "box-1",
            "search",
            "http://10.0.0.1:8080",
            "host-a",
            metadata("blue"),
        );
        registry.register(
            "box-2",
            "search",
            "http://10.0.0.2:8080",
            "host-b",
            metadata("green"),
        );
        registry.register(
            "box-3",
            "gateway",
            "http://10.0.0.3:8080",
            "host-a",
            metadata("edge"),
        );

        assert_eq!(registry.len(), 3);

        let mut search_endpoints = registry.endpoints("search");
        search_endpoints.sort();
        assert_eq!(
            search_endpoints,
            vec![
                "http://10.0.0.1:8080".to_string(),
                "http://10.0.0.2:8080".to_string()
            ]
        );

        let mut host_a = registry.instances_on_host("host-a");
        host_a.sort();
        assert_eq!(host_a, vec!["box-1", "box-3"]);

        let mut services = registry.services();
        services.sort();
        assert_eq!(services, vec!["gateway".to_string(), "search".to_string()]);
    }

    #[test]
    fn register_replaces_existing_instance_record() {
        let mut registry = InstanceRegistry::new();

        registry.register(
            "box-1",
            "old",
            "http://10.0.0.1:8080",
            "host-a",
            metadata("old"),
        );
        registry.register(
            "box-1",
            "new",
            "http://10.0.0.9:9090",
            "host-b",
            metadata("new"),
        );

        assert_eq!(registry.len(), 1);
        assert!(registry.endpoints("old").is_empty());
        assert_eq!(
            registry.endpoints("new"),
            vec!["http://10.0.0.9:9090".to_string()]
        );
        assert_eq!(registry.instances_on_host("host-b"), vec!["box-1"]);
        assert_eq!(
            registry.entries["box-1"].metadata.get("tier"),
            Some(&"new".to_string())
        );
    }

    #[test]
    fn heartbeat_and_deregister_report_whether_instance_exists() {
        let mut registry = InstanceRegistry::new();
        registry.register(
            "box-1",
            "search",
            "http://10.0.0.1:8080",
            "host-a",
            metadata("blue"),
        );
        let before = registry.entries["box-1"].last_heartbeat;

        assert!(registry.heartbeat("box-1"));
        assert!(registry.entries["box-1"].last_heartbeat >= before);
        assert!(!registry.heartbeat("missing"));

        assert!(registry.deregister("box-1"));
        assert!(registry.is_empty());
        assert!(!registry.deregister("box-1"));
    }

    #[test]
    fn evict_stale_removes_only_expired_entries() {
        let mut registry = InstanceRegistry::new();
        registry.register(
            "fresh",
            "search",
            "http://10.0.0.1:8080",
            "host-a",
            metadata("fresh"),
        );
        registry.register(
            "stale",
            "search",
            "http://10.0.0.2:8080",
            "host-b",
            metadata("stale"),
        );
        registry.entries.get_mut("stale").unwrap().last_heartbeat =
            Utc::now() - chrono::Duration::minutes(10);

        let evicted = registry.evict_stale(chrono::Duration::minutes(5));

        assert_eq!(evicted, vec!["stale".to_string()]);
        assert!(registry.entries.contains_key("fresh"));
        assert!(!registry.entries.contains_key("stale"));
    }
}
