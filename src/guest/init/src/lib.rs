//! Guest init library for a3s-box VM.
//!
//! Provides namespace isolation utilities for running agent and business code
//! in isolated environments within the same VM, an exec server for
//! host-to-guest command execution, and network configuration for
//! passt-based virtio-net interfaces.

pub mod attest_server;
pub mod exec_server;
pub mod host_config;
pub mod namespace;
pub mod network;
pub mod port_forward;
pub mod pty_server;
pub mod user;

pub use namespace::{spawn_isolated, NamespaceConfig, NamespaceError};
pub use network::configure_guest_network;
