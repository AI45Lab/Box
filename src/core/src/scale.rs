//! Scale API types for Gateway ↔ Box communication.
//!
//! Defines the request/response types for the internal Scale API that
//! Gateway uses to request instance scale-up/scale-down in standalone mode.
//!
//! ## Protocol
//!
//! Gateway → Box: `ScaleRequest` (service, desired replicas, config)
//! Box → Gateway: `ScaleResponse` (actual replicas, instance states)
//! Box → Gateway: `InstanceEvent` (state transitions, health updates)

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Instance lifecycle state for readiness signaling.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum InstanceState {
    /// Instance is being created (image pull, rootfs build)
    Creating,
    /// VM is booting (kernel + init)
    Booting,
    /// Agent is ready, accepting requests
    Ready,
    /// Instance is actively processing a request
    Busy,
    /// Instance is draining in-flight requests before shutdown
    Draining,
    /// Instance is shutting down
    Stopping,
    /// Instance has terminated
    Stopped,
    /// Instance failed to start or crashed
    Failed,
}

impl std::fmt::Display for InstanceState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Creating => write!(f, "creating"),
            Self::Booting => write!(f, "booting"),
            Self::Ready => write!(f, "ready"),
            Self::Busy => write!(f, "busy"),
            Self::Draining => write!(f, "draining"),
            Self::Stopping => write!(f, "stopping"),
            Self::Stopped => write!(f, "stopped"),
            Self::Failed => write!(f, "failed"),
        }
    }
}

/// Request from Gateway to scale a service.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScaleRequest {
    /// Service identifier
    pub service: String,
    /// Desired number of running instances
    pub replicas: u32,
    /// Instance configuration overrides
    #[serde(default)]
    pub config: ScaleConfig,
    /// Request ID for correlation
    #[serde(default)]
    pub request_id: String,
}

/// Instance configuration for scale requests.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ScaleConfig {
    /// OCI image to use (overrides service default)
    #[serde(default)]
    pub image: Option<String>,
    /// vCPUs per instance
    #[serde(default)]
    pub vcpus: Option<u8>,
    /// Memory in MiB per instance
    #[serde(default)]
    pub memory_mib: Option<u32>,
    /// Environment variables
    #[serde(default)]
    pub env: HashMap<String, String>,
    /// Port mappings
    #[serde(default)]
    pub port_map: Vec<String>,
}

/// Response from Box after processing a scale request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScaleResponse {
    /// Request ID for correlation
    pub request_id: String,
    /// Whether the scale operation was accepted
    pub accepted: bool,
    /// Current number of running instances
    pub current_replicas: u32,
    /// Target number of instances (may differ from requested if at capacity)
    pub target_replicas: u32,
    /// Per-instance status
    pub instances: Vec<InstanceInfo>,
    /// Error message if not accepted
    #[serde(default)]
    pub error: Option<String>,
}

/// Information about a single instance.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstanceInfo {
    /// Instance (box) ID
    pub id: String,
    /// Current state
    pub state: InstanceState,
    /// Service this instance belongs to
    pub service: String,
    /// When the instance was created
    pub created_at: DateTime<Utc>,
    /// When the instance became ready (None if not yet ready)
    #[serde(default)]
    pub ready_at: Option<DateTime<Utc>>,
    /// Instance endpoint (host:port) for traffic routing
    #[serde(default)]
    pub endpoint: Option<String>,
    /// Health metrics
    #[serde(default)]
    pub health: InstanceHealth,
}

/// Health metrics for an instance.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstanceHealth {
    /// CPU usage percentage (0-100)
    #[serde(default)]
    pub cpu_percent: Option<f32>,
    /// Memory usage in bytes
    #[serde(default)]
    pub memory_bytes: Option<u64>,
    /// Number of in-flight requests
    #[serde(default)]
    pub inflight_requests: u32,
    /// Whether the instance is healthy
    #[serde(default = "default_true")]
    pub healthy: bool,
}

impl Default for InstanceHealth {
    fn default() -> Self {
        Self {
            cpu_percent: None,
            memory_bytes: None,
            inflight_requests: 0,
            healthy: true,
        }
    }
}

fn default_true() -> bool {
    true
}

/// Event emitted by Box when an instance state changes.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstanceEvent {
    /// Instance (box) ID
    pub instance_id: String,
    /// Service this instance belongs to
    pub service: String,
    /// Previous state
    pub from_state: InstanceState,
    /// New state
    pub to_state: InstanceState,
    /// When the transition occurred
    pub timestamp: DateTime<Utc>,
    /// Optional message (e.g., error details for Failed state)
    #[serde(default)]
    pub message: String,
}

