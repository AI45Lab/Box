//! Kubernetes BoxAutoscaler CRD types.
//!
//! Defines the Custom Resource Definition for autoscaling Box instances
//! in a Kubernetes cluster. The controller watches these resources and
//! adjusts replica counts based on metrics.
//!
//! ## CRD Schema
//!
//! ```yaml
//! apiVersion: box.a3s.dev/v1alpha1
//! kind: BoxAutoscaler
//! metadata:
//!   name: my-service-autoscaler
//! spec:
//!   targetRef:
//!     kind: BoxService
//!     name: my-service
//!   minReplicas: 1
//!   maxReplicas: 10
//!   metrics:
//!     - type: cpu
//!       target: 70
//!     - type: inflight
//!       target: 50
//!   behavior:
//!     scaleUp:
//!       stabilizationWindowSeconds: 60
//!       maxScalePerMinute: 3
//!     scaleDown:
//!       stabilizationWindowSeconds: 300
//!       maxScalePerMinute: 1
//! ```

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// API group for Box CRDs.
pub const API_GROUP: &str = "box.a3s.dev";
/// API version for BoxAutoscaler.
pub const API_VERSION: &str = "v1alpha1";
/// CRD kind.
pub const KIND: &str = "BoxAutoscaler";

/// BoxAutoscaler CRD spec — desired autoscaling behavior.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BoxAutoscalerSpec {
    /// Reference to the target resource to scale.
    pub target_ref: TargetRef,
    /// Minimum number of replicas (default: 1).
    #[serde(default = "default_min_replicas")]
    pub min_replicas: u32,
    /// Maximum number of replicas.
    pub max_replicas: u32,
    /// Metrics to evaluate for scaling decisions.
    #[serde(default)]
    pub metrics: Vec<MetricSpec>,
    /// Scaling behavior configuration.
    #[serde(default)]
    pub behavior: ScalingBehavior,
    /// Cooldown period in seconds after a scale event (default: 60).
    #[serde(default = "default_cooldown")]
    pub cooldown_secs: u64,
}

fn default_min_replicas() -> u32 {
    1
}

fn default_cooldown() -> u64 {
    60
}

/// Reference to the target resource being scaled.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TargetRef {
    /// Resource kind (e.g., "BoxService", "Deployment").
    pub kind: String,
    /// Resource name.
    pub name: String,
    /// Namespace (optional, defaults to "default").
    #[serde(default = "default_namespace")]
    pub namespace: String,
}

fn default_namespace() -> String {
    "default".to_string()
}

/// A metric used for autoscaling decisions.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MetricSpec {
    /// Metric type.
    #[serde(rename = "type")]
    pub metric_type: MetricType,
    /// Target value (interpretation depends on metric type).
    pub target: u32,
    /// Tolerance percentage before triggering scale (default: 10%).
    #[serde(default = "default_tolerance")]
    pub tolerance_percent: u32,
}

fn default_tolerance() -> u32 {
    10
}

/// Supported metric types for autoscaling.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MetricType {
    /// Average CPU utilization percentage across instances.
    Cpu,
    /// Average memory utilization percentage across instances.
    Memory,
    /// Total in-flight requests across instances.
    Inflight,
    /// Requests per second across all instances.
    Rps,
    /// Custom metric (from Prometheus or external source).
    Custom,
}

impl std::fmt::Display for MetricType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Cpu => write!(f, "cpu"),
            Self::Memory => write!(f, "memory"),
            Self::Inflight => write!(f, "inflight"),
            Self::Rps => write!(f, "rps"),
            Self::Custom => write!(f, "custom"),
        }
    }
}

/// Scaling behavior configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScalingBehavior {
    /// Scale-up behavior.
    #[serde(default)]
    pub scale_up: ScalingRules,
    /// Scale-down behavior.
    #[serde(default)]
    pub scale_down: ScalingRules,
}

impl Default for ScalingBehavior {
    fn default() -> Self {
        Self {
            scale_up: ScalingRules {
                stabilization_window_secs: 60,
                max_scale_per_minute: 3,
            },
            scale_down: ScalingRules {
                stabilization_window_secs: 300,
                max_scale_per_minute: 1,
            },
        }
    }
}

/// Rules for a scaling direction (up or down).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScalingRules {
    /// Seconds to wait after a metric change before acting.
    #[serde(default = "default_stabilization")]
    pub stabilization_window_secs: u64,
    /// Maximum replica changes per minute.
    #[serde(default = "default_max_scale")]
    pub max_scale_per_minute: u32,
}

