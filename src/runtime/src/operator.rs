//! BoxAutoscaler controller — reconciliation logic for the K8s operator.
//!
//! Evaluates metrics against the BoxAutoscaler spec and computes the
//! desired replica count. This is the core decision engine that can be
//! used standalone or inside a kube-rs controller.

use std::time::Instant;

use a3s_box_core::operator::{
    AutoscalerCondition, BoxAutoscalerSpec, BoxAutoscalerStatus, MetricType, MetricValue,
};
use chrono::Utc;
use serde::{Deserialize, Serialize};

/// Input metrics for the reconciler.
#[derive(Debug, Clone, Default)]
pub struct ObservedMetrics {
    /// Average CPU utilization (0-100).
    pub avg_cpu_percent: Option<f32>,
    /// Average memory utilization (0-100).
    pub avg_memory_percent: Option<f32>,
    /// Total in-flight requests.
    pub total_inflight: Option<u32>,
    /// Requests per second.
    pub rps: Option<u32>,
    /// Custom metric values (name → value).
    pub custom: std::collections::HashMap<String, u32>,
}

/// Result of a reconciliation cycle.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReconcileResult {
    /// Computed desired replicas.
    pub desired_replicas: u32,
    /// Whether a scale action is needed.
    pub scale_needed: bool,
    /// Direction: "up", "down", or "none".
    pub direction: String,
    /// Reason for the decision.
    pub reason: String,
    /// Updated metric values for status.
    pub metric_values: Vec<MetricValue>,
}

/// Autoscaler controller that computes desired replicas from metrics.
pub struct AutoscalerController {
    /// Last time a scale-up was performed.
    last_scale_up: Option<Instant>,
    /// Last time a scale-down was performed.
    last_scale_down: Option<Instant>,
}

impl Default for AutoscalerController {
    fn default() -> Self {
        Self::new()
    }
}

impl AutoscalerController {
    /// Create a new controller.
    pub fn new() -> Self {
        Self {
            last_scale_up: None,
            last_scale_down: None,
        }
    }

    /// Reconcile the desired state against observed metrics.
    ///
    /// Returns the computed desired replica count and whether scaling is needed.
    pub fn reconcile(
        &mut self,
        spec: &BoxAutoscalerSpec,
        current_replicas: u32,
        metrics: &ObservedMetrics,
    ) -> ReconcileResult {
        if spec.metrics.is_empty() {
            return ReconcileResult {
                desired_replicas: current_replicas,
                scale_needed: false,
                direction: "none".to_string(),
                reason: "No metrics configured".to_string(),
                metric_values: Vec::new(),
            };
        }

        let mut computed_desired: Option<u32> = None;
        let mut metric_values = Vec::new();
        let mut reasons = Vec::new();

        for metric_spec in &spec.metrics {
            let current_value = match metric_spec.metric_type {
                MetricType::Cpu => metrics.avg_cpu_percent.map(|v| v as u32),
                MetricType::Memory => metrics.avg_memory_percent.map(|v| v as u32),
                MetricType::Inflight => metrics.total_inflight,
                MetricType::Rps => metrics.rps,
                MetricType::Custom => None, // Custom metrics not evaluated here
            };

            let current = match current_value {
                Some(v) => v,
                None => continue,
            };

            metric_values.push(MetricValue {
                metric_type: metric_spec.metric_type,
                current,
                target: metric_spec.target,
            });

            // Compute desired replicas for this metric
            let desired = compute_desired_replicas(
                current_replicas,
                current,
                metric_spec.target,
                metric_spec.tolerance_percent,
                spec.min_replicas,
                spec.max_replicas,
            );

            // Take the maximum across all metrics (most aggressive)
            computed_desired =
                Some(computed_desired.map_or(desired, |prev: u32| prev.max(desired)));

            if desired > current_replicas {
                reasons.push(format!(
                    "{} at {}% (target {}%)",
                    metric_spec.metric_type, current, metric_spec.target
                ));
            }
        }

        // No metric data available — hold at current
        let max_desired = match computed_desired {
            Some(d) => d,
            None => {
                return ReconcileResult {
                    desired_replicas: current_replicas,
                    scale_needed: false,
                    direction: "none".to_string(),
                    reason: "No metric data available".to_string(),
                    metric_values,
                };
            }
        };

        let desired = max_desired.clamp(spec.min_replicas, spec.max_replicas);

        let (scale_needed, direction, reason) = if desired > current_replicas {
            // Check scale-up stabilization
            if let Some(last) = self.last_scale_up {
                if last.elapsed().as_secs() < spec.behavior.scale_up.stabilization_window_secs {
                    return ReconcileResult {
                        desired_replicas: current_replicas,
                        scale_needed: false,
                        direction: "none".to_string(),
                        reason: "Scale-up stabilization window active".to_string(),
                        metric_values,
                    };
                }
            }
            self.last_scale_up = Some(Instant::now());
            (
                true,
                "up".to_string(),
                if reasons.is_empty() {
                    "Metrics above target".to_string()
                } else {
                    reasons.join("; ")
                },
            )
        } else if desired < current_replicas {
            // Check scale-down stabilization
            if let Some(last) = self.last_scale_down {
                if last.elapsed().as_secs() < spec.behavior.scale_down.stabilization_window_secs {
                    return ReconcileResult {
                        desired_replicas: current_replicas,
                        scale_needed: false,
                        direction: "none".to_string(),
                        reason: "Scale-down stabilization window active".to_string(),
                        metric_values,
                    };
                }
            }
            self.last_scale_down = Some(Instant::now());
            (true, "down".to_string(), "Metrics below target".to_string())
        } else {
            (false, "none".to_string(), "At target".to_string())
        };

        ReconcileResult {
            desired_replicas: desired,
            scale_needed,
            direction,
            reason,
            metric_values,
        }
    }

