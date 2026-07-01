//! VM executor trait for MicroVM workload execution
//!
//! This trait allows the SDK to delegate MicroVM execution to an external
//! implementation provided by the runtime (e.g., a3s-lambda).

use std::time::Duration;

pub use crate::BoxWorkloadEnvelope;

/// Result type for VM execution
pub type VmResult = std::result::Result<serde_json::Value, String>;

use async_trait::async_trait;

/// Trait for executing workloads inside MicroVMs.
///
/// This trait is implemented by the runtime (e.g., a3s-lambda) to provide
/// actual MicroVM execution capabilities. The SDK uses this trait to delegate
/// VM execution when in MicroVM mode.
#[async_trait]
pub trait VmExecutor: Send + Sync {
    /// Execute a workload inside a MicroVM.
    ///
    /// # Arguments
    /// * `envelope` - The workload envelope specifying what to execute
    /// * `timeout` - Maximum duration for the execution
    ///
    /// # Returns
    /// The execution result as JSON, or an error message.
    async fn execute_in_vm(&self, envelope: &BoxWorkloadEnvelope, timeout: Duration) -> VmResult;

    /// Get the current VM pool statistics.
    async fn pool_stats(&self) -> VmPoolStats;
}

/// VM pool statistics.
#[derive(Debug, Clone, Default)]
pub struct VmPoolStats {
    pub idle: usize,
    pub active: usize,
    pub max_total: usize,
    pub available_permits: usize,
}