fn default_stabilization() -> u64 {
    60
}

fn default_max_scale() -> u32 {
    2
}

impl Default for ScalingRules {
    fn default() -> Self {
        Self {
            stabilization_window_secs: 60,
            max_scale_per_minute: 2,
        }
    }
}

/// BoxAutoscaler CRD status — observed state.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BoxAutoscalerStatus {
    /// Current number of replicas.
    pub current_replicas: u32,
    /// Desired number of replicas (as computed by the controller).
    pub desired_replicas: u32,
    /// Last time the autoscaler scaled.
    #[serde(default)]
    pub last_scale_time: Option<DateTime<Utc>>,
    /// Current metric values.
    #[serde(default)]
    pub current_metrics: Vec<MetricValue>,
    /// Conditions describing the autoscaler state.
    #[serde(default)]
    pub conditions: Vec<AutoscalerCondition>,
}

/// An observed metric value.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MetricValue {
    /// Metric type.
    #[serde(rename = "type")]
    pub metric_type: MetricType,
    /// Current observed value.
    pub current: u32,
    /// Target value from spec.
    pub target: u32,
}

/// A condition on the autoscaler.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AutoscalerCondition {
    /// Condition type (e.g., "Ready", "ScalingActive", "ScalingLimited").
    #[serde(rename = "type")]
    pub condition_type: String,
    /// Status: "True", "False", or "Unknown".
    pub status: String,
    /// Last time the condition transitioned.
    #[serde(default)]
    pub last_transition_time: Option<DateTime<Utc>>,
    /// Human-readable reason.
    #[serde(default)]
    pub reason: String,
    /// Human-readable message.
    #[serde(default)]
    pub message: String,
}

/// Full BoxAutoscaler resource (spec + status + metadata).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BoxAutoscaler {
    /// API version.
    pub api_version: String,
    /// Resource kind.
    pub kind: String,
    /// Resource name.
    pub name: String,
    /// Namespace.
    #[serde(default = "default_namespace")]
    pub namespace: String,
    /// Labels.
    #[serde(default)]
    pub labels: HashMap<String, String>,
    /// Spec.
    pub spec: BoxAutoscalerSpec,
    /// Status.
    #[serde(default)]
    pub status: BoxAutoscalerStatus,
}

