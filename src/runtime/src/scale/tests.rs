//! Tests for scale module.

use std::collections::HashMap;

use a3s_box_core::scale::{InstanceHealth, InstanceState, ScaleRequest};
use chrono::Utc;

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
