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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{BoxRuntimeSpec, RuntimeClass, WorkloadKind};
    use std::sync::Arc;

    struct MockVmExecutor;

    #[async_trait]
    impl VmExecutor for MockVmExecutor {
        async fn execute_in_vm(
            &self,
            envelope: &BoxWorkloadEnvelope,
            timeout: Duration,
        ) -> VmResult {
            Ok(serde_json::json!({
                "runtime": envelope.runtime.runtime,
                "timeout_ms": timeout.as_millis(),
            }))
        }

        async fn pool_stats(&self) -> VmPoolStats {
            VmPoolStats {
                idle: 2,
                active: 1,
                max_total: 4,
                available_permits: 1,
            }
        }
    }

    fn envelope() -> BoxWorkloadEnvelope {
        BoxWorkloadEnvelope {
            runtime_class: RuntimeClass::A3sBox,
            workload_kind: WorkloadKind::ExecutionTask,
            runtime: BoxRuntimeSpec::for_execution_adapter("http", "get"),
            input: serde_json::json!({"url": "https://example.invalid"}),
            labels: Default::default(),
        }
    }

    #[test]
    fn vm_pool_stats_default_clone_and_debug_are_stable() {
        let stats = VmPoolStats::default();
        assert_eq!(stats.idle, 0);
        assert_eq!(stats.active, 0);
        assert_eq!(stats.max_total, 0);
        assert_eq!(stats.available_permits, 0);

        let cloned = stats.clone();
        assert!(format!("{cloned:?}").contains("available_permits"));
    }

    #[tokio::test]
    async fn vm_executor_trait_object_executes_and_reports_stats() {
        let executor: Arc<dyn VmExecutor> = Arc::new(MockVmExecutor);

        let result = executor
            .execute_in_vm(&envelope(), Duration::from_millis(250))
            .await
            .unwrap();
        assert_eq!(result["runtime"], "a3s/executor/http");
        assert_eq!(result["timeout_ms"], 250);

        let stats = executor.pool_stats().await;
        assert_eq!(stats.idle, 2);
        assert_eq!(stats.active, 1);
        assert_eq!(stats.max_total, 4);
        assert_eq!(stats.available_permits, 1);
    }
}
