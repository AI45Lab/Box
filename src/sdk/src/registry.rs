//! Execution registry - main entry point for MicroVM workload execution
//!
//! The ExecutionRegistry manages adapters and executes workloads inside
//! A3S Box MicroVMs.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

pub use crate::{
    BoxWorkloadEnvelope, CapabilityRisk, ExecutionCapability, ExecutionCapabilityGrant,
    ExecutionLaunchMode, RuntimeClass, SdkError, WorkloadKind,
};

pub use crate::vm::{VmExecutor, VmPoolStats};

/// Result type for SDK operations
pub type Result<T> = std::result::Result<T, SdkError>;

/// Capability match mode
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CapabilityMatchMode {
    None,
    AllRequired,
}

/// Execution policy for capability validation
#[derive(Debug, Clone)]
pub struct ExecutionPolicy {
    pub capability_match_mode: CapabilityMatchMode,
    pub max_risk: Option<CapabilityRisk>,
    pub allow_risk_escalation: bool,
    pub allowed_scopes: Vec<String>,
    pub allow_scope_escalation: bool,
}

impl Default for ExecutionPolicy {
    fn default() -> Self {
        Self {
            capability_match_mode: CapabilityMatchMode::AllRequired,
            max_risk: None,
            allow_risk_escalation: false,
            allowed_scopes: Vec::new(),
            allow_scope_escalation: false,
        }
    }
}

/// Execution registry - manages workload execution across adapters and VMs.
pub struct ExecutionRegistry {
    adapters: HashMap<String, Arc<dyn crate::ExecutionAdapter>>,
    launch_mode: ExecutionLaunchMode,
    vm_executor: Option<Arc<dyn VmExecutor>>,
}

impl ExecutionRegistry {
    /// Create a new ExecutionRegistry with default settings.
    pub fn new() -> Self {
        Self::with_defaults()
    }

    /// Create a new ExecutionRegistry with default adapter.
    pub fn with_defaults() -> Self {
        let mut registry = Self {
            adapters: HashMap::new(),
            launch_mode: ExecutionLaunchMode::HostAdapterCompat,
            vm_executor: None,
        };

        // Register default HTTP adapter
        registry.register_adapter("http", Arc::new(crate::HttpExecutionAdapter));

        registry
    }

    /// Create a registry with specific enabled adapters.
    ///
    /// # Errors
    /// Returns an error if an unknown adapter name is specified.
    pub fn from_enabled_with_launch_mode(
        adapters: impl IntoIterator<Item = impl AsRef<str>>,
        launch_mode: ExecutionLaunchMode,
    ) -> std::result::Result<Self, String> {
        let mut registry = Self {
            adapters: HashMap::new(),
            launch_mode,
            vm_executor: None,
        };

        for adapter_name in adapters {
            let name = adapter_name.as_ref();
            match name {
                "http" => {
                    registry.register_adapter(name, Arc::new(crate::HttpExecutionAdapter));
                }
                "vm" | "microvm" => {
                    // VM adapter would be registered here when integrated
                    tracing::debug!("VM adapter requested but not yet implemented");
                }
                other => {
                    return Err(format!("unknown execution adapter: {other}"));
                }
            }
        }

        Ok(registry)
    }

    /// Set the VM executor for MicroVM execution.
    ///
    /// This allows the registry to execute workloads inside MicroVMs
    /// when the launch mode is MicroVM or Hybrid.
    pub fn with_vm_executor(mut self, executor: Arc<dyn VmExecutor>) -> Self {
        self.vm_executor = Some(executor);
        self
    }

    /// Check if VM execution is enabled.
    pub fn is_vm_enabled(&self) -> bool {
        self.vm_executor.is_some()
    }

    /// Register an execution adapter.
    pub fn register_adapter(&mut self, name: &str, adapter: Arc<dyn crate::ExecutionAdapter>) {
        self.adapters.insert(name.to_string(), adapter);
    }

    /// Get an adapter by name.
    pub fn get_adapter(&self, name: &str) -> Option<&Arc<dyn crate::ExecutionAdapter>> {
        self.adapters.get(name)
    }

