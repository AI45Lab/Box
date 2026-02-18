//! ScaleManager implementation.

use std::collections::HashMap;

use a3s_box_core::scale::{
    InstanceEvent, InstanceHealth, InstanceInfo, InstanceState, ScaleRequest, ScaleResponse,
};
use chrono::Utc;

use super::{ServiceHealth, ServiceInstances, TrackedInstance};

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
    pub fn register_instance(&mut self, service: &str, instance_id: &str, endpoint: Option<&str>) {
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
    pub fn update_health(&mut self, service: &str, instance_id: &str, health: InstanceHealth) {
        if let Some(svc) = self.services.get_mut(service) {
            if let Some(inst) = svc.instances.iter_mut().find(|i| i.id == instance_id) {
                inst.health = health;
            }
        }
    }

    /// Update an instance's endpoint.
    pub fn update_endpoint(&mut self, service: &str, instance_id: &str, endpoint: &str) {
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
                .filter(|i| {
                    !matches!(
                        i.state,
                        InstanceState::Stopped
                            | InstanceState::Failed
                            | InstanceState::Stopping
                            | InstanceState::Draining
                    )
                })
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
        self.services
            .values()
            .map(|s| s.instances.len() as u32)
            .sum()
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

        let ready_count = active
            .iter()
            .filter(|i| i.state == InstanceState::Ready)
            .count() as u32;
        let busy_count = active
            .iter()
            .filter(|i| i.state == InstanceState::Busy)
            .count() as u32;

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
    pub fn start_drain(&mut self, service: &str, instance_id: &str) -> Option<InstanceEvent> {
        let svc = self.services.get_mut(service)?;
        let inst = svc.instances.iter_mut().find(|i| i.id == instance_id)?;

        // Can only drain from Ready or Busy
        if !matches!(inst.state, InstanceState::Ready | InstanceState::Busy) {
            return None;
        }

        let old_state = inst.state;
        inst.state = InstanceState::Draining;

        let event =
            InstanceEvent::transition(instance_id, service, old_state, InstanceState::Draining)
                .with_message("Graceful drain initiated");
        self.push_event(event.clone());
        Some(event)
    }

    /// Complete a drain and transition to Stopping.
    ///
    /// Called after in-flight requests have completed or the drain timeout expired.
    pub fn complete_drain(&mut self, service: &str, instance_id: &str) -> Option<InstanceEvent> {
        let svc = self.services.get_mut(service)?;
        let inst = svc.instances.iter_mut().find(|i| i.id == instance_id)?;

        if inst.state != InstanceState::Draining {
            return None;
        }

        inst.state = InstanceState::Stopping;

        let event = InstanceEvent::transition(
            instance_id,
            service,
            InstanceState::Draining,
            InstanceState::Stopping,
        )
        .with_message("Drain complete, stopping instance");
        self.push_event(event.clone());
        Some(event)
    }

    /// Check if a draining instance has no in-flight requests.
    pub fn is_drain_complete(&self, service: &str, instance_id: &str) -> bool {
        if let Some(svc) = self.services.get(service) {
            if let Some(inst) = svc.instances.iter().find(|i| i.id == instance_id) {
                return inst.state == InstanceState::Draining && inst.health.inflight_requests == 0;
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
