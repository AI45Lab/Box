//! VM workload types for Lambda integration
//!
//! These types define the contract between Lambda and the Box runtime
//! for workload execution.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Execution launch mode
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExecutionLaunchMode {
    /// Run in actual MicroVMs
    MicroVM,
    /// Run using host adapters (for testing/development)
    HostAdapterCompat,
    /// Hybrid mode - MicroVM when available, host otherwise
    Hybrid,
}

impl Default for ExecutionLaunchMode {
    fn default() -> Self {
        ExecutionLaunchMode::HostAdapterCompat
    }
}

/// Runtime class - the type of runtime to use
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeClass {
    /// A3S Box MicroVM
    A3sBox,
}

/// Workload kind - the type of workload being executed
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkloadKind {
    /// Agent invocation workload
    AgentInvocation,
    /// Generic execution task
    ExecutionTask,
}

/// Box runtime specification - defines how to run a workload
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BoxRuntimeSpec {
    /// Runtime identifier (e.g., "a3s/executor/http")
    pub runtime: String,
    /// Entrypoint to execute
    pub entrypoint: String,
    /// Command-line arguments
    #[serde(default)]
    pub args: Vec<String>,
    /// Environment variables
    #[serde(default)]
    pub env: HashMap<String, String>,
}

impl BoxRuntimeSpec {
    /// Create a runtime spec for agent invocation.
    pub fn for_agent_invocation(agent: &str, version: &str) -> Self {
        Self {
            runtime: "a3s/agent-runner".into(),
            entrypoint: "a3s-code".into(),
            args: vec![
                "run".into(),
                "--package".into(),
                format!("registry://{agent}@{version}"),
            ],
            env: HashMap::new(),
        }
    }

    /// Create a runtime spec for an execution adapter.
    pub fn for_execution_adapter(executor: &str, handler: &str) -> Self {
        Self {
            runtime: format!("a3s/executor/{executor}"),
            entrypoint: "a3s-executor".into(),
            args: vec![
                "--executor".into(),
                executor.into(),
                "--handler".into(),
                handler.into(),
            ],
            env: HashMap::new(),
        }
    }
}

/// Box workload envelope - the complete workload specification
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BoxWorkloadEnvelope {
    /// Runtime class to use
    pub runtime_class: RuntimeClass,
    /// Kind of workload
    pub workload_kind: WorkloadKind,
    /// Runtime specification
    pub runtime: BoxRuntimeSpec,
    /// Workload input (JSON)
    pub input: serde_json::Value,
    /// Labels/tags for the workload
    #[serde(default)]
    pub labels: HashMap<String, String>,
}

