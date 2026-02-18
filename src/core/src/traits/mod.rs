//! Core trait abstractions for pluggable backends.
//!
//! These traits define the extension points of A3S Box. The runtime provides
//! default implementations, but consumers can swap in their own backends
//! (e.g., a different registry client, a Redis-backed cache, etc.).

pub mod audit;
pub mod cache;
pub mod credential;
pub mod event;
pub mod metrics;
pub mod registry;
pub mod store;

pub use audit::AuditSink;
pub use cache::{CacheBackend, CacheEntry, CacheStats};
pub use credential::CredentialProvider;
pub use event::EventBus;
pub use metrics::{MetricsCollector, NoopMetrics};
pub use registry::{ImageRegistry, PulledImage};
pub use store::{ImageStoreBackend, NetworkStoreBackend, SnapshotStoreBackend, StoredImage, VolumeStoreBackend};