    /// Build an updated status from a reconcile result.
    pub fn build_status(
        &self,
        current_replicas: u32,
        result: &ReconcileResult,
    ) -> BoxAutoscalerStatus {
        let mut conditions = Vec::new();

        conditions.push(AutoscalerCondition {
            condition_type: "Ready".to_string(),
            status: "True".to_string(),
            last_transition_time: Some(Utc::now()),
            reason: "Reconciled".to_string(),
            message: result.reason.clone(),
        });

        if result.scale_needed {
            conditions.push(AutoscalerCondition {
                condition_type: "ScalingActive".to_string(),
                status: "True".to_string(),
                last_transition_time: Some(Utc::now()),
                reason: format!(
                    "Scale{}",
                    if result.direction == "up" {
                        "Up"
                    } else {
                        "Down"
                    }
                ),
                message: format!(
                    "{} → {} replicas",
                    current_replicas, result.desired_replicas
                ),
            });
        }

        BoxAutoscalerStatus {
            current_replicas,
            desired_replicas: result.desired_replicas,
            last_scale_time: if result.scale_needed {
                Some(Utc::now())
            } else {
                None
            },
            current_metrics: result.metric_values.clone(),
            conditions,
        }
    }
}

/// Compute desired replicas for a single metric using the ratio algorithm.
///
/// `desired = ceil(current_replicas * (current_value / target_value))`
///
/// Respects tolerance: no change if within ±tolerance% of target.
fn compute_desired_replicas(
    current_replicas: u32,
    current_value: u32,
    target_value: u32,
    tolerance_percent: u32,
    min_replicas: u32,
    max_replicas: u32,
) -> u32 {
    if target_value == 0 || current_replicas == 0 {
        return min_replicas;
    }

    let ratio = current_value as f64 / target_value as f64;
    let tolerance = tolerance_percent as f64 / 100.0;

    // Within tolerance band — no change
    if (ratio - 1.0).abs() <= tolerance {
        return current_replicas;
    }

    let desired = (current_replicas as f64 * ratio).ceil() as u32;
    desired.clamp(min_replicas, max_replicas)
}

#[cfg(test)]
mod tests {
    use super::*;
    use a3s_box_core::operator::{
        BoxAutoscalerSpec, MetricSpec, MetricType, ScalingBehavior, ScalingRules, TargetRef,
    };

