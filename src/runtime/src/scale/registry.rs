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
        let mut svcs: Vec<String> = self
            .entries
            .values()
            .map(|e| e.service.clone())
            .collect();
        svcs.sort();
        svcs.dedup();
        svcs
    }
}
