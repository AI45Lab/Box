//! Scale Manager — Tracks instances per service and processes scale requests.
//!
//! Manages the mapping between services and their running instances,
//! handles scale-up/scale-down decisions, and emits instance state events.

use a3s_box_core::scale::{InstanceHealth, InstanceState};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

mod manager;
mod registry;

#[cfg(test)]
mod tests;

// Re-export public types
pub use manager::ScaleManager;
pub use registry::InstanceRegistry;

/// Instances belonging to a single service.
#[derive(Debug, Clone)]
pub(super) struct ServiceInstances {
    /// Target replica count
    pub(super) target_replicas: u32,
    /// Active instances
    pub(super) instances: Vec<TrackedInstance>,
}

/// A tracked instance with its current state.
#[derive(Debug, Clone)]
pub(super) struct TrackedInstance {
    pub(super) id: String,
    pub(super) state: InstanceState,
    pub(super) created_at: DateTime<Utc>,
    pub(super) ready_at: Option<DateTime<Utc>>,
    pub(super) endpoint: Option<String>,
    pub(super) health: InstanceHealth,
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