    fn test_spec(max: u32) -> BoxAutoscalerSpec {
        BoxAutoscalerSpec {
            target_ref: TargetRef {
                kind: "BoxService".to_string(),
                name: "test".to_string(),
                namespace: "default".to_string(),
            },
            min_replicas: 1,
            max_replicas: max,
            metrics: vec![MetricSpec {
                metric_type: MetricType::Cpu,
                target: 70,
                tolerance_percent: 10,
            }],
            behavior: ScalingBehavior {
                scale_up: ScalingRules {
                    stabilization_window_secs: 0,
                    max_scale_per_minute: 10,
                },
                scale_down: ScalingRules {
                    stabilization_window_secs: 0,
                    max_scale_per_minute: 10,
                },
            },
            cooldown_secs: 0,
        }
    }

    #[test]
    fn test_compute_desired_replicas_above_target() {
        // 3 replicas, CPU at 90%, target 70% → ratio 1.28 → ceil(3 * 1.28) = 4
        let desired = compute_desired_replicas(3, 90, 70, 10, 1, 10);
        assert_eq!(desired, 4);
    }

    #[test]
    fn test_compute_desired_replicas_below_target() {
        // 5 replicas, CPU at 30%, target 70% → ratio 0.43 → ceil(5 * 0.43) = 3
        let desired = compute_desired_replicas(5, 30, 70, 10, 1, 10);
        assert_eq!(desired, 3);
    }

    #[test]
    fn test_compute_desired_replicas_within_tolerance() {
        // 3 replicas, CPU at 75%, target 70%, tolerance 10% → ratio 1.07 → within ±10%
        let desired = compute_desired_replicas(3, 75, 70, 10, 1, 10);
        assert_eq!(desired, 3); // No change
    }

    #[test]
    fn test_compute_desired_replicas_clamped_max() {
        // Would want 15 but max is 10
        let desired = compute_desired_replicas(5, 200, 70, 10, 1, 10);
        assert_eq!(desired, 10);
    }

    #[test]
    fn test_compute_desired_replicas_clamped_min() {
        // Would want 0 but min is 1
        let desired = compute_desired_replicas(5, 1, 70, 10, 1, 10);
        assert_eq!(desired, 1);
    }

    #[test]
    fn test_compute_desired_replicas_zero_target() {
        let desired = compute_desired_replicas(3, 50, 0, 10, 1, 10);
        assert_eq!(desired, 1); // min_replicas
    }

    #[test]
    fn test_reconcile_scale_up() {
        let mut ctrl = AutoscalerController::new();
        let spec = test_spec(10);
        let metrics = ObservedMetrics {
            avg_cpu_percent: Some(95.0),
            ..Default::default()
        };

        let result = ctrl.reconcile(&spec, 3, &metrics);
        assert!(result.scale_needed);
        assert_eq!(result.direction, "up");
        assert!(result.desired_replicas > 3);
        assert!(!result.metric_values.is_empty());
    }

    #[test]
    fn test_reconcile_scale_down() {
        let mut ctrl = AutoscalerController::new();
        let spec = test_spec(10);
        let metrics = ObservedMetrics {
            avg_cpu_percent: Some(20.0),
            ..Default::default()
        };

        let result = ctrl.reconcile(&spec, 5, &metrics);
        assert!(result.scale_needed);
        assert_eq!(result.direction, "down");
        assert!(result.desired_replicas < 5);
    }

    #[test]
    fn test_reconcile_no_change() {
        let mut ctrl = AutoscalerController::new();
        let spec = test_spec(10);
        let metrics = ObservedMetrics {
            avg_cpu_percent: Some(72.0), // Within 10% tolerance of 70
            ..Default::default()
        };

        let result = ctrl.reconcile(&spec, 3, &metrics);
        assert!(!result.scale_needed);
        assert_eq!(result.direction, "none");
        assert_eq!(result.desired_replicas, 3);
    }

    #[test]
    fn test_reconcile_no_metrics_configured() {
        let mut ctrl = AutoscalerController::new();
        let mut spec = test_spec(10);
        spec.metrics.clear();

        let metrics = ObservedMetrics::default();
        let result = ctrl.reconcile(&spec, 3, &metrics);
        assert!(!result.scale_needed);
        assert_eq!(result.desired_replicas, 3);
    }

    #[test]
    fn test_reconcile_no_metric_data() {
        let mut ctrl = AutoscalerController::new();
        let spec = test_spec(10);
        let metrics = ObservedMetrics::default(); // No CPU data

        let result = ctrl.reconcile(&spec, 3, &metrics);
        assert!(!result.scale_needed);
    }