    /// Get all registered adapter names.
    pub fn adapter_names(&self) -> Vec<&str> {
        self.adapters.keys().map(|s| s.as_str()).collect()
    }

    /// Get all capability grants from all adapters.
    pub fn all_capabilities(&self) -> Vec<ExecutionCapabilityGrant> {
        self.adapters
            .values()
            .flat_map(|a| a.capabilities())
            .collect()
    }

    /// Execute a workload envelope.
    ///
    /// This is the main entry point for workload execution. It validates
    /// the envelope, selects an appropriate adapter or VM executor, and executes the workload.
    pub async fn execute_box_workload(
        &self,
        envelope: &BoxWorkloadEnvelope,
        timeout: Duration,
    ) -> std::result::Result<serde_json::Value, String> {
        // Validate envelope
        envelope
            .validate()
            .map_err(|e| format!("invalid workload envelope: {e}"))?;

        // For MicroVM mode with a VM executor, delegate to VM runtime
        if self.vm_executor.is_some() && matches!(envelope.runtime_class, RuntimeClass::A3sBox) {
            return self.execute_in_vm(envelope, timeout).await;
        }

        // For HostAdapterCompat mode or no VM executor, use registered adapters
        self.execute_via_adapter(envelope, timeout).await
    }

    /// Execute workload via adapters in host-compatible mode.
    async fn execute_via_adapter(
        &self,
        envelope: &BoxWorkloadEnvelope,
        timeout: Duration,
    ) -> std::result::Result<serde_json::Value, String> {
        // Extract executor name from runtime spec
        // Format: "a3s/executor/{executor}" or "a3s/agent-runner"
        let runtime = &envelope.runtime.runtime;
        let executor = if runtime.starts_with("a3s/executor/") {
            runtime.trim_start_matches("a3s/executor/")
        } else if runtime.starts_with("a3s/agent-runner") {
            "agent"
        } else {
            return Err(format!("unknown runtime: {runtime}"));
        };

        // Get adapter for this executor
        let adapter = self
            .adapters
            .get(executor)
            .ok_or_else(|| format!("no adapter registered for executor: {executor}"))?;

        // Extract handler from args
        let handler = extract_handler_from_args(&envelope.runtime.args)
            .unwrap_or_else(|| executor.to_string());

        // Execute via adapter
        adapter
            .execute(&handler, &envelope.input, timeout)
            .await
            .map_err(|e| e.to_string())
    }

    /// Execute workload inside a MicroVM.
    async fn execute_in_vm(
        &self,
        envelope: &BoxWorkloadEnvelope,
        timeout: Duration,
    ) -> std::result::Result<serde_json::Value, String> {
        match &self.vm_executor {
            Some(executor) => executor.execute_in_vm(envelope, timeout).await,
            None => Err("MicroVM executor not configured - cannot execute in VM mode".to_string()),
        }
    }

    /// Validate that required capabilities are satisfied.
    pub fn validate_capabilities(
        &self,
        executor: &str,
        required_capabilities: &[ExecutionCapability],
        policy: &ExecutionPolicy,
    ) -> std::result::Result<(), String> {
        let adapter = self
            .adapters
            .get(executor)
            .ok_or_else(|| format!("no adapter registered for executor: {executor}"))?;

        let grants = adapter.capabilities();

        // Check each required capability against grants
        for required in required_capabilities {
            let matching_grant = grants
                .iter()
                .find(|g| capability_matches(&g.capability, required));

            match (matching_grant, policy.allow_risk_escalation) {
                (Some(grant), _) => {
                    // Check risk level
                    if let Some(max_risk) = &policy.max_risk {
                        if grant.risk > *max_risk && !policy.allow_risk_escalation {
                            return Err(format!(
                                "capability {:#?} exceeds max risk {:?}",
                                required, max_risk
                            ));
                        }
                    }
                }
                (None, _) if !policy.allow_scope_escalation => {
                    return Err(format!("required capability not granted: {:#?}", required));
                }
                _ => {}
            }
        }

        Ok(())
    }

