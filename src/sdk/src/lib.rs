//! A3S Box SDK - MicroVM workload execution for Lambda
//!
//! This crate provides the SDK interface for executing workloads inside
//! isolated A3S Box MicroVMs. It wraps the lower-level a3s-box-runtime
//! to provide a simple execution interface.
//!
//! # Core Concepts
//!
//! - **ExecutionRegistry**: Main entry point for workload execution
//! - **ExecutionAdapter**: Pluggable backends for different execution modes
//! - **BoxWorkloadEnvelope**: Workload specification (runtime, entrypoint, input)
//!
//! # Example
//!
//! ```rust,no_run
//! use a3s_box_sdk::{ExecutionRegistry, BoxWorkloadEnvelope, RuntimeClass, WorkloadKind, BoxRuntimeSpec};
//! use std::time::Duration;
//!
//! #[tokio::main]
//! async fn main() -> Result<(), Box<dyn std::error::Error>> {
//!     let registry = ExecutionRegistry::new();
//!
//!     let envelope = BoxWorkloadEnvelope {
//!         runtime_class: RuntimeClass::A3sBox,
//!         workload_kind: WorkloadKind::ExecutionTask,
//!         runtime: BoxRuntimeSpec {
//!             runtime: "a3s/executor/http".into(),
//!             entrypoint: "a3s-executor".into(),
//!             args: vec!["--executor".into(), "http".into(), "--handler".into(), "get".into()],
//!             env: Default::default(),
//!         },
//!         input: serde_json::json!({"url": "https://example.com"}),
//!         labels: Default::default(),
//!     };
//!
//!     let result = registry.execute_box_workload(&envelope, Duration::from_secs(300)).await;
//!     println!("Result: {:?}", result);
//!     Ok(())
//! }
//! ```

pub mod adapter;
pub mod error;
pub mod registry;
pub mod vm;

pub use adapter::{
    CapabilityAccess, CapabilityRisk, ExecutionAdapter, ExecutionCapability,
    ExecutionCapabilityGrant, HttpExecutionAdapter,
};
pub use error::SdkError;
pub use registry::{
    BoxRuntimePoolSnapshot, CapabilityMatchMode, ExecutionPolicy, ExecutionRegistry, Result,
};
pub use vm::{VmExecutor, VmPoolStats};

pub use a3s_box_core::{
    BoxRuntimeSpec, BoxWorkloadEnvelope, ExecutionLaunchMode, RuntimeClass, WorkloadKind,
};
