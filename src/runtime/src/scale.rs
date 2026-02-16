//! Scale Manager — Tracks instances per service and processes scale requests.
//!
//! Manages the mapping between services and their running instances,
//! handles scale-up/scale-down decisions, and emits instance state events.

use std::collections::HashMap;

use a3s_box_core::error::{BoxError, Result};
use a3s_box_core::scale::{
    InstanceEvent, InstanceHealth, InstanceInfo, InstanceState, ScaleRequest, ScaleResponse,
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Tracks all instances managed by this Box host.
pub struct ScaleManager {
    /// Maximum total instances across all services
    max_instances: u32,
    /// Per-service instance tracking
    services: HashMap<String, ServiceInstances>,
    /// Event log (bounded ring buffer)
    events: Vec<InstanceEvent>,
    /// Maximum events to retain
    max_events: usize,
}

/// Instances belonging to a single service.
#[derive(Debug, Clone)]
struct ServiceInstances {
    /// Target replica count
    target_replicas: u32,
    /// Active instances
    instances: Vec<TrackedInstance>,
}

/// A tracked instance with its current state.
#[derive(Debug, Clone)]
struct TrackedInstance {
    id: String,
    state: InstanceState,
    created_at: DateTime<Utc>,
    ready_at: Option<DateTime<Utc>>,
    endpoint: Option<String>,
    health: InstanceHealth,
}

impl ScaleManager {
    /// Create a new scale manager with the given capacity.
    pub fn new(max_instances: u32) -> Self {
        Self {
            max_instances,
            services: HashMap::new(),
            events: Vec::new(),
            max_events: 1000,
        }
    }

    /// Process a scale request and return the response.
    ///
    /// This determines how many instances to create or destroy
    /// but does not actually start/stop VMs — the caller is responsible
    /// for that based on the response.
    pub fn process_request(&mut self, request: &ScaleRequest) -> ScaleResponse {
        let service = &request.service;
        let desired = request.replicas;

        // Compute total instances in other services first (before mutable borrow)
        let total_other: u32 = self
            .services
            .iter()
            .filter(|(k, _)| k.as_str() != service)
            .map(|(_, v)| v.instances.len() as u32)
            .sum();

        // Get or create service entry
        let svc = self
            .services
            .entry(service.clone())
            .or_insert_with(|| ServiceInstances {
                target_replicas: 0,
                instances: Vec::new(),
            });

        let current = svc.instances.len() as u32;

        // Check capacity
        let available = self.max_instances.saturating_sub(total_other);
        let target = desired.min(available);

        svc.target_replicas = target;

        let accepted = target == desired;
        let error = if !accepted {
            Some(format!(
                "Capped to {} instances (max {} total, {} used by other services)",
                target, self.max_instances, total_other
            ))
        } else {
            None
        };

        let instances: Vec<InstanceInfo> = svc
            .instances
            .iter()
            .map(|inst| InstanceInfo {
                id: inst.id.clone(),
                state: inst.state,
                service: service.clone(),
                created_at: inst.created_at,
                ready_at: inst.ready_at,
                endpoint: inst.endpoint.clone(),
                health: inst.health.clone(),
            })
            .collect();

        ScaleResponse {
            request_id: request.request_id.clone(),
            accepted,
            current_replicas: current,
            target_replicas: target,
            instances,
            error,
        }
    }

    /// Register a new instance for a service.
    pub fn register_instance(
        &mut self,
        service: &str,
        instance_id: &str,
        endpoint: Option<&str>,
    ) {
        let svc = self
            .services
            .entry(service.to_string())
            .or_insert_with(|| ServiceInstances {
                target_replicas: 0,
                instances: Vec::new(),
            });

        // Don't register duplicates
        if svc.instances.iter().any(|i| i.id == instance_id) {
            return;
        }

        svc.instances.push(TrackedInstance {
            id: instance_id.to_string(),
            state: InstanceState::Creating,
            created_at: Utc::now(),
            ready_at: None,
            endpoint: endpoint.map(|s| s.to_string()),
            health: InstanceHealth::default(),
        });
    }

    /// Update an instance's state and emit a transition event.
    pub fn update_state(
        &mut self,
        service: &str,
        instance_id: &str,
        new_state: InstanceState,
    ) -> Option<InstanceEvent> {
        let svc = self.services.get_mut(service)?;
        let inst = svc.instances.iter_mut().find(|i| i.id == instance_id)?;

        let old_state = inst.state;
        if old_state == new_state {
            return None;
        }

        inst.state = new_state;
        if new_state == InstanceState::Ready && inst.ready_at.is_none() {
            inst.ready_at = Some(Utc::now());
        }

        let event = InstanceEvent::transition(instance_id, service, old_state, new_state);
        self.push_event(event.clone());
        Some(event)
    }

    /// Update an instance's health metrics.
    pub fn update_health(
        &mut self,
        service: &str,
        instance_id: &str,
        health: InstanceHealth,
    ) {
        if let Some(svc) = self.services.get_mut(service) {
            if let Some(inst) = svc.instances.iter_mut().find(|i| i.id == instance_id) {
                inst.health = health;
            }
        }
    }

    /// Update an instance's endpoint.
    pub fn update_endpoint(
        &mut self,
        service: &str,
        instance_id: &str,
        endpoint: &str,
    ) {
        if let Some(svc) = self.services.get_mut(service) {
            if let Some(inst) = svc.instances.iter_mut().find(|i| i.id == instance_id) {
                inst.endpoint = Some(endpoint.to_string());
            }
        }
    }

    /// Remove an instance from tracking.
    pub fn deregister_instance(&mut self, service: &str, instance_id: &str) -> bool {
        if let Some(svc) = self.services.get_mut(service) {
            let before = svc.instances.len();
            svc.instances.retain(|i| i.id != instance_id);
            return svc.instances.len() < before;
        }
        false
    }

    /// Get instances that need to be created (target > current running).
    pub fn instances_to_create(&self, service: &str) -> u32 {
        if let Some(svc) = self.services.get(service) {
            let active = svc
                .instances
                .iter()
                .filter(|i| !matches!(i.state, InstanceState::Stopped | InstanceState::Failed))
                .count() as u32;
            svc.target_replicas.saturating_sub(active)
        } else {
            0
        }
    }

    /// Get instances that should be stopped (current > target).
    /// Returns instance IDs to stop, preferring non-busy instances.
    pub fn instances_to_stop(&self, service: &str) -> Vec<String> {
        if let Some(svc) = self.services.get(service) {
            let active: Vec<&TrackedInstance> = svc
                .instances
                .iter()
                .filter(|i| !matches!(i.state, InstanceState::Stopped | InstanceState::Failed | InstanceState::Stopping | InstanceState::Draining))
                .collect();

            let excess = (active.len() as u32).saturating_sub(svc.target_replicas);
            if excess == 0 {
                return Vec::new();
            }

            // Prefer stopping idle (Ready) instances over Busy ones
            let mut candidates: Vec<&TrackedInstance> = active;
            candidates.sort_by_key(|i| match i.state {
                InstanceState::Ready => 0,    // Stop idle first
                InstanceState::Creating => 1, // Then creating
                InstanceState::Booting => 2,  // Then booting
                InstanceState::Busy => 3,     // Busy last
                _ => 4,
            });

            candidates
                .iter()
                .take(excess as usize)
                .map(|i| i.id.clone())
                .collect()
        } else {
            Vec::new()
        }
    }

    /// Get all ready instances for a service (for traffic routing).
    pub fn ready_instances(&self, service: &str) -> Vec<InstanceInfo> {
        if let Some(svc) = self.services.get(service) {
            svc.instances
                .iter()
                .filter(|i| i.state == InstanceState::Ready)
                .map(|i| InstanceInfo {
                    id: i.id.clone(),
                    state: i.state,
                    service: service.to_string(),
                    created_at: i.created_at,
                    ready_at: i.ready_at,
                    endpoint: i.endpoint.clone(),
                    health: i.health.clone(),
                })
                .collect()
        } else {
            Vec::new()
        }
    }

    /// Get the total number of instances across all services.
    pub fn total_instances(&self) -> u32 {
        self.services.values().map(|s| s.instances.len() as u32).sum()
    }

    /// Get the number of instances for a specific service.
    pub fn service_instance_count(&self, service: &str) -> u32 {
        self.services
            .get(service)
            .map(|s| s.instances.len() as u32)
            .unwrap_or(0)
    }

    /// List all tracked services.
    pub fn services(&self) -> Vec<String> {
        self.services.keys().cloned().collect()
    }

    /// Get recent events.
    pub fn recent_events(&self, limit: usize) -> &[InstanceEvent] {
        let start = self.events.len().saturating_sub(limit);
        &self.events[start..]
    }

    fn push_event(&mut self, event: InstanceEvent) {
        self.events.push(event);
        if self.events.len() > self.max_events {
            self.events.drain(..self.events.len() - self.max_events);
        }
    }

    /// Aggregate health metrics for a service (for autoscaler decisions).
    pub fn service_health(&self, service: &str) -> ServiceHealth {
        let svc = match self.services.get(service) {
            Some(s) => s,
            None => return ServiceHealth::default(),
        };

        let active: Vec<&TrackedInstance> = svc
            .instances
            .iter()
            .filter(|i| matches!(i.state, InstanceState::Ready | InstanceState::Busy))
            .collect();

        if active.is_empty() {
            return ServiceHealth {
                active_instances: 0,
                ready_instances: 0,
                busy_instances: 0,
                ..Default::default()
            };
        }

        let ready_count = active.iter().filter(|i| i.state == InstanceState::Ready).count() as u32;
        let busy_count = active.iter().filter(|i| i.state == InstanceState::Busy).count() as u32;

        let mut total_cpu = 0.0f64;
        let mut total_mem = 0u64;
        let mut total_inflight = 0u32;
        let mut cpu_count = 0u32;
        let mut unhealthy = 0u32;

        for inst in &active {
            if let Some(cpu) = inst.health.cpu_percent {
                total_cpu += cpu as f64;
                cpu_count += 1;
            }
            if let Some(mem) = inst.health.memory_bytes {
                total_mem += mem;
            }
            total_inflight += inst.health.inflight_requests;
            if !inst.health.healthy {
                unhealthy += 1;
            }
        }

        ServiceHealth {
            active_instances: active.len() as u32,
            ready_instances: ready_count,
            busy_instances: busy_count,
            avg_cpu_percent: if cpu_count > 0 {
                Some((total_cpu / cpu_count as f64) as f32)
            } else {
                None
            },
            total_memory_bytes: total_mem,
            total_inflight_requests: total_inflight,
            unhealthy_instances: unhealthy,
        }
    }

    /// Initiate graceful drain for an instance.
    ///
    /// Transitions the instance to `Draining` state. The caller should:
    /// 1. Stop routing new requests to this instance
    /// 2. Wait for in-flight requests to complete (or timeout)
    /// 3. Call `complete_drain()` to transition to `Stopping`
    pub fn start_drain(
        &mut self,
        service: &str,
        instance_id: &str,
    ) -> Option<InstanceEvent> {
        let svc = self.services.get_mut(service)?;
        let inst = svc.instances.iter_mut().find(|i| i.id == instance_id)?;

        // Can only drain from Ready or Busy
        if !matches!(inst.state, InstanceState::Ready | InstanceState::Busy) {
            return None;
        }

        let old_state = inst.state;
        inst.state = InstanceState::Draining;

        let event = InstanceEvent::transition(instance_id, service, old_state, InstanceState::Draining)
            .with_message("Graceful drain initiated");
        self.push_event(event.clone());
        Some(event)
    }

    /// Complete a drain and transition to Stopping.
    ///
    /// Called after in-flight requests have completed or the drain timeout expired.
    pub fn complete_drain(
        &mut self,
        service: &str,
        instance_id: &str,
    ) -> Option<InstanceEvent> {
        let svc = self.services.get_mut(service)?;
        let inst = svc.instances.iter_mut().find(|i| i.id == instance_id)?;

        if inst.state != InstanceState::Draining {
            return None;
        }

        inst.state = InstanceState::Stopping;

        let event = InstanceEvent::transition(instance_id, service, InstanceState::Draining, InstanceState::Stopping)
            .with_message("Drain complete, stopping instance");
        self.push_event(event.clone());
        Some(event)
    }

    /// Check if a draining instance has no in-flight requests.
    pub fn is_drain_complete(&self, service: &str, instance_id: &str) -> bool {
        if let Some(svc) = self.services.get(service) {
            if let Some(inst) = svc.instances.iter().find(|i| i.id == instance_id) {
                return inst.state == InstanceState::Draining
                    && inst.health.inflight_requests == 0;
            }
        }
        false
    }

    /// Get all instances currently draining.
    pub fn draining_instances(&self, service: &str) -> Vec<String> {
        if let Some(svc) = self.services.get(service) {
            svc.instances
                .iter()
                .filter(|i| i.state == InstanceState::Draining)
                .map(|i| i.id.clone())
                .collect()
        } else {
            Vec::new()
        }
    }
}

/// Aggregated health metrics for a service.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ServiceHealth {
    /// Number of active instances (Ready + Busy)
    pub active_instances: u32,
    /// Number of Ready instances
    pub ready_instances: u32,
    /// Number of Busy instances
    pub busy_instances: u32,
    /// Average CPU usage across active instances
    pub avg_cpu_percent: Option<f32>,
    /// Total memory usage across all active instances
    pub total_memory_bytes: u64,
    /// Total in-flight requests across all active instances
    pub total_inflight_requests: u32,
    /// Number of unhealthy instances
    pub unhealthy_instances: u32,
}