    /// Take a snapshot of the VM runtime pool status.
    pub async fn box_runtime_pool_snapshot(&self) -> BoxRuntimePoolSnapshot {
        match &self.vm_executor {
            Some(executor) => {
                let stats = executor.pool_stats().await;
                BoxRuntimePoolSnapshot {
                    launch_mode: self.launch_mode,
                    image_pool_count: 0,
                    idle_vms: stats.idle as u64,
                    active_vms: stats.active as u64,
                    total_vms: stats.max_total as u64,
                    max_total_vms: stats.max_total as u64,
                    available_vms: stats.available_permits as u64,
                    occupancy_ratio: if stats.max_total > 0 {
                        stats.active as f64 / stats.max_total as f64
                    } else {
                        0.0
                    },
                    active_ratio: if stats.max_total > 0 {
                        stats.active as f64 / stats.max_total as f64
                    } else {
                        0.0
                    },
                    has_capacity_pressure: stats.available_permits == 0
                        && stats.active >= stats.max_total,
                }
            }
            None => BoxRuntimePoolSnapshot {
                launch_mode: self.launch_mode,
                image_pool_count: 0,
                idle_vms: 0,
                active_vms: 0,
                total_vms: 0,
                max_total_vms: 0,
                available_vms: 0,
                occupancy_ratio: 0.0,
                active_ratio: 0.0,
                has_capacity_pressure: false,
            },
        }
    }
}

impl Default for ExecutionRegistry {
    fn default() -> Self {
        Self::new()
    }
}

/// Box runtime pool snapshot for observability.
#[derive(Debug, Clone)]
pub struct BoxRuntimePoolSnapshot {
    pub launch_mode: ExecutionLaunchMode,
    pub image_pool_count: u64,
    pub idle_vms: u64,
    pub active_vms: u64,
    pub total_vms: u64,
    pub max_total_vms: u64,
    pub available_vms: u64,
    pub occupancy_ratio: f64,
    pub active_ratio: f64,
    pub has_capacity_pressure: bool,
}

/// Extract handler from runtime args.
///
/// Looks for "--handler <value>" pattern in args.
fn extract_handler_from_args(args: &[String]) -> Option<String> {
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        if arg == "--handler" {
            if let Some(handler) = iter.next() {
                return Some(handler.clone());
            }
        }
    }
    None
}