impl InstanceEvent {
    /// Create a new state transition event.
    pub fn transition(
        instance_id: &str,
        service: &str,
        from: InstanceState,
        to: InstanceState,
    ) -> Self {
        Self {
            instance_id: instance_id.to_string(),
            service: service.to_string(),
            from_state: from,
            to_state: to,
            timestamp: Utc::now(),
            message: String::new(),
        }
    }

    /// Add a message to the event.
    pub fn with_message(mut self, msg: &str) -> Self {
        self.message = msg.to_string();
        self
    }
}

/// Registration payload for instance self-registration with Gateway.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstanceRegistration {
    /// Instance (box) ID
    pub instance_id: String,
    /// Service this instance belongs to
    pub service: String,
    /// Endpoint for traffic routing (host:port)
    pub endpoint: String,
    /// Instance metadata
    #[serde(default)]
    pub metadata: HashMap<String, String>,
    /// When the instance started
    pub started_at: DateTime<Utc>,
}

/// Deregistration payload when an instance is shutting down.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstanceDeregistration {
    /// Instance (box) ID
    pub instance_id: String,
    /// Service this instance belongs to
    pub service: String,
    /// Reason for deregistration
    #[serde(default)]
    pub reason: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_instance_state_display() {
        assert_eq!(InstanceState::Creating.to_string(), "creating");
        assert_eq!(InstanceState::Booting.to_string(), "booting");
        assert_eq!(InstanceState::Ready.to_string(), "ready");
        assert_eq!(InstanceState::Busy.to_string(), "busy");
        assert_eq!(InstanceState::Draining.to_string(), "draining");
        assert_eq!(InstanceState::Stopping.to_string(), "stopping");
        assert_eq!(InstanceState::Stopped.to_string(), "stopped");
        assert_eq!(InstanceState::Failed.to_string(), "failed");
    }

    #[test]
    fn test_scale_request_serde() {
        let req = ScaleRequest {
            service: "my-service".to_string(),
            replicas: 3,
            config: ScaleConfig {
                image: Some("nginx:latest".to_string()),
                vcpus: Some(2),
                memory_mib: Some(512),
                env: HashMap::from([("PORT".to_string(), "8080".to_string())]),
                port_map: vec!["8080:80".to_string()],
            },
            request_id: "req-001".to_string(),
        };
        let json = serde_json::to_string(&req).unwrap();
        let parsed: ScaleRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.service, "my-service");
        assert_eq!(parsed.replicas, 3);
        assert_eq!(parsed.config.image, Some("nginx:latest".to_string()));
        assert_eq!(parsed.config.vcpus, Some(2));
        assert_eq!(parsed.config.env.get("PORT").unwrap(), "8080");
    }

    #[test]
    fn test_scale_request_minimal() {
        let json = r#"{"service":"svc","replicas":1}"#;
        let req: ScaleRequest = serde_json::from_str(json).unwrap();
        assert_eq!(req.service, "svc");
        assert_eq!(req.replicas, 1);
        assert!(req.config.image.is_none());
        assert!(req.request_id.is_empty());
    }

    #[test]
    fn test_scale_response_accepted() {
        let resp = ScaleResponse {
            request_id: "req-001".to_string(),
            accepted: true,
            current_replicas: 2,
            target_replicas: 3,
            instances: vec![InstanceInfo {
                id: "box-1".to_string(),
                state: InstanceState::Ready,
                service: "svc".to_string(),
                created_at: Utc::now(),
                ready_at: Some(Utc::now()),
                endpoint: Some("10.0.0.2:8080".to_string()),
                health: InstanceHealth::default(),
            }],
            error: None,
        };
        let json = serde_json::to_string(&resp).unwrap();
        let parsed: ScaleResponse = serde_json::from_str(&json).unwrap();
        assert!(parsed.accepted);
        assert_eq!(parsed.current_replicas, 2);
        assert_eq!(parsed.target_replicas, 3);
        assert_eq!(parsed.instances.len(), 1);
        assert_eq!(parsed.instances[0].state, InstanceState::Ready);
    }

    #[test]
    fn test_scale_response_rejected() {
        let resp = ScaleResponse {
            request_id: "req-002".to_string(),
            accepted: false,
            current_replicas: 5,
            target_replicas: 5,
            instances: vec![],
            error: Some("At maximum capacity".to_string()),
        };
        let json = serde_json::to_string(&resp).unwrap();
        let parsed: ScaleResponse = serde_json::from_str(&json).unwrap();
        assert!(!parsed.accepted);
        assert_eq!(parsed.error, Some("At maximum capacity".to_string()));
    }

    #[test]
    fn test_instance_info_serde() {
        let info = InstanceInfo {
            id: "box-abc".to_string(),
            state: InstanceState::Busy,
            service: "api".to_string(),
            created_at: Utc::now(),
            ready_at: Some(Utc::now()),
            endpoint: Some("10.0.0.5:3000".to_string()),
            health: InstanceHealth {
                cpu_percent: Some(45.2),
                memory_bytes: Some(256 * 1024 * 1024),
                inflight_requests: 3,
                healthy: true,
            },
        };
        let json = serde_json::to_string(&info).unwrap();
        let parsed: InstanceInfo = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.id, "box-abc");
        assert_eq!(parsed.state, InstanceState::Busy);
        assert_eq!(parsed.health.cpu_percent, Some(45.2));
        assert_eq!(parsed.health.inflight_requests, 3);
    }

    #[test]
    fn test_instance_health_default() {
        let health = InstanceHealth::default();
        assert!(health.cpu_percent.is_none());
        assert!(health.memory_bytes.is_none());
        assert_eq!(health.inflight_requests, 0);
        assert!(health.healthy);
    }

    #[test]
    fn test_instance_event_transition() {
        let event = InstanceEvent::transition(
            "box-123",
            "my-svc",
            InstanceState::Booting,
            InstanceState::Ready,
        );
        assert_eq!(event.instance_id, "box-123");
        assert_eq!(event.service, "my-svc");
        assert_eq!(event.from_state, InstanceState::Booting);
        assert_eq!(event.to_state, InstanceState::Ready);
        assert!(event.message.is_empty());
    }

    #[test]
    fn test_instance_event_with_message() {
        let event = InstanceEvent::transition(
            "box-456",
            "svc",
            InstanceState::Booting,
            InstanceState::Failed,
        )
        .with_message("OOM killed");
        assert_eq!(event.message, "OOM killed");
        assert_eq!(event.to_state, InstanceState::Failed);
    }

    #[test]
    fn test_instance_event_serde() {
        let event = InstanceEvent::transition(
            "box-789",
            "api",
            InstanceState::Ready,
            InstanceState::Draining,
        );
        let json = serde_json::to_string(&event).unwrap();
        let parsed: InstanceEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.instance_id, "box-789");
        assert_eq!(parsed.from_state, InstanceState::Ready);
        assert_eq!(parsed.to_state, InstanceState::Draining);
    }

    #[test]
    fn test_instance_registration_serde() {
        let reg = InstanceRegistration {
            instance_id: "box-reg".to_string(),
            service: "web".to_string(),
            endpoint: "10.0.0.10:8080".to_string(),
            metadata: HashMap::from([("version".to_string(), "v1.2".to_string())]),
            started_at: Utc::now(),
        };
        let json = serde_json::to_string(&reg).unwrap();
        let parsed: InstanceRegistration = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.instance_id, "box-reg");
        assert_eq!(parsed.endpoint, "10.0.0.10:8080");
        assert_eq!(parsed.metadata.get("version").unwrap(), "v1.2");
    }

    #[test]
    fn test_instance_deregistration_serde() {
        let dereg = InstanceDeregistration {
            instance_id: "box-dereg".to_string(),
            service: "web".to_string(),
            reason: "scale-down".to_string(),
        };
        let json = serde_json::to_string(&dereg).unwrap();
        let parsed: InstanceDeregistration = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.instance_id, "box-dereg");
        assert_eq!(parsed.reason, "scale-down");
    }

    #[test]
    fn test_scale_config_default() {
        let config = ScaleConfig::default();
        assert!(config.image.is_none());
        assert!(config.vcpus.is_none());
        assert!(config.memory_mib.is_none());
        assert!(config.env.is_empty());
        assert!(config.port_map.is_empty());
    }

    #[test]
    fn test_instance_state_equality() {
        assert_eq!(InstanceState::Ready, InstanceState::Ready);
        assert_ne!(InstanceState::Ready, InstanceState::Busy);
    }

    #[test]
    fn test_instance_state_hash() {
        use std::collections::HashSet;
        let mut set = HashSet::new();
        set.insert(InstanceState::Ready);
        set.insert(InstanceState::Busy);
        set.insert(InstanceState::Ready); // duplicate
        assert_eq!(set.len(), 2);
    }
}