impl BoxWorkloadEnvelope {
    /// Validate the workload envelope.
    pub fn validate(&self) -> Result<(), String> {
        if self.runtime.runtime.trim().is_empty() {
            return Err("box workload envelope requires a non-empty runtime".into());
        }

        if self.runtime.entrypoint.trim().is_empty() {
            return Err("box workload envelope requires a non-empty entrypoint".into());
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── ExecutionLaunchMode tests ───────────────────────────────────────────

    #[test]
    fn test_execution_launch_mode_default() {
        assert_eq!(ExecutionLaunchMode::default(), ExecutionLaunchMode::HostAdapterCompat);
    }

    #[test]
    fn test_execution_launch_mode_variants() {
        assert_ne!(ExecutionLaunchMode::MicroVM, ExecutionLaunchMode::HostAdapterCompat);
        assert_ne!(ExecutionLaunchMode::HostAdapterCompat, ExecutionLaunchMode::Hybrid);
        assert_ne!(ExecutionLaunchMode::MicroVM, ExecutionLaunchMode::Hybrid);
    }

    #[test]
    fn test_execution_launch_mode_eq() {
        assert_eq!(ExecutionLaunchMode::MicroVM, ExecutionLaunchMode::MicroVM);
        assert_eq!(ExecutionLaunchMode::HostAdapterCompat, ExecutionLaunchMode::HostAdapterCompat);
        assert_eq!(ExecutionLaunchMode::Hybrid, ExecutionLaunchMode::Hybrid);
    }

    #[test]
    fn test_execution_launch_mode_debug() {
        assert_eq!(format!("{:?}", ExecutionLaunchMode::MicroVM), "MicroVM");
        assert_eq!(format!("{:?}", ExecutionLaunchMode::HostAdapterCompat), "HostAdapterCompat");
        assert_eq!(format!("{:?}", ExecutionLaunchMode::Hybrid), "Hybrid");
    }

    // ── RuntimeClass tests ────────────────────────────────────────────────

    #[test]
    fn test_runtime_class_serde_snake_case() {
        let json = serde_json::to_string(&RuntimeClass::A3sBox).unwrap();
        assert_eq!(json, "\"a3s_box\"");
    }

    #[test]
    fn test_runtime_class_serde_deserialize() {
        let json = "\"a3s_box\"";
        let parsed: RuntimeClass = serde_json::from_str(json).unwrap();
        assert_eq!(parsed, RuntimeClass::A3sBox);
    }

    #[test]
    fn test_runtime_class_default() {
        // RuntimeClass doesn't implement Default, but we can test Clone and Eq
        let original = RuntimeClass::A3sBox;
        let cloned = original.clone();
        assert_eq!(cloned, original);
    }

    // ── WorkloadKind tests ────────────────────────────────────────────────

    #[test]
    fn test_workload_kind_serde_snake_case() {
        assert_eq!(serde_json::to_string(&WorkloadKind::AgentInvocation).unwrap(), "\"agent_invocation\"");
        assert_eq!(serde_json::to_string(&WorkloadKind::ExecutionTask).unwrap(), "\"execution_task\"");
    }

    #[test]
    fn test_workload_kind_serde_deserialize() {
        let json1 = "\"agent_invocation\"";
        let parsed1: WorkloadKind = serde_json::from_str(json1).unwrap();
        assert_eq!(parsed1, WorkloadKind::AgentInvocation);

        let json2 = "\"execution_task\"";
        let parsed2: WorkloadKind = serde_json::from_str(json2).unwrap();
        assert_eq!(parsed2, WorkloadKind::ExecutionTask);
    }

    // ── BoxRuntimeSpec tests ───────────────────────────────────────────────

    #[test]
    fn test_box_runtime_spec_new() {
        let spec = BoxRuntimeSpec {
            runtime: "test/runtime".to_string(),
            entrypoint: "test-entry".to_string(),
            args: vec!["arg1".to_string()],
            env: std::collections::HashMap::new(),
        };
        assert_eq!(spec.runtime, "test/runtime");
        assert_eq!(spec.entrypoint, "test-entry");
        assert_eq!(spec.args, vec!["arg1"]);
    }

    #[test]
    fn test_box_runtime_spec_for_agent_invocation() {
        let spec = BoxRuntimeSpec::for_agent_invocation("my-agent", "v1.0.0");
        assert_eq!(spec.runtime, "a3s/agent-runner");
        assert_eq!(spec.entrypoint, "a3s-code");
        assert!(spec.args.contains(&"run".to_string()));
        assert!(spec.args.contains(&"--package".to_string()));
        assert!(spec.args.contains(&"registry://my-agent@v1.0.0".to_string()));
        assert!(spec.env.is_empty());
    }

    #[test]
    fn test_box_runtime_spec_for_execution_adapter() {
        let spec = BoxRuntimeSpec::for_execution_adapter("http", "get");
        assert_eq!(spec.runtime, "a3s/executor/http");
        assert_eq!(spec.entrypoint, "a3s-executor");
        assert!(spec.args.contains(&"--executor".to_string()));
        assert!(spec.args.contains(&"http".to_string()));
        assert!(spec.args.contains(&"--handler".to_string()));
        assert!(spec.args.contains(&"get".to_string()));
    }

    #[test]
    fn test_box_runtime_spec_serde_roundtrip() {
        let spec = BoxRuntimeSpec::for_execution_adapter("bash", "run");
        let json = serde_json::to_string(&spec).unwrap();
        let parsed: BoxRuntimeSpec = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.runtime, spec.runtime);
        assert_eq!(parsed.entrypoint, spec.entrypoint);
        assert_eq!(parsed.args, spec.args);
        assert_eq!(parsed.env, spec.env);
    }

    // ── BoxWorkloadEnvelope tests ─────────────────────────────────────────

    #[test]
    fn test_box_workload_envelope_new() {
        let envelope = BoxWorkloadEnvelope {
            runtime_class: RuntimeClass::A3sBox,
            workload_kind: WorkloadKind::ExecutionTask,
            runtime: BoxRuntimeSpec::for_execution_adapter("bash", "run"),
            input: serde_json::json!({"test": "data"}),
            labels: std::collections::HashMap::new(),
        };
        assert_eq!(envelope.runtime_class, RuntimeClass::A3sBox);
        assert_eq!(envelope.workload_kind, WorkloadKind::ExecutionTask);
        assert_eq!(envelope.labels.len(), 0);
    }

    #[test]
    fn test_box_workload_envelope_validate_success() {
        let envelope = BoxWorkloadEnvelope {
            runtime_class: RuntimeClass::A3sBox,
            workload_kind: WorkloadKind::ExecutionTask,
            runtime: BoxRuntimeSpec {
                runtime: "test/runtime".to_string(),
                entrypoint: "test".to_string(),
                args: vec![],
                env: std::collections::HashMap::new(),
            },
            input: serde_json::json!({}),
            labels: std::collections::HashMap::new(),
        };
        assert!(envelope.validate().is_ok());
    }

    #[test]
    fn test_box_workload_envelope_validate_empty_runtime() {
        let envelope = BoxWorkloadEnvelope {
            runtime_class: RuntimeClass::A3sBox,
            workload_kind: WorkloadKind::ExecutionTask,
            runtime: BoxRuntimeSpec {
                runtime: "".to_string(),
                entrypoint: "test".to_string(),
                args: vec![],
                env: std::collections::HashMap::new(),
            },
            input: serde_json::json!({}),
            labels: std::collections::HashMap::new(),
        };
        let result = envelope.validate();
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("non-empty runtime"));
    }