/// Check if a granted capability matches a required capability.
fn capability_matches(granted: &ExecutionCapability, required: &ExecutionCapability) -> bool {
    match (granted, required) {
        (
            ExecutionCapability::Network { protocol: gp, .. },
            ExecutionCapability::Network { protocol: rp, .. },
        ) => gp == rp,
        (
            ExecutionCapability::Filesystem { scope: gs, .. },
            ExecutionCapability::Filesystem { scope: rs, .. },
        ) => gs == rs || gs == "*",
        (
            ExecutionCapability::Tool { name: gn, .. },
            ExecutionCapability::Tool { name: rn, .. },
        ) => gn == rn,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapter::{CapabilityAccess, ExecutionCapabilityGrant};
    use crate::vm::VmResult;
    use crate::BoxRuntimeSpec;
    use crate::ExecutionAdapter;
    use async_trait::async_trait;
    use std::sync::Arc;

    // Mock adapter for testing
    struct MockAdapter {
        capabilities_return: Vec<ExecutionCapabilityGrant>,
        execute_value: serde_json::Value,
        execute_error: Option<String>,
    }

    impl MockAdapter {
        fn new(
            capabilities: Vec<ExecutionCapabilityGrant>,
            result: std::result::Result<serde_json::Value, SdkError>,
        ) -> Self {
            match result {
                Ok(v) => Self {
                    capabilities_return: capabilities,
                    execute_value: v,
                    execute_error: None,
                },
                Err(e) => Self {
                    capabilities_return: capabilities,
                    execute_value: serde_json::json!({}),
                    execute_error: Some(e.to_string()),
                },
            }
        }
    }

    #[async_trait]
    impl ExecutionAdapter for MockAdapter {
        fn capabilities(&self) -> Vec<ExecutionCapabilityGrant> {
            self.capabilities_return.clone()
        }

        async fn execute(
            &self,
            _handler: &str,
            _input: &serde_json::Value,
            _timeout: Duration,
        ) -> std::result::Result<serde_json::Value, SdkError> {
            match &self.execute_error {
                Some(msg) => Err(SdkError::ExecutionFailed(msg.clone())),
                None => Ok(self.execute_value.clone()),
            }
        }
    }

    // Mock VM executor for testing
    struct MockVmExecutor {
        execute_result: VmResult,
        pool_stats: VmPoolStats,
    }

    impl MockVmExecutor {
        fn new(execute_result: VmResult, pool_stats: VmPoolStats) -> Self {
            Self {
                execute_result,
                pool_stats,
            }
        }
    }

    #[async_trait]
    impl VmExecutor for MockVmExecutor {
        async fn execute_in_vm(
            &self,
            _envelope: &BoxWorkloadEnvelope,
            _timeout: Duration,
        ) -> VmResult {
            self.execute_result.clone()
        }

        async fn pool_stats(&self) -> VmPoolStats {
            self.pool_stats.clone()
        }
    }

    // ── CapabilityMatchMode tests ───────────────────────────────────────────

    #[test]
    fn test_capability_match_mode_eq() {
        assert_eq!(CapabilityMatchMode::None, CapabilityMatchMode::None);
        assert_eq!(
            CapabilityMatchMode::AllRequired,
            CapabilityMatchMode::AllRequired
        );
        assert_ne!(CapabilityMatchMode::None, CapabilityMatchMode::AllRequired);
    }

    #[test]
    fn test_capability_match_mode_debug() {
        assert_eq!(format!("{:?}", CapabilityMatchMode::None), "None");
        assert_eq!(
            format!("{:?}", CapabilityMatchMode::AllRequired),
            "AllRequired"
        );
    }

    // ── ExecutionPolicy tests ─────────────────────────────────────────────────

    #[test]
    fn test_execution_policy_default() {
        let policy = ExecutionPolicy::default();
        assert_eq!(
            policy.capability_match_mode,
            CapabilityMatchMode::AllRequired
        );
        assert_eq!(policy.max_risk, None);
        assert!(!policy.allow_risk_escalation);
        assert!(policy.allowed_scopes.is_empty());
        assert!(!policy.allow_scope_escalation);
    }

    #[test]
    fn test_execution_policy_debug() {
        let policy = ExecutionPolicy {
            capability_match_mode: CapabilityMatchMode::AllRequired,
            max_risk: Some(crate::CapabilityRisk::Medium),
            allow_risk_escalation: true,
            allowed_scopes: vec!["read".to_string()],
            allow_scope_escalation: true,
        };
        let debug_str = format!("{:?}", policy);
        assert!(debug_str.contains("AllRequired"));
        assert!(debug_str.contains("Medium"));
    }

    // ── ExecutionRegistry tests ───────────────────────────────────────────────

    #[test]
    fn test_execution_registry_new() {
        let registry = ExecutionRegistry::new();
        // Should have http adapter by default
        assert!(registry.get_adapter("http").is_some());
    }

    #[test]
    fn test_execution_registry_with_defaults() {
        let registry = ExecutionRegistry::with_defaults();
        assert!(registry.get_adapter("http").is_some());
    }

    #[test]
    fn test_execution_registry_from_enabled_with_launch_mode_http() {
        let registry = ExecutionRegistry::from_enabled_with_launch_mode(
            vec!["http"],
            ExecutionLaunchMode::HostAdapterCompat,
        )
        .unwrap();
        assert!(registry.get_adapter("http").is_some());
    }

    #[test]
    fn test_execution_registry_from_enabled_with_launch_mode_unknown() {
        let result = ExecutionRegistry::from_enabled_with_launch_mode(
            vec!["unknown"],
            ExecutionLaunchMode::HostAdapterCompat,
        );
        assert!(result.is_err());
        let err = result.err().unwrap();
        assert!(err.to_string().contains("unknown execution adapter"));
    }

    #[test]
    fn test_execution_registry_vm_microvm() {
        // "vm" and "microvm" are accepted without error
        let registry = ExecutionRegistry::from_enabled_with_launch_mode(
            vec!["vm"],
            ExecutionLaunchMode::MicroVM,
        )
        .unwrap();
        // VM adapter is not actually registered, just accepted
        assert!(registry.get_adapter("vm").is_none());
    }

    #[test]
    fn test_execution_registry_with_vm_executor() {
        let mock_executor = Arc::new(MockVmExecutor::new(
            Ok(serde_json::json!({"result": "ok"})),
            VmPoolStats {
                idle: 5,
                active: 2,
                max_total: 10,
                available_permits: 8,
            },
        ));
        let registry = ExecutionRegistry::new().with_vm_executor(mock_executor);
        assert!(registry.is_vm_enabled());
    }

    #[test]
    fn test_execution_registry_is_vm_enabled_false() {
        let registry = ExecutionRegistry::new();
        assert!(!registry.is_vm_enabled());
    }

    #[test]
    fn test_execution_registry_register_adapter() {
        let mut registry = ExecutionRegistry::new();
        let mock = Arc::new(MockAdapter::new(vec![], Ok(serde_json::json!({}))));
        registry.register_adapter("test", mock);
        assert!(registry.get_adapter("test").is_some());
    }

    #[test]
    fn test_execution_registry_adapter_names() {
        let registry = ExecutionRegistry::new();
        let names = registry.adapter_names();
        assert!(names.contains(&"http"));
    }

    #[test]
    fn test_execution_registry_all_capabilities() {
        let registry = ExecutionRegistry::new();
        let caps = registry.all_capabilities();
        // HTTP adapter provides 3 capabilities
        assert_eq!(caps.len(), 3);
    }

    // ── extract_handler_from_args tests ──────────────────────────────────────

    #[test]
    fn test_extract_handler_from_args_found() {
        let args = vec![
            "--executor".to_string(),
            "http".to_string(),
            "--handler".to_string(),
            "get".to_string(),
        ];
        assert_eq!(extract_handler_from_args(&args), Some("get".to_string()));
    }

    #[test]
    fn test_extract_handler_from_args_not_found() {
        let args = vec!["--executor".to_string(), "http".to_string()];
        assert_eq!(extract_handler_from_args(&args), None);
    }

    #[test]
    fn test_extract_handler_from_args_last_arg() {
        let args = vec!["--handler".to_string(), "post".to_string()];
        assert_eq!(extract_handler_from_args(&args), Some("post".to_string()));
    }

    #[test]
    fn test_extract_handler_from_args_empty() {
        let args: Vec<String> = vec![];
        assert_eq!(extract_handler_from_args(&args), None);
    }

    #[test]
    fn test_extract_handler_from_args_handler_is_last() {
        let args = vec!["--handler".to_string()];
        assert_eq!(extract_handler_from_args(&args), None);
    }

    // ── capability_matches tests ─────────────────────────────────────────────

    #[test]
    fn test_capability_matches_network_same() {
        let granted = ExecutionCapability::Network {
            protocol: "http".to_string(),
            operation: "fetch".to_string(),
            scope: "public".to_string(),
        };
        let required = ExecutionCapability::Network {
            protocol: "http".to_string(),
            operation: "fetch".to_string(),
            scope: "public".to_string(),
        };
        assert!(capability_matches(&granted, &required));
    }

    #[test]
    fn test_capability_matches_network_different_protocol() {
        let granted = ExecutionCapability::Network {
            protocol: "http".to_string(),
            operation: "fetch".to_string(),
            scope: "public".to_string(),
        };
        let required = ExecutionCapability::Network {
            protocol: "https".to_string(),
            operation: "fetch".to_string(),
            scope: "public".to_string(),
        };
        assert!(!capability_matches(&granted, &required));
    }

    #[test]
    fn test_capability_matches_filesystem_wildcard_scope() {
        let granted = ExecutionCapability::Filesystem {
            scope: "*".to_string(),
            access: CapabilityAccess::Read,
        };
        let required = ExecutionCapability::Filesystem {
            scope: "/specific".to_string(),
            access: CapabilityAccess::Read,
        };
        assert!(capability_matches(&granted, &required));
    }

    #[test]
    fn test_capability_matches_filesystem_exact_scope() {
        let granted = ExecutionCapability::Filesystem {
            scope: "/workspace".to_string(),
            access: CapabilityAccess::Read,
        };
        let required = ExecutionCapability::Filesystem {
            scope: "/workspace".to_string(),
            access: CapabilityAccess::Read,
        };
        assert!(capability_matches(&granted, &required));
    }

    #[test]
    fn test_capability_matches_tool_name() {
        let granted = ExecutionCapability::Tool {
            name: "bash".to_string(),
            access: CapabilityAccess::Execute,
            scope: "shell".to_string(),
        };
        let required = ExecutionCapability::Tool {
            name: "bash".to_string(),
            access: CapabilityAccess::Execute,
            scope: "shell".to_string(),
        };
        assert!(capability_matches(&granted, &required));
    }

    #[test]
    fn test_capability_matches_tool_different_name() {
        let granted = ExecutionCapability::Tool {
            name: "bash".to_string(),
            access: CapabilityAccess::Execute,
            scope: "shell".to_string(),
        };
        let required = ExecutionCapability::Tool {
            name: "python".to_string(),
            access: CapabilityAccess::Execute,
            scope: "shell".to_string(),
        };
        assert!(!capability_matches(&granted, &required));
    }

    #[test]
    fn test_capability_matches_different_types() {
        let granted = ExecutionCapability::Network {
            protocol: "http".to_string(),
            operation: "fetch".to_string(),
            scope: "public".to_string(),
        };
        let required = ExecutionCapability::Filesystem {
            scope: "/workspace".to_string(),
            access: CapabilityAccess::Read,
        };
        assert!(!capability_matches(&granted, &required));
    }

    // ── execute_via_adapter tests ─────────────────────────────────────────────

    #[tokio::test]
    async fn test_execute_via_adapter_unknown_runtime() {
        let registry = ExecutionRegistry::new();
        let envelope = BoxWorkloadEnvelope {
            runtime_class: RuntimeClass::A3sBox,
            workload_kind: WorkloadKind::ExecutionTask,
            runtime: BoxRuntimeSpec {
                runtime: "unknown/runtime".to_string(),
                entrypoint: "test".to_string(),
                args: vec![],
                env: Default::default(),
            },
            input: serde_json::json!({}),
            labels: Default::default(),
        };
        let result = registry
            .execute_via_adapter(&envelope, Duration::from_secs(5))
            .await;
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("unknown runtime"));
    }

    #[tokio::test]
    async fn test_execute_via_adapter_agent_runner() {
        let registry = ExecutionRegistry::new();
        let envelope = BoxWorkloadEnvelope {
            runtime_class: RuntimeClass::A3sBox,
            workload_kind: WorkloadKind::ExecutionTask,
            runtime: BoxRuntimeSpec {
                runtime: "a3s/agent-runner".to_string(),
                entrypoint: "agent".to_string(),
                args: vec![],
                env: Default::default(),
            },
            input: serde_json::json!({}),
            labels: Default::default(),
        };
        // Should fail because "agent" is not registered
        let result = registry
            .execute_via_adapter(&envelope, Duration::from_secs(5))
            .await;
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .contains("no adapter registered for executor: agent"));
    }

    // ── execute_box_workload tests ───────────────────────────────────────────

    #[tokio::test]
    async fn test_execute_box_workload_invalid_envelope() {
        let registry = ExecutionRegistry::new();
        let envelope = BoxWorkloadEnvelope {
            runtime_class: RuntimeClass::A3sBox,
            workload_kind: WorkloadKind::ExecutionTask,
            runtime: BoxRuntimeSpec {
                runtime: "".to_string(), // Empty runtime - invalid
                entrypoint: "test".to_string(),
                args: vec![],
                env: Default::default(),
            },
            input: serde_json::json!({}),
            labels: Default::default(),
        };
        let result = registry
            .execute_box_workload(&envelope, Duration::from_secs(5))
            .await;
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("invalid workload envelope"));
    }

    #[tokio::test]
    async fn test_execute_box_workload_with_vm_executor() {
        let mock_executor = Arc::new(MockVmExecutor::new(
            Ok(serde_json::json!({"vm": "result"})),
            VmPoolStats::default(),
        ));
        let registry = ExecutionRegistry::new().with_vm_executor(mock_executor);
        let envelope = BoxWorkloadEnvelope {
            runtime_class: RuntimeClass::A3sBox,
            workload_kind: WorkloadKind::ExecutionTask,
            runtime: BoxRuntimeSpec {
                runtime: "test/runtime".to_string(),
                entrypoint: "test".to_string(),
                args: vec![],
                env: Default::default(),
            },
            input: serde_json::json!({}),
            labels: Default::default(),
        };
        let result = registry
            .execute_box_workload(&envelope, Duration::from_secs(5))
            .await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), serde_json::json!({"vm": "result"}));
    }

    #[tokio::test]
    async fn test_execute_box_workload_host_adapter_mode() {
        let registry = ExecutionRegistry::new(); // No VM executor
        let envelope = BoxWorkloadEnvelope {
            runtime_class: RuntimeClass::A3sBox,
            workload_kind: WorkloadKind::ExecutionTask,
            runtime: BoxRuntimeSpec {
                runtime: "unknown/runtime".to_string(),
                entrypoint: "test".to_string(),
                args: vec![],
                env: Default::default(),
            },
            input: serde_json::json!({}),
            labels: Default::default(),
        };
        let result = registry
            .execute_box_workload(&envelope, Duration::from_secs(5))
            .await;
        // Should fail with unknown runtime since no VM executor
        assert!(result.is_err());
    }

    // ── validate_capabilities tests ─────────────────────────────────────────

    #[test]
    fn test_validate_capabilities_unknown_executor() {
        let registry = ExecutionRegistry::new();
        let result = registry.validate_capabilities("unknown", &[], &ExecutionPolicy::default());
        assert!(result.is_err());
        let err = result.err().unwrap();
        assert!(err.contains("no adapter registered"));
    }

    #[test]
    fn test_validate_capabilities_missing_capability() {
        let mock = Arc::new(MockAdapter::new(vec![], Ok(serde_json::json!({}))));
        let mut registry = ExecutionRegistry::new();
        registry.register_adapter("test", mock);

        let required = vec![ExecutionCapability::Network {
            protocol: "http".to_string(),
            operation: "fetch".to_string(),
            scope: "public".to_string(),
        }];

        // No scope escalation and no matching grant should fail
        let policy = ExecutionPolicy {
            allow_scope_escalation: false,
            ..Default::default()
        };
        let result = registry.validate_capabilities("test", &required, &policy);
        assert!(result.is_err());
        let err = result.err().unwrap();
        assert!(err.contains("required capability not granted"));
    }

    #[test]
    fn test_validate_capabilities_risk_exceeded() {
        let mock = Arc::new(MockAdapter::new(
            vec![ExecutionCapabilityGrant {
                capability: ExecutionCapability::Network {
                    protocol: "http".to_string(),
                    operation: "fetch".to_string(),
                    scope: "public".to_string(),
                },
                risk: crate::CapabilityRisk::High,
            }],
            Ok(serde_json::json!({})),
        ));
        let mut registry = ExecutionRegistry::new();
        registry.register_adapter("test", mock);

        let required = vec![ExecutionCapability::Network {
            protocol: "http".to_string(),
            operation: "fetch".to_string(),
            scope: "public".to_string(),
        }];

        // Risk exceeds max without allow_risk_escalation
        let policy = ExecutionPolicy {
            max_risk: Some(crate::CapabilityRisk::Low),
            allow_risk_escalation: false,
            ..Default::default()
        };
        let result = registry.validate_capabilities("test", &required, &policy);
        assert!(result.is_err());
        let err = result.err().unwrap();
        assert!(err.contains("exceeds max risk"));
    }

    #[test]
    fn test_validate_capabilities_success_with_risk_escalation() {
        let mock = Arc::new(MockAdapter::new(
            vec![ExecutionCapabilityGrant {
                capability: ExecutionCapability::Network {
                    protocol: "http".to_string(),
                    operation: "fetch".to_string(),
                    scope: "public".to_string(),
                },
                risk: crate::CapabilityRisk::High,
            }],
            Ok(serde_json::json!({})),
        ));
        let mut registry = ExecutionRegistry::new();
        registry.register_adapter("test", mock);

        let required = vec![ExecutionCapability::Network {
            protocol: "http".to_string(),
            operation: "fetch".to_string(),
            scope: "public".to_string(),
        }];

        // Risk exceeds max but allow_risk_escalation is true
        let policy = ExecutionPolicy {
            max_risk: Some(crate::CapabilityRisk::Low),
            allow_risk_escalation: true,
            ..Default::default()
        };
        let result = registry.validate_capabilities("test", &required, &policy);
        assert!(result.is_ok());
    }

    // ── box_runtime_pool_snapshot tests ───────────────────────────────────────

    #[tokio::test]
    async fn test_box_runtime_pool_snapshot_no_vm() {
        let registry = ExecutionRegistry::new();
        let snapshot = registry.box_runtime_pool_snapshot().await;
        assert_eq!(snapshot.launch_mode, ExecutionLaunchMode::HostAdapterCompat);
        assert_eq!(snapshot.idle_vms, 0);
        assert_eq!(snapshot.active_vms, 0);
        assert_eq!(snapshot.total_vms, 0);
        assert_eq!(snapshot.available_vms, 0);
        assert!(!snapshot.has_capacity_pressure);
    }

    #[tokio::test]
    async fn test_box_runtime_pool_snapshot_with_vm() {
        let mock_executor = Arc::new(MockVmExecutor::new(
            Ok(serde_json::json!({})),
            VmPoolStats {
                idle: 5,
                active: 3,
                max_total: 10,
                available_permits: 7,
            },
        ));
        let registry = ExecutionRegistry::new().with_vm_executor(mock_executor);
        let snapshot = registry.box_runtime_pool_snapshot().await;

        assert_eq!(snapshot.idle_vms, 5);
        assert_eq!(snapshot.active_vms, 3);
        assert_eq!(snapshot.total_vms, 10);
        assert_eq!(snapshot.max_total_vms, 10);
        assert_eq!(snapshot.available_vms, 7);
        assert!(!snapshot.has_capacity_pressure);
    }

    #[tokio::test]
    async fn test_box_runtime_pool_snapshot_capacity_pressure() {
        let mock_executor = Arc::new(MockVmExecutor::new(
            Ok(serde_json::json!({})),
            VmPoolStats {
                idle: 0,
                active: 10,
                max_total: 10,
                available_permits: 0,
            },
        ));
        let registry = ExecutionRegistry::new().with_vm_executor(mock_executor);
        let snapshot = registry.box_runtime_pool_snapshot().await;

        assert!(snapshot.has_capacity_pressure);
    }

    #[tokio::test]
    async fn test_box_runtime_pool_snapshot_occupancy_ratio() {
        let mock_executor = Arc::new(MockVmExecutor::new(
            Ok(serde_json::json!({})),
            VmPoolStats {
                idle: 2,
                active: 8,
                max_total: 10,
                available_permits: 2,
            },
        ));
        let registry = ExecutionRegistry::new().with_vm_executor(mock_executor);
        let snapshot = registry.box_runtime_pool_snapshot().await;

        assert_eq!(snapshot.occupancy_ratio, 0.8);
        assert_eq!(snapshot.active_ratio, 0.8);
    }

    #[test]
    fn test_box_runtime_pool_snapshot_default() {
        let snapshot = BoxRuntimePoolSnapshot {
            launch_mode: ExecutionLaunchMode::MicroVM,
            image_pool_count: 5,
            idle_vms: 3,
            active_vms: 2,
            total_vms: 10,
            max_total_vms: 10,
            available_vms: 5,
            occupancy_ratio: 0.2,
            active_ratio: 0.2,
            has_capacity_pressure: false,
        };
        assert_eq!(snapshot.launch_mode, ExecutionLaunchMode::MicroVM);
        assert_eq!(snapshot.total_vms, 10);
    }

    // ── Default trait tests ─────────────────────────────────────────────────

    #[test]
    fn test_execution_registry_default() {
        let registry = ExecutionRegistry::default();
        // Should have http adapter by default
        assert!(registry.get_adapter("http").is_some());
    }
}