impl BoxAutoscaler {
    /// Create a new BoxAutoscaler resource.
    pub fn new(name: &str, spec: BoxAutoscalerSpec) -> Self {
        Self {
            api_version: format!("{}/{}", API_GROUP, API_VERSION),
            kind: KIND.to_string(),
            name: name.to_string(),
            namespace: "default".to_string(),
            labels: HashMap::new(),
            spec,
            status: BoxAutoscalerStatus::default(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_spec() -> BoxAutoscalerSpec {
        BoxAutoscalerSpec {
            target_ref: TargetRef {
                kind: "BoxService".to_string(),
                name: "my-service".to_string(),
                namespace: "default".to_string(),
            },
            min_replicas: 1,
            max_replicas: 10,
            metrics: vec![MetricSpec {
                metric_type: MetricType::Cpu,
                target: 70,
                tolerance_percent: 10,
            }],
            behavior: ScalingBehavior::default(),
            cooldown_secs: 60,
        }
    }

    #[test]
    fn test_box_autoscaler_new() {
        let ba = BoxAutoscaler::new("test-scaler", sample_spec());
        assert_eq!(ba.name, "test-scaler");
        assert_eq!(ba.api_version, "box.a3s.dev/v1alpha1");
        assert_eq!(ba.kind, "BoxAutoscaler");
        assert_eq!(ba.namespace, "default");
        assert_eq!(ba.spec.min_replicas, 1);
        assert_eq!(ba.spec.max_replicas, 10);
    }

    #[test]
    fn test_spec_serde_roundtrip() {
        let spec = sample_spec();
        let json = serde_json::to_string(&spec).unwrap();
        let parsed: BoxAutoscalerSpec = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.min_replicas, 1);
        assert_eq!(parsed.max_replicas, 10);
        assert_eq!(parsed.metrics.len(), 1);
        assert_eq!(parsed.metrics[0].metric_type, MetricType::Cpu);
        assert_eq!(parsed.metrics[0].target, 70);
    }

    #[test]
    fn test_spec_deserialize_minimal() {
        let json = r#"{
            "target_ref": {"kind": "BoxService", "name": "svc"},
            "max_replicas": 5
        }"#;
        let spec: BoxAutoscalerSpec = serde_json::from_str(json).unwrap();
        assert_eq!(spec.min_replicas, 1); // default
        assert_eq!(spec.max_replicas, 5);
        assert_eq!(spec.cooldown_secs, 60); // default
        assert!(spec.metrics.is_empty());
        assert_eq!(spec.target_ref.namespace, "default"); // default
    }

    #[test]
    fn test_metric_type_display() {
        assert_eq!(MetricType::Cpu.to_string(), "cpu");
        assert_eq!(MetricType::Memory.to_string(), "memory");
        assert_eq!(MetricType::Inflight.to_string(), "inflight");
        assert_eq!(MetricType::Rps.to_string(), "rps");
        assert_eq!(MetricType::Custom.to_string(), "custom");
    }

    #[test]
    fn test_metric_type_serde() {
        let json = r#""cpu""#;
        let mt: MetricType = serde_json::from_str(json).unwrap();
        assert_eq!(mt, MetricType::Cpu);

        let json = serde_json::to_string(&MetricType::Inflight).unwrap();
        assert_eq!(json, r#""inflight""#);
    }

    #[test]
    fn test_scaling_behavior_default() {
        let behavior = ScalingBehavior::default();
        assert_eq!(behavior.scale_up.stabilization_window_secs, 60);
        assert_eq!(behavior.scale_up.max_scale_per_minute, 3);
        assert_eq!(behavior.scale_down.stabilization_window_secs, 300);
        assert_eq!(behavior.scale_down.max_scale_per_minute, 1);
    }

    #[test]
    fn test_status_default() {
        let status = BoxAutoscalerStatus::default();
        assert_eq!(status.current_replicas, 0);
        assert_eq!(status.desired_replicas, 0);
        assert!(status.last_scale_time.is_none());
        assert!(status.current_metrics.is_empty());
        assert!(status.conditions.is_empty());
    }

    #[test]
    fn test_status_serde_roundtrip() {
        let status = BoxAutoscalerStatus {
            current_replicas: 3,
            desired_replicas: 5,
            last_scale_time: Some(Utc::now()),
            current_metrics: vec![MetricValue {
                metric_type: MetricType::Cpu,
                current: 85,
                target: 70,
            }],
            conditions: vec![AutoscalerCondition {
                condition_type: "ScalingActive".to_string(),
                status: "True".to_string(),
                last_transition_time: Some(Utc::now()),
                reason: "HighCPU".to_string(),
                message: "CPU above target".to_string(),
            }],
        };
        let json = serde_json::to_string(&status).unwrap();
        let parsed: BoxAutoscalerStatus = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.current_replicas, 3);
        assert_eq!(parsed.desired_replicas, 5);
        assert_eq!(parsed.current_metrics.len(), 1);
        assert_eq!(parsed.conditions.len(), 1);
        assert_eq!(parsed.conditions[0].reason, "HighCPU");
    }

    #[test]
    fn test_full_resource_serde() {
        let ba = BoxAutoscaler::new("my-scaler", sample_spec());
        let json = serde_json::to_string_pretty(&ba).unwrap();
        let parsed: BoxAutoscaler = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.name, "my-scaler");
        assert_eq!(parsed.spec.max_replicas, 10);
        assert_eq!(parsed.status.current_replicas, 0);
    }

    #[test]
    fn test_target_ref_serde() {
        let tr = TargetRef {
            kind: "BoxService".to_string(),
            name: "web".to_string(),
            namespace: "production".to_string(),
        };
        let json = serde_json::to_string(&tr).unwrap();
        let parsed: TargetRef = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.kind, "BoxService");
        assert_eq!(parsed.name, "web");
        assert_eq!(parsed.namespace, "production");
    }

    #[test]
    fn test_metric_spec_with_tolerance() {
        let ms = MetricSpec {
            metric_type: MetricType::Rps,
            target: 1000,
            tolerance_percent: 15,
        };
        let json = serde_json::to_string(&ms).unwrap();
        let parsed: MetricSpec = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.metric_type, MetricType::Rps);
        assert_eq!(parsed.target, 1000);
        assert_eq!(parsed.tolerance_percent, 15);
    }

    #[test]
    fn test_metric_spec_default_tolerance() {
        let json = r#"{"type": "cpu", "target": 80}"#;
        let ms: MetricSpec = serde_json::from_str(json).unwrap();
        assert_eq!(ms.tolerance_percent, 10); // default
    }

    #[test]
    fn test_constants() {
        assert_eq!(API_GROUP, "box.a3s.dev");
        assert_eq!(API_VERSION, "v1alpha1");
        assert_eq!(KIND, "BoxAutoscaler");
    }
}