    #[test]
    fn test_box_workload_envelope_validate_whitespace_runtime() {
        let envelope = BoxWorkloadEnvelope {
            runtime_class: RuntimeClass::A3sBox,
            workload_kind: WorkloadKind::ExecutionTask,
            runtime: BoxRuntimeSpec {
                runtime: "   ".to_string(),
                entrypoint: "test".to_string(),
                args: vec![],
                env: std::collections::HashMap::new(),
            },
            input: serde_json::json!({}),
            labels: std::collections::HashMap::new(),
        };
        let result = envelope.validate();
        assert!(result.is_err());
    }

    #[test]
    fn test_box_workload_envelope_validate_empty_entrypoint() {
        let envelope = BoxWorkloadEnvelope {
            runtime_class: RuntimeClass::A3sBox,
            workload_kind: WorkloadKind::ExecutionTask,
            runtime: BoxRuntimeSpec {
                runtime: "test/runtime".to_string(),
                entrypoint: "".to_string(),
                args: vec![],
                env: std::collections::HashMap::new(),
            },
            input: serde_json::json!({}),
            labels: std::collections::HashMap::new(),
        };
        let result = envelope.validate();
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("non-empty entrypoint"));
    }

    #[test]
    fn test_box_workload_envelope_validate_whitespace_entrypoint() {
        let envelope = BoxWorkloadEnvelope {
            runtime_class: RuntimeClass::A3sBox,
            workload_kind: WorkloadKind::ExecutionTask,
            runtime: BoxRuntimeSpec {
                runtime: "test/runtime".to_string(),
                entrypoint: "  \t\n  ".to_string(),
                args: vec![],
                env: std::collections::HashMap::new(),
            },
            input: serde_json::json!({}),
            labels: std::collections::HashMap::new(),
        };
        let result = envelope.validate();
        assert!(result.is_err());
    }

    #[test]
    fn test_box_workload_envelope_with_labels() {
        let mut labels = std::collections::HashMap::new();
        labels.insert("env".to_string(), "test".to_string());
        labels.insert("version".to_string(), "1.0".to_string());

        let envelope = BoxWorkloadEnvelope {
            runtime_class: RuntimeClass::A3sBox,
            workload_kind: WorkloadKind::AgentInvocation,
            runtime: BoxRuntimeSpec::for_agent_invocation("test-agent", "v1"),
            input: serde_json::json!({"input": "data"}),
            labels,
        };
        assert_eq!(envelope.labels.len(), 2);
        assert_eq!(envelope.labels.get("env").unwrap(), "test");
    }

    #[test]
    fn test_box_workload_envelope_serde_roundtrip() {
        let envelope = BoxWorkloadEnvelope {
            runtime_class: RuntimeClass::A3sBox,
            workload_kind: WorkloadKind::ExecutionTask,
            runtime: BoxRuntimeSpec::for_execution_adapter("bash", "run"),
            input: serde_json::json!({"key": "value"}),
            labels: std::collections::HashMap::new(),
        };
        let json = serde_json::to_string(&envelope).unwrap();
        let parsed: BoxWorkloadEnvelope = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.runtime_class, envelope.runtime_class);
        assert_eq!(parsed.workload_kind, envelope.workload_kind);
        assert_eq!(parsed.runtime.runtime, envelope.runtime.runtime);
    }

    // ── Edge cases ────────────────────────────────────────────────────────

    #[test]
    fn test_box_runtime_spec_env_defaults_to_empty() {
        let spec: BoxRuntimeSpec = serde_json::from_str(r#"{"runtime":"test","entrypoint":"entry"}"#).unwrap();
        assert!(spec.env.is_empty());
    }

    #[test]
    fn test_box_runtime_spec_args_defaults_to_empty() {
        let spec: BoxRuntimeSpec = serde_json::from_str(r#"{"runtime":"test","entrypoint":"entry"}"#).unwrap();
        assert!(spec.args.is_empty());
    }

    #[test]
    fn test_box_workload_envelope_labels_defaults_to_empty() {
        let json = r#"{
            "runtime_class": "a3s_box",
            "workload_kind": "execution_task",
            "runtime": {"runtime": "test", "entrypoint": "entry"},
            "input": {}
        }"#;
        let parsed: BoxWorkloadEnvelope = serde_json::from_str(json).unwrap();
        assert!(parsed.labels.is_empty());
    }
}