    #[test]
    fn test_reconcile_stabilization_window() {
        let mut ctrl = AutoscalerController::new();
        let mut spec = test_spec(10);
        spec.behavior.scale_up.stabilization_window_secs = 3600; // 1 hour

        let metrics = ObservedMetrics {
            avg_cpu_percent: Some(95.0),
            ..Default::default()
        };

        // First reconcile: scale up
        let r1 = ctrl.reconcile(&spec, 3, &metrics);
        assert!(r1.scale_needed);

        // Second reconcile immediately: blocked by stabilization
        let r2 = ctrl.reconcile(&spec, 3, &metrics);
        assert!(!r2.scale_needed);
        assert!(r2.reason.contains("stabilization"));
    }

    #[test]
    fn test_reconcile_multiple_metrics() {
        let mut ctrl = AutoscalerController::new();
        let mut spec = test_spec(10);
        spec.metrics = vec![
            MetricSpec {
                metric_type: MetricType::Cpu,
                target: 70,
                tolerance_percent: 10,
            },
            MetricSpec {
                metric_type: MetricType::Inflight,
                target: 50,
                tolerance_percent: 10,
            },
        ];

        let metrics = ObservedMetrics {
            avg_cpu_percent: Some(65.0), // Within tolerance
            total_inflight: Some(200),   // Way above target
            ..Default::default()
        };

        let result = ctrl.reconcile(&spec, 2, &metrics);
        assert!(result.scale_needed);
        assert_eq!(result.direction, "up");
        // Should scale based on the highest demand metric (inflight)
        assert!(result.desired_replicas > 2);
    }

    #[test]
    fn test_reconcile_respects_max_replicas() {
        let mut ctrl = AutoscalerController::new();
        let spec = test_spec(5); // max 5

        let metrics = ObservedMetrics {
            avg_cpu_percent: Some(200.0), // Extreme load
            ..Default::default()
        };

        let result = ctrl.reconcile(&spec, 4, &metrics);
        assert!(result.desired_replicas <= 5);
    }

    #[test]
    fn test_reconcile_respects_min_replicas() {
        let mut ctrl = AutoscalerController::new();
        let mut spec = test_spec(10);
        spec.min_replicas = 2;

        let metrics = ObservedMetrics {
            avg_cpu_percent: Some(1.0), // Almost idle
            ..Default::default()
        };

        let result = ctrl.reconcile(&spec, 5, &metrics);
        assert!(result.desired_replicas >= 2);
    }

    #[test]
    fn test_build_status_no_scale() {
        let ctrl = AutoscalerController::new();
        let result = ReconcileResult {
            desired_replicas: 3,
            scale_needed: false,
            direction: "none".to_string(),
            reason: "At target".to_string(),
            metric_values: vec![],
        };

        let status = ctrl.build_status(3, &result);
        assert_eq!(status.current_replicas, 3);
        assert_eq!(status.desired_replicas, 3);
        assert!(status.last_scale_time.is_none());
        assert_eq!(status.conditions.len(), 1);
        assert_eq!(status.conditions[0].condition_type, "Ready");
    }

    #[test]
    fn test_build_status_with_scale() {
        let ctrl = AutoscalerController::new();
        let result = ReconcileResult {
            desired_replicas: 5,
            scale_needed: true,
            direction: "up".to_string(),
            reason: "cpu at 90% (target 70%)".to_string(),
            metric_values: vec![MetricValue {
                metric_type: MetricType::Cpu,
                current: 90,
                target: 70,
            }],
        };

        let status = ctrl.build_status(3, &result);
        assert_eq!(status.current_replicas, 3);
        assert_eq!(status.desired_replicas, 5);
        assert!(status.last_scale_time.is_some());
        assert_eq!(status.conditions.len(), 2); // Ready + ScalingActive
        assert_eq!(status.conditions[1].condition_type, "ScalingActive");
    }

    #[test]
    fn test_controller_new() {
        let ctrl = AutoscalerController::new();
        assert!(ctrl.last_scale_up.is_none());
        assert!(ctrl.last_scale_down.is_none());
    }
}
