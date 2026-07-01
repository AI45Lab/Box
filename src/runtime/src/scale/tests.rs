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
fn test_process_request_capacity_excludes_same_service_instances() {
    let mut mgr = ScaleManager::new(5);
    mgr.register_instance("web", "w1", None);
    mgr.register_instance("web", "w2", None);
    mgr.register_instance("web", "w3", None);
    mgr.register_instance("api", "a1", None);

    let req = ScaleRequest {
        service: "web".to_string(),
        replicas: 5,
        config: Default::default(),
        request_id: "same-service-capacity".to_string(),
    };

    let resp = mgr.process_request(&req);

    assert!(!resp.accepted);
    assert_eq!(resp.current_replicas, 3);
    assert_eq!(resp.target_replicas, 4);
    assert!(resp
        .error
        .as_deref()
        .unwrap()
        .contains("1 used by other services"));
}

#[test]
fn test_process_request_returns_current_instance_snapshot() {
    let mut mgr = ScaleManager::new(10);
    mgr.register_instance("web", "box-1", Some("10.0.0.1:8080"));
    mgr.update_state("web", "box-1", InstanceState::Ready);
    mgr.update_health(
        "web",
        "box-1",
        InstanceHealth {
            cpu_percent: Some(25.0),
            memory_bytes: Some(128 * 1024 * 1024),
            inflight_requests: 4,
            healthy: true,
        },
    );

    let req = ScaleRequest {
        service: "web".to_string(),
        replicas: 2,
        config: Default::default(),
        request_id: "snapshot-request".to_string(),
    };

    let resp = mgr.process_request(&req);

    assert!(resp.accepted);
    assert_eq!(resp.request_id, "snapshot-request");
    assert_eq!(resp.current_replicas, 1);
    assert_eq!(resp.target_replicas, 2);
    assert_eq!(resp.instances.len(), 1);

    let instance = &resp.instances[0];
    assert_eq!(instance.id, "box-1");
    assert_eq!(instance.service, "web");
    assert_eq!(instance.state, InstanceState::Ready);
    assert_eq!(instance.endpoint, Some("10.0.0.1:8080".to_string()));
    assert!(instance.ready_at.is_some());
    assert_eq!(instance.health.cpu_percent, Some(25.0));
    assert_eq!(instance.health.memory_bytes, Some(128 * 1024 * 1024));
    assert_eq!(instance.health.inflight_requests, 4);
    assert!(instance.health.healthy);
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

    mgr.update_health(
        "web",
        "box-1",
        InstanceHealth {
            cpu_percent: Some(50.0),
            memory_bytes: Some(256 * 1024 * 1024),
            inflight_requests: 2,
            healthy: true,
        },
    );

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
fn test_update_health_and_endpoint_ignore_missing_targets() {
    let mut mgr = ScaleManager::new(10);
    mgr.update_health(
        "missing",
        "box-1",
        InstanceHealth {
            cpu_percent: Some(99.0),
            ..Default::default()
        },
    );
    mgr.update_endpoint("missing", "box-1", "10.0.0.9:8080");
    assert_eq!(mgr.total_instances(), 0);
    assert!(mgr.services().is_empty());

    mgr.register_instance("web", "box-1", Some("10.0.0.1:8080"));
    mgr.update_state("web", "box-1", InstanceState::Ready);
    mgr.update_health(
        "web",
        "missing",
        InstanceHealth {
            cpu_percent: Some(99.0),
            inflight_requests: 10,
            ..Default::default()
        },
    );
    mgr.update_endpoint("web", "missing", "10.0.0.9:8080");

    let ready = mgr.ready_instances("web");
    assert_eq!(ready.len(), 1);
    assert_eq!(ready[0].endpoint, Some("10.0.0.1:8080".to_string()));
    assert_eq!(ready[0].health.cpu_percent, None);
    assert_eq!(ready[0].health.inflight_requests, 0);
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
fn test_deregister_instance_missing_service() {
    let mut mgr = ScaleManager::new(10);
    assert!(!mgr.deregister_instance("missing", "box-1"));
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
fn test_instances_to_create_ignores_terminal_instances() {
    let mut mgr = ScaleManager::new(10);
    let req = ScaleRequest {
        service: "web".to_string(),
        replicas: 3,
        config: Default::default(),
        request_id: "".to_string(),
    };
    mgr.process_request(&req);

    for id in ["ready", "busy", "stopped", "failed"] {
        mgr.register_instance("web", id, None);
    }
    mgr.update_state("web", "ready", InstanceState::Ready);
    mgr.update_state("web", "busy", InstanceState::Busy);
    mgr.update_state("web", "stopped", InstanceState::Booting);
    mgr.update_state("web", "stopped", InstanceState::Stopped);
    mgr.update_state("web", "failed", InstanceState::Booting);
    mgr.update_state("web", "failed", InstanceState::Failed);

    assert_eq!(mgr.service_instance_count("web"), 4);
    assert_eq!(mgr.instances_to_create("web"), 1);
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
fn test_instances_to_stop_orders_idle_and_starting_before_busy() {
    let mut mgr = ScaleManager::new(10);
    let req = ScaleRequest {
        service: "web".to_string(),
        replicas: 5,
        config: Default::default(),
        request_id: "".to_string(),
    };
    mgr.process_request(&req);

    for id in [
        "ready", "creating", "booting", "busy", "draining", "stopping",
    ] {
        mgr.register_instance("web", id, None);
    }
    mgr.update_state("web", "ready", InstanceState::Ready);
    // "creating" remains Creating.
    mgr.update_state("web", "booting", InstanceState::Booting);
    mgr.update_state("web", "busy", InstanceState::Busy);
    mgr.update_state("web", "draining", InstanceState::Ready);
    mgr.start_drain("web", "draining");
    mgr.update_state("web", "stopping", InstanceState::Booting);
    mgr.update_state("web", "stopping", InstanceState::Stopping);

    let req = ScaleRequest {
        service: "web".to_string(),
        replicas: 1,
        config: Default::default(),
        request_id: "".to_string(),
    };
    mgr.process_request(&req);

    assert_eq!(
        mgr.instances_to_stop("web"),
        vec![
            "ready".to_string(),
            "creating".to_string(),
            "booting".to_string()
        ]
    );
}

#[test]
fn test_instances_to_stop_missing_service_no_excess_and_terminal_states() {
    let mut mgr = ScaleManager::new(10);
    assert!(mgr.instances_to_stop("missing").is_empty());

    let req = ScaleRequest {
        service: "web".to_string(),
        replicas: 2,
        config: Default::default(),
        request_id: "".to_string(),
    };
    mgr.process_request(&req);
    mgr.register_instance("web", "ready", None);
    mgr.register_instance("web", "busy", None);
    mgr.register_instance("web", "stopping", None);
    mgr.register_instance("web", "draining", None);
    mgr.register_instance("web", "stopped", None);
    mgr.register_instance("web", "failed", None);

    mgr.update_state("web", "ready", InstanceState::Ready);
    mgr.update_state("web", "busy", InstanceState::Busy);
    mgr.update_state("web", "stopping", InstanceState::Booting);
    mgr.update_state("web", "stopping", InstanceState::Stopping);
    mgr.update_state("web", "draining", InstanceState::Ready);
    mgr.start_drain("web", "draining");
    mgr.update_state("web", "stopped", InstanceState::Booting);
    mgr.update_state("web", "stopped", InstanceState::Stopped);
    mgr.update_state("web", "failed", InstanceState::Booting);
    mgr.update_state("web", "failed", InstanceState::Failed);

    assert!(mgr.instances_to_stop("web").is_empty());

    let req = ScaleRequest {
        service: "web".to_string(),
        replicas: 1,
        config: Default::default(),
        request_id: "".to_string(),
    };
    mgr.process_request(&req);

    assert_eq!(mgr.instances_to_stop("web"), vec!["ready".to_string()]);
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
fn test_ready_instances_excludes_busy_draining_and_terminal_states() {
    let mut mgr = ScaleManager::new(10);
    for id in ["ready", "busy", "draining", "stopped", "failed"] {
        mgr.register_instance("web", id, Some(&format!("{id}.svc:80")));
    }

    mgr.update_state("web", "ready", InstanceState::Ready);
    mgr.update_state("web", "busy", InstanceState::Busy);
    mgr.update_state("web", "draining", InstanceState::Ready);
    mgr.start_drain("web", "draining");
    mgr.update_state("web", "stopped", InstanceState::Booting);
    mgr.update_state("web", "stopped", InstanceState::Stopped);
    mgr.update_state("web", "failed", InstanceState::Booting);
    mgr.update_state("web", "failed", InstanceState::Failed);

    let ready = mgr.ready_instances("web");

    assert_eq!(ready.len(), 1);
    assert_eq!(ready[0].id, "ready");
    assert_eq!(ready[0].service, "web");
    assert_eq!(ready[0].endpoint, Some("ready.svc:80".to_string()));
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
fn test_recent_events_zero_limit() {
    let mut mgr = ScaleManager::new(10);
    mgr.register_instance("web", "box-1", None);
    mgr.update_state("web", "box-1", InstanceState::Booting);

    assert!(mgr.recent_events(0).is_empty());
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
fn test_recent_events_are_bounded_to_ring_buffer_capacity() {
    let mut mgr = ScaleManager::new(2_000);

    for i in 0..1_005 {
        let instance_id = format!("box-{i}");
        mgr.register_instance("web", &instance_id, None);
        mgr.update_state("web", &instance_id, InstanceState::Booting);
    }

    let events = mgr.recent_events(usize::MAX);

    assert_eq!(events.len(), 1_000);
    assert_eq!(events[0].instance_id, "box-5");
    assert_eq!(events[999].instance_id, "box-1004");
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

    mgr.update_health(
        "web",
        "b1",
        InstanceHealth {
            cpu_percent: Some(30.0),
            memory_bytes: Some(100),
            inflight_requests: 0,
            healthy: true,
        },
    );
    mgr.update_health(
        "web",
        "b2",
        InstanceHealth {
            cpu_percent: Some(50.0),
            memory_bytes: Some(200),
            inflight_requests: 1,
            healthy: true,
        },
    );
    mgr.update_health(
        "web",
        "b3",
        InstanceHealth {
            cpu_percent: Some(80.0),
            memory_bytes: Some(300),
            inflight_requests: 5,
            healthy: false,
        },
    );

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

#[test]
fn test_service_health_counts_only_ready_and_busy_instances() {
    let mut mgr = ScaleManager::new(10);
    for id in [
        "creating", "booting", "ready", "busy", "draining", "stopping", "stopped", "failed",
    ] {
        mgr.register_instance("web", id, None);
    }

    mgr.update_state("web", "booting", InstanceState::Booting);
    mgr.update_state("web", "ready", InstanceState::Ready);
    mgr.update_state("web", "busy", InstanceState::Busy);
    mgr.update_state("web", "draining", InstanceState::Ready);
    mgr.start_drain("web", "draining");
    mgr.update_state("web", "stopping", InstanceState::Booting);
    mgr.update_state("web", "stopping", InstanceState::Stopping);
    mgr.update_state("web", "stopped", InstanceState::Booting);
    mgr.update_state("web", "stopped", InstanceState::Stopped);
    mgr.update_state("web", "failed", InstanceState::Booting);
    mgr.update_state("web", "failed", InstanceState::Failed);

    for id in [
        "creating", "booting", "draining", "stopping", "stopped", "failed",
    ] {
        mgr.update_health(
            "web",
            id,
            InstanceHealth {
                cpu_percent: Some(100.0),
                memory_bytes: Some(1_000),
                inflight_requests: 50,
                healthy: false,
            },
        );
    }
    mgr.update_health(
        "web",
        "ready",
        InstanceHealth {
            cpu_percent: Some(20.0),
            memory_bytes: Some(200),
            inflight_requests: 1,
            healthy: true,
        },
    );
    mgr.update_health(
        "web",
        "busy",
        InstanceHealth {
            cpu_percent: Some(60.0),
            memory_bytes: Some(600),
            inflight_requests: 3,
            healthy: false,
        },
    );

    let health = mgr.service_health("web");
    assert_eq!(health.active_instances, 2);
    assert_eq!(health.ready_instances, 1);
    assert_eq!(health.busy_instances, 1);
    assert_eq!(health.avg_cpu_percent, Some(40.0));
    assert_eq!(health.total_memory_bytes, 800);
    assert_eq!(health.total_inflight_requests, 4);
    assert_eq!(health.unhealthy_instances, 1);
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
    mgr.update_health(
        "web",
        "b1",
        InstanceHealth {
            inflight_requests: 0,
            ..Default::default()
        },
    );
    mgr.start_drain("web", "b1");

    assert!(mgr.is_drain_complete("web", "b1"));
}

#[test]
fn test_is_drain_complete_with_inflight() {
    let mut mgr = ScaleManager::new(10);
    mgr.register_instance("web", "b1", None);
    mgr.update_state("web", "b1", InstanceState::Ready);

    mgr.update_health(
        "web",
        "b1",
        InstanceHealth {
            inflight_requests: 3,
            ..Default::default()
        },
    );
    mgr.start_drain("web", "b1");

    assert!(!mgr.is_drain_complete("web", "b1"));
}

#[test]
fn test_drain_operations_ignore_missing_and_non_draining_instances() {
    let mut mgr = ScaleManager::new(10);
    assert!(mgr.start_drain("missing", "box-1").is_none());
    assert!(mgr.complete_drain("missing", "box-1").is_none());
    assert!(!mgr.is_drain_complete("missing", "box-1"));
    assert!(mgr.draining_instances("missing").is_empty());

    mgr.register_instance("web", "box-1", None);
    assert!(mgr.start_drain("web", "missing").is_none());
    assert!(mgr.complete_drain("web", "missing").is_none());
    assert!(!mgr.is_drain_complete("web", "missing"));

    mgr.update_state("web", "box-1", InstanceState::Ready);
    assert!(!mgr.is_drain_complete("web", "box-1"));
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
