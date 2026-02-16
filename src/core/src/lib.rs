//! A3S Box Core - Foundational Types and Abstractions
//!
//! This module provides the foundational types, traits, and abstractions
//! used across the A3S Box MicroVM runtime.

pub mod audit;
pub mod compose;
pub mod config;
pub mod dns;
pub mod error;
pub mod event;
pub mod exec;
pub mod log;
pub mod network;
pub mod platform;
pub mod pty;
pub mod security;
pub mod snapshot;
pub mod tee;
pub mod volume;

// Re-export commonly used types
pub use audit::{AuditAction, AuditConfig, AuditEvent, AuditOutcome};
pub use compose::ComposeConfig;
pub use config::{BoxConfig, ResourceConfig, ResourceLimits};
pub use error::{BoxError, Result};
pub use event::{BoxEvent, EventEmitter};
pub use exec::{ExecOutput, ExecRequest};
pub use network::{IsolationMode, NetworkConfig, NetworkEndpoint, NetworkMode, NetworkPolicy};
pub use platform::Platform;
pub use pty::PTY_VSOCK_PORT;
pub use security::{SeccompMode, SecurityConfig};
pub use snapshot::{SnapshotConfig, SnapshotMetadata};
pub use tee::ATTEST_VSOCK_PORT;
pub use tee::{TeeCapability, TeeType, detect_tee, is_tee_available};
pub use volume::VolumeConfig;

/// A3S Box version
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
