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
}
