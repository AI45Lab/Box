//! A3S Box Runtime - MicroVM runtime implementation.
//!
//! This module provides the actual runtime implementation for A3S Box,
//! including VM management, OCI image handling, rootfs building, and gRPC health checks.
//!
//! # Feature Flags
//!
//! - `pool` — Warm VM pool with autoscaling (enabled by default)
//! - `scale` — Multi-node scale manager and instance registry (enabled by default)
//! - `compose` — Multi-container compose orchestration (enabled by default)
//! - `operator` — Kubernetes CRD autoscaler controller (enabled by default)
//! - `build` — Dockerfile build engine (enabled by default)

#![allow(clippy::result_large_err)]

// -- Core modules (always compiled) --
pub mod audit;
pub mod cache;
pub mod fs;
pub mod grpc;
pub mod host_check;
pub mod krun;
pub mod log;
pub mod network;
pub mod oci;
pub mod prom;
pub mod resize;
pub mod rootfs;
pub mod snapshot;
pub mod tee;
pub mod vm;
pub mod vmm;
pub mod volume;

// -- Optional modules (feature-gated) --
#[cfg(feature = "compose")]
pub mod compose;
#[cfg(feature = "operator")]
pub mod operator;
#[cfg(feature = "pool")]
pub mod pool;
#[cfg(feature = "scale")]
pub mod scale;

// ── Core re-exports (used by CLI, CRI, SDK, shim) ──

// Audit
pub use audit::{read_audit_log, AuditLog, AuditQuery};

// gRPC clients
pub use grpc::{AttestationClient, ExecClient, PtyClient, RaTlsAttestationClient, StreamingExec};
pub use grpc::{SealClient, SecretEntry, SecretInjector};

// Host checks
pub use host_check::check_virtualization_support;

// Network
pub use network::NetworkStore;

// OCI images
pub use a3s_box_core::StoredImage;
pub use oci::{CredentialStore, PushResult, RegistryPusher};
pub use oci::{ImagePuller, ImageReference, ImageStore, RegistryAuth};
pub use oci::{OciImage, SignResult, SignaturePolicy};

// Metrics
pub use prom::RuntimeMetrics;

// Snapshot
pub use snapshot::SnapshotStore;

// TEE
pub use tee::{seal, unseal};
pub use tee::{
    verify_attestation, verify_attestation_with_time, AmdKdsClient, AttestationPolicy,
    MinTcbPolicy, PolicyResult, VerificationResult,
};
pub use tee::{AttestationReport, AttestationRequest, PlatformInfo};

// VM
pub use vm::{BoxState, VmManager};
pub use vmm::{
    Entrypoint, FsMount, InstanceSpec, NetworkInstanceConfig, ShimHandler, TeeInstanceConfig,
    VmController, VmHandler, VmMetrics, VmmProvider,
};

// Resize
pub use resize::{validate_update, ResizeResult, ResourceUpdate};

// Volume
pub use volume::VolumeStore;

// ── Feature-gated re-exports ──

#[cfg(feature = "build")]
pub use oci::{BuildConfig, Dockerfile, Instruction};

#[cfg(feature = "compose")]
pub use compose::{ComposeProject, HealthCheckSpec};

#[cfg(feature = "operator")]
pub use operator::AutoscalerController;

#[cfg(feature = "pool")]
pub use pool::WarmPool;

#[cfg(feature = "scale")]
pub use scale::ScaleManager;

// ── Constants ──

/// A3S Box Runtime version.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Default vsock port for exec server in the guest.
pub const EXEC_VSOCK_PORT: u32 = 4089;

/// Default vsock port for PTY server in the guest.
pub const PTY_VSOCK_PORT: u32 = 4090;

/// Default vsock port for TEE attestation server in the guest.
pub const ATTEST_VSOCK_PORT: u32 = 4091;

/// Default maximum image cache size: 10 GB.
pub const DEFAULT_IMAGE_CACHE_SIZE: u64 = 10 * 1024 * 1024 * 1024;
