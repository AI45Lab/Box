//! A3S Box CRI - Kubernetes Container Runtime Interface implementation.
//!
//! Maps CRI concepts to A3S Box primitives:
//! - Pod Sandbox → Box instance (one microVM per pod)
//! - Container → Session within Box

pub mod config_mapper;
pub mod container;
pub mod error;
pub mod image_service;
pub mod persistent_store;
pub mod runtime_service;
pub mod sandbox;
pub mod server;
pub mod state;
pub mod streaming;

/// Generated CRI v1 protobuf types.
pub mod cri_api {
    tonic::include_proto!("runtime.v1");
}