/// Registry for instance self-registration in standalone multi-node deployments.
///
/// Each Box host registers its instances with the registry so Gateway
/// can discover endpoints for traffic routing.
pub struct InstanceRegistry {
    /// Registered instances: instance_id → registration
    entries: HashMap<String, RegistryEntry>,
}

/// A registered instance entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct RegistryEntry {
    instance_id: String,
    service: String,
    endpoint: String,
    host_id: String,
    metadata: HashMap<String, String>,
    registered_at: DateTime<Utc>,
    last_heartbeat: DateTime<Utc>,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_scale_manager_new() {
        let mgr = ScaleManager::new(100);
        assert_eq!(mgr.total_instances(), 0);
        assert!(mgr.services().is_empty());
    }

    #[test]
    fn test_process_request_scale_up() {
        let mut mgr = ScaleManager::new(10);
        let req = ScaleRequest {
            service: "web".to_string(),
            replicas: 3,
            config: Default::default(),
            request_id: "r1".to_string(),
        };
        let resp = mgr.process_request(&req);
        assert!(resp.accepted);
        assert_eq!(resp.target_replicas, 3);
        assert_eq!(resp.current_replicas, 0);
    }

    #[test]
    fn test_process_request_capped_by_max() {
        let mut mgr = ScaleManager::new(5);

        // Register 3 instances for another service
        mgr.register_instance("other", "o1", None);
        mgr.register_instance("other", "o2", None);
        mgr.register_instance("other", "o3", None);

        let req = ScaleRequest {
            service: "web".to_string(),
            replicas: 5,
            config: Default::default(),
            request_id: "r2".to_string(),
        };
        let resp = mgr.process_request(&req);
        assert!(!resp.accepted); // Capped
        assert_eq!(resp.target_replicas, 2); // 5 max - 3 other = 2
        assert!(resp.error.is_some());
    }

    #[test]
    fn test_register_instance() {
        let mut mgr = ScaleManager::new(10);
        mgr.register_instance("web", "box-1", Some("10.0.0.1:8080"));
        assert_eq!(mgr.service_instance_count("web"), 1);
        assert_eq!(mgr.total_instances(), 1);
    }

    #[test]
    fn test_register_instance_no_duplicate() {
        let mut mgr = ScaleManager::new(10);
        mgr.register_instance("web", "box-1", None);
        mgr.register_instance("web", "box-1", None); // duplicate
        assert_eq!(mgr.service_instance_count("web"), 1);
    }

    #[test]
    fn test_update_state() {
        let mut mgr = ScaleManager::new(10);
        mgr.register_instance("web", "box-1", None);

        let event = mgr.update_state("web", "box-1", InstanceState::Booting);
        assert!(event.is_some());
        let e = event.unwrap();
        assert_eq!(e.from_state, InstanceState::Creating);
        assert_eq!(e.to_state, InstanceState::Booting);

        let event = mgr.update_state("web", "box-1", InstanceState::Ready);
        assert!(event.is_some());
        let e = event.unwrap();
        assert_eq!(e.from_state, InstanceState::Booting);
        assert_eq!(e.to_state, InstanceState::Ready);
    }

    #[test]
    fn test_update_state_same_state_no_event() {
        let mut mgr = ScaleManager::new(10);
        mgr.register_instance("web", "box-1", None);
        mgr.update_state("web", "box-1", InstanceState::Ready);

        let event = mgr.update_state("web", "box-1", InstanceState::Ready);
        assert!(event.is_none());
    }

    #[test]
    fn test_update_state_nonexistent() {
        let mut mgr = ScaleManager::new(10);
        let event = mgr.update_state("web", "nonexistent", InstanceState::Ready);
        assert!(event.is_none());
    }

    #[test]
    fn test_update_health() {
        let mut mgr = ScaleManager::new(10);
        mgr.register_instance("web", "box-1", None);
        mgr.update_state("web", "box-1", InstanceState::Ready);

        mgr.update_health("web", "box-1", InstanceHealth {
            cpu_percent: Some(50.0),
            memory_bytes: Some(256 * 1024 * 1024),
            inflight_requests: 2,
            healthy: true,
        });

        let ready = mgr.ready_instances("web");
        assert_eq!(ready.len(), 1);
        assert_eq!(ready[0].health.cpu_percent, Some(50.0));
        assert_eq!(ready[0].health.inflight_requests, 2);
    }

    #[test]
    fn test_update_endpoint() {
        let mut mgr = ScaleManager::new(10);
        mgr.register_instance("web", "box-1", None);
        mgr.update_endpoint("web", "box-1", "10.0.0.5:3000");

        let ready = mgr.ready_instances("web"); // Not ready yet
        assert!(ready.is_empty());

        mgr.update_state("web", "box-1", InstanceState::Ready);
        let ready = mgr.ready_instances("web");
        assert_eq!(ready[0].endpoint, Some("10.0.0.5:3000".to_string()));
    }

    #[test]
    fn test_deregister_instance() {
        let mut mgr = ScaleManager::new(10);
        mgr.register_instance("web", "box-1", None);
        mgr.register_instance("web", "box-2", None);

        assert!(mgr.deregister_instance("web", "box-1"));
        assert_eq!(mgr.service_instance_count("web"), 1);
        assert!(!mgr.deregister_instance("web", "box-1")); // Already removed
    }

    #[test]
    fn test_instances_to_create() {
        let mut mgr = ScaleManager::new(10);
        let req = ScaleRequest {
            service: "web".to_string(),
            replicas: 3,
            config: Default::default(),
            request_id: "".to_string(),
        };
        mgr.process_request(&req);

        assert_eq!(mgr.instances_to_create("web"), 3);

        mgr.register_instance("web", "box-1", None);
        assert_eq!(mgr.instances_to_create("web"), 2);

        mgr.register_instance("web", "box-2", None);
        mgr.register_instance("web", "box-3", None);
        assert_eq!(mgr.instances_to_create("web"), 0);
    }

    #[test]
    fn test_instances_to_stop() {
        let mut mgr = ScaleManager::new(10);

        // Start with 3 instances
        let req = ScaleRequest {
            service: "web".to_string(),
            replicas: 3,
            config: Default::default(),
            request_id: "".to_string(),
        };
        mgr.process_request(&req);
        mgr.register_instance("web", "box-1", None);
        mgr.register_instance("web", "box-2", None);
        mgr.register_instance("web", "box-3", None);
        mgr.update_state("web", "box-1", InstanceState::Ready);
        mgr.update_state("web", "box-2", InstanceState::Ready);
        mgr.update_state("web", "box-3", InstanceState::Busy);

        // Scale down to 1
        let req = ScaleRequest {
            service: "web".to_string(),
            replicas: 1,
            config: Default::default(),
            request_id: "".to_string(),
        };
        mgr.process_request(&req);

        let to_stop = mgr.instances_to_stop("web");
        assert_eq!(to_stop.len(), 2);
        // Ready instances should be stopped before Busy ones
        assert!(to_stop.contains(&"box-1".to_string()));
        assert!(to_stop.contains(&"box-2".to_string()));
        assert!(!to_stop.contains(&"box-3".to_string()));
    }

    #[test]
    fn test_ready_instances() {
        let mut mgr = ScaleManager::new(10);
        mgr.register_instance("web", "box-1", Some("10.0.0.1:80"));
        mgr.register_instance("web", "box-2", Some("10.0.0.2:80"));
        mgr.register_instance("web", "box-3", None);

        mgr.update_state("web", "box-1", InstanceState::Ready);
        mgr.update_state("web", "box-2", InstanceState::Ready);
        // box-3 still Creating

        let ready = mgr.ready_instances("web");
        assert_eq!(ready.len(), 2);
    }

    #[test]
    fn test_ready_instances_empty_service() {
        let mgr = ScaleManager::new(10);
        assert!(mgr.ready_instances("nonexistent").is_empty());
    }

    #[test]
    fn test_recent_events() {
        let mut mgr = ScaleManager::new(10);
        mgr.register_instance("web", "box-1", None);
        mgr.update_state("web", "box-1", InstanceState::Booting);
        mgr.update_state("web", "box-1", InstanceState::Ready);

        let events = mgr.recent_events(10);
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].to_state, InstanceState::Booting);
        assert_eq!(events[1].to_state, InstanceState::Ready);
    }

    #[test]
    fn test_recent_events_limited() {
        let mut mgr = ScaleManager::new(10);
        mgr.register_instance("web", "box-1", None);
        mgr.update_state("web", "box-1", InstanceState::Booting);
        mgr.update_state("web", "box-1", InstanceState::Ready);
        mgr.update_state("web", "box-1", InstanceState::Busy);

        let events = mgr.recent_events(2);
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].to_state, InstanceState::Ready);
        assert_eq!(events[1].to_state, InstanceState::Busy);
    }

    #[test]
    fn test_multiple_services() {
        let mut mgr = ScaleManager::new(20);
        mgr.register_instance("web", "w1", None);
        mgr.register_instance("web", "w2", None);
        mgr.register_instance("api", "a1", None);

        assert_eq!(mgr.service_instance_count("web"), 2);
        assert_eq!(mgr.service_instance_count("api"), 1);
        assert_eq!(mgr.total_instances(), 3);

        let services = mgr.services();
        assert_eq!(services.len(), 2);
        assert!(services.contains(&"web".to_string()));
        assert!(services.contains(&"api".to_string()));
    }

    #[test]
    fn test_ready_at_set_on_first_ready() {
        let mut mgr = ScaleManager::new(10);
        mgr.register_instance("web", "box-1", None);
        mgr.update_state("web", "box-1", InstanceState::Booting);
        mgr.update_state("web", "box-1", InstanceState::Ready);

        let ready = mgr.ready_instances("web");
        assert!(ready[0].ready_at.is_some());

        // Transition away and back — ready_at should not change
        mgr.update_state("web", "box-1", InstanceState::Busy);
        mgr.update_state("web", "box-1", InstanceState::Ready);
        let ready2 = mgr.ready_instances("web");
        assert_eq!(ready[0].ready_at, ready2[0].ready_at);
    }

    // ========== Service Health ==========

    #[test]
    fn test_service_health_empty() {
        let mgr = ScaleManager::new(10);
        let health = mgr.service_health("nonexistent");
        assert_eq!(health.active_instances, 0);
        assert_eq!(health.ready_instances, 0);
        assert_eq!(health.busy_instances, 0);
        assert!(health.avg_cpu_percent.is_none());
    }

    #[test]
    fn test_service_health_with_instances() {
        let mut mgr = ScaleManager::new(10);
        mgr.register_instance("web", "b1", None);
        mgr.register_instance("web", "b2", None);
        mgr.register_instance("web", "b3", None);

        mgr.update_state("web", "b1", InstanceState::Ready);
        mgr.update_state("web", "b2", InstanceState::Ready);
        mgr.update_state("web", "b3", InstanceState::Busy);

        mgr.update_health("web", "b1", InstanceHealth {
            cpu_percent: Some(30.0),
            memory_bytes: Some(100),
            inflight_requests: 0,
            healthy: true,
        });
        mgr.update_health("web", "b2", InstanceHealth {
            cpu_percent: Some(50.0),
            memory_bytes: Some(200),
            inflight_requests: 1,
            healthy: true,
        });
        mgr.update_health("web", "b3", InstanceHealth {
            cpu_percent: Some(80.0),
            memory_bytes: Some(300),
            inflight_requests: 5,
            healthy: false,
        });

        let health = mgr.service_health("web");
        assert_eq!(health.active_instances, 3);
        assert_eq!(health.ready_instances, 2);
        assert_eq!(health.busy_instances, 1);
        // avg cpu: (30 + 50 + 80) / 3 ≈ 53.33
        let avg = health.avg_cpu_percent.unwrap();
        assert!(avg > 53.0 && avg < 54.0);
        assert_eq!(health.total_memory_bytes, 600);
        assert_eq!(health.total_inflight_requests, 6);
        assert_eq!(health.unhealthy_instances, 1);
    }

    #[test]
    fn test_service_health_no_cpu_data() {
        let mut mgr = ScaleManager::new(10);
        mgr.register_instance("web", "b1", None);
        mgr.update_state("web", "b1", InstanceState::Ready);
        // No health update — cpu_percent is None

        let health = mgr.service_health("web");
        assert_eq!(health.active_instances, 1);
        assert!(health.avg_cpu_percent.is_none());
    }

    // ========== Graceful Drain ==========

    #[test]
    fn test_start_drain_from_ready() {
        let mut mgr = ScaleManager::new(10);
        mgr.register_instance("web", "b1", None);
        mgr.update_state("web", "b1", InstanceState::Ready);

        let event = mgr.start_drain("web", "b1");
        assert!(event.is_some());
        let e = event.unwrap();
        assert_eq!(e.from_state, InstanceState::Ready);
        assert_eq!(e.to_state, InstanceState::Draining);
        assert!(e.message.contains("drain"));
    }

    #[test]
    fn test_start_drain_from_busy() {
        let mut mgr = ScaleManager::new(10);
        mgr.register_instance("web", "b1", None);
        mgr.update_state("web", "b1", InstanceState::Ready);
        mgr.update_state("web", "b1", InstanceState::Busy);

        let event = mgr.start_drain("web", "b1");
        assert!(event.is_some());
        assert_eq!(event.unwrap().from_state, InstanceState::Busy);
    }

    #[test]
    fn test_start_drain_from_creating_fails() {
        let mut mgr = ScaleManager::new(10);
        mgr.register_instance("web", "b1", None);
        // Still in Creating state
        assert!(mgr.start_drain("web", "b1").is_none());
    }

    #[test]
    fn test_complete_drain() {
        let mut mgr = ScaleManager::new(10);
        mgr.register_instance("web", "b1", None);
        mgr.update_state("web", "b1", InstanceState::Ready);
        mgr.start_drain("web", "b1");

        let event = mgr.complete_drain("web", "b1");
        assert!(event.is_some());
        let e = event.unwrap();
        assert_eq!(e.from_state, InstanceState::Draining);
        assert_eq!(e.to_state, InstanceState::Stopping);
    }

    #[test]
    fn test_complete_drain_not_draining() {
        let mut mgr = ScaleManager::new(10);
        mgr.register_instance("web", "b1", None);
        mgr.update_state("web", "b1", InstanceState::Ready);
        // Not draining
        assert!(mgr.complete_drain("web", "b1").is_none());
    }

    #[test]
    fn test_is_drain_complete() {
        let mut mgr = ScaleManager::new(10);
        mgr.register_instance("web", "b1", None);
        mgr.update_state("web", "b1", InstanceState::Ready);

        // Set inflight to 0
        mgr.update_health("web", "b1", InstanceHealth {
            inflight_requests: 0,
            ..Default::default()
        });
        mgr.start_drain("web", "b1");

        assert!(mgr.is_drain_complete("web", "b1"));
    }

    #[test]
    fn test_is_drain_complete_with_inflight() {
        let mut mgr = ScaleManager::new(10);
        mgr.register_instance("web", "b1", None);
        mgr.update_state("web", "b1", InstanceState::Ready);

        mgr.update_health("web", "b1", InstanceHealth {
            inflight_requests: 3,
            ..Default::default()
        });
        mgr.start_drain("web", "b1");

        assert!(!mgr.is_drain_complete("web", "b1"));
    }

    #[test]
    fn test_draining_instances() {
        let mut mgr = ScaleManager::new(10);
        mgr.register_instance("web", "b1", None);
        mgr.register_instance("web", "b2", None);
        mgr.register_instance("web", "b3", None);
        mgr.update_state("web", "b1", InstanceState::Ready);
        mgr.update_state("web", "b2", InstanceState::Ready);
        mgr.update_state("web", "b3", InstanceState::Ready);

        mgr.start_drain("web", "b1");
        mgr.start_drain("web", "b3");

        let draining = mgr.draining_instances("web");
        assert_eq!(draining.len(), 2);
        assert!(draining.contains(&"b1".to_string()));
        assert!(draining.contains(&"b3".to_string()));
    }

    // ========== Instance Registry ==========

    #[test]
    fn test_registry_new() {
        let reg = InstanceRegistry::new();
        assert!(reg.is_empty());
        assert_eq!(reg.len(), 0);
    }

    #[test]
    fn test_registry_register() {
        let mut reg = InstanceRegistry::new();
        reg.register("i1", "web", "10.0.0.1:80", "host-a", HashMap::new());
        assert_eq!(reg.len(), 1);
        assert!(!reg.is_empty());
    }

    #[test]
    fn test_registry_deregister() {
        let mut reg = InstanceRegistry::new();
        reg.register("i1", "web", "10.0.0.1:80", "host-a", HashMap::new());
        assert!(reg.deregister("i1"));
        assert!(reg.is_empty());
        assert!(!reg.deregister("i1")); // Already removed
    }

    #[test]
    fn test_registry_endpoints() {
        let mut reg = InstanceRegistry::new();
        reg.register("i1", "web", "10.0.0.1:80", "host-a", HashMap::new());
        reg.register("i2", "web", "10.0.0.2:80", "host-b", HashMap::new());
        reg.register("i3", "api", "10.0.0.3:3000", "host-a", HashMap::new());

        let web_eps = reg.endpoints("web");
        assert_eq!(web_eps.len(), 2);
        assert!(web_eps.contains(&"10.0.0.1:80".to_string()));
        assert!(web_eps.contains(&"10.0.0.2:80".to_string()));

        let api_eps = reg.endpoints("api");
        assert_eq!(api_eps.len(), 1);
    }

    #[test]
    fn test_registry_instances_for_service() {
        let mut reg = InstanceRegistry::new();
        reg.register("i1", "web", "10.0.0.1:80", "host-a", HashMap::new());
        reg.register("i2", "web", "10.0.0.2:80", "host-b", HashMap::new());
        reg.register("i3", "api", "10.0.0.3:3000", "host-a", HashMap::new());

        let web = reg.instances_for_service("web");
        assert_eq!(web.len(), 2);
    }

    #[test]
    fn test_registry_instances_on_host() {
        let mut reg = InstanceRegistry::new();
        reg.register("i1", "web", "10.0.0.1:80", "host-a", HashMap::new());
        reg.register("i2", "api", "10.0.0.2:3000", "host-a", HashMap::new());
        reg.register("i3", "web", "10.0.0.3:80", "host-b", HashMap::new());

        let host_a = reg.instances_on_host("host-a");
        assert_eq!(host_a.len(), 2);

        let host_b = reg.instances_on_host("host-b");
        assert_eq!(host_b.len(), 1);
    }

    #[test]
    fn test_registry_heartbeat() {
        let mut reg = InstanceRegistry::new();
        reg.register("i1", "web", "10.0.0.1:80", "host-a", HashMap::new());

        assert!(reg.heartbeat("i1"));
        assert!(!reg.heartbeat("nonexistent"));
    }

    #[test]
    fn test_registry_evict_stale() {
        let mut reg = InstanceRegistry::new();
        reg.register("i1", "web", "10.0.0.1:80", "host-a", HashMap::new());
        reg.register("i2", "web", "10.0.0.2:80", "host-b", HashMap::new());

        // Manually backdate i1's heartbeat
        if let Some(entry) = reg.entries.get_mut("i1") {
            entry.last_heartbeat = Utc::now() - chrono::Duration::seconds(120);
        }

        // Evict entries older than 60 seconds
        let evicted = reg.evict_stale(chrono::Duration::seconds(60));
        assert_eq!(evicted.len(), 1);
        assert_eq!(evicted[0], "i1");
        assert_eq!(reg.len(), 1);
    }

    #[test]
    fn test_registry_services() {
        let mut reg = InstanceRegistry::new();
        reg.register("i1", "web", "10.0.0.1:80", "host-a", HashMap::new());
        reg.register("i2", "api", "10.0.0.2:3000", "host-a", HashMap::new());
        reg.register("i3", "web", "10.0.0.3:80", "host-b", HashMap::new());

        let svcs = reg.services();
        assert_eq!(svcs.len(), 2);
        assert!(svcs.contains(&"api".to_string()));
        assert!(svcs.contains(&"web".to_string()));
    }

    #[test]
    fn test_registry_register_overwrites() {
        let mut reg = InstanceRegistry::new();
        reg.register("i1", "web", "10.0.0.1:80", "host-a", HashMap::new());
        reg.register("i1", "web", "10.0.0.1:8080", "host-a", HashMap::new()); // New endpoint

        assert_eq!(reg.len(), 1);
        let eps = reg.endpoints("web");
        assert_eq!(eps[0], "10.0.0.1:8080");
    }

    #[test]
    fn test_service_health_serde() {
        let health = ServiceHealth {
            active_instances: 3,
            ready_instances: 2,
            busy_instances: 1,
            avg_cpu_percent: Some(45.5),
            total_memory_bytes: 1024 * 1024 * 512,
            total_inflight_requests: 10,
            unhealthy_instances: 0,
        };
        let json = serde_json::to_string(&health).unwrap();
        let parsed: ServiceHealth = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.active_instances, 3);
        assert_eq!(parsed.avg_cpu_percent, Some(45.5));
        assert_eq!(parsed.total_inflight_requests, 10);
    }
}
