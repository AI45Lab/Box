//! Network management for container-to-container communication.
//!
//! Provides `NetworkStore` for persisting network state and
//! platform-specific network backend managers for bridge networking:
//! - Linux: `PasstManager` (passt Unix stream socket)
//! - macOS: `NetProxyManager` (pure-Rust vfkit server, no external binary)

#[cfg(target_os = "macos")]
mod netproxy;
mod passt;
mod store;

#[cfg(target_os = "macos")]
pub use netproxy::{spawn_inherited_netproxy, NetProxyManager};
pub use passt::PasstManager;
pub use store::NetworkStore;

/// Platform-agnostic handle to a running network backend process or thread.
pub trait NetworkBackend: Send + Sync {
    /// Path to the Unix socket used to communicate with this backend.
    fn socket_path(&self) -> &std::path::Path;
    /// Stop the backend and clean up the socket.
    fn stop(&mut self);
}
