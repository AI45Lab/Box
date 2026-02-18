//! Metrics collector abstraction.
//!
//! Decouples runtime instrumentation from Prometheus. Implementations
//! can emit metrics to any backend: Prometheus, StatsD, OpenTelemetry,
//! or a no-op sink for environments where metrics are not needed.

/// Abstraction over runtime metrics collection.
///
/// The runtime calls these methods at key lifecycle points. Each method
/// corresponds to a specific observable event. Implementations translate
/// these calls into their backend's metric primitives.
///
/// # Thread Safety
///
/// Implementations must be `Send + Sync + Clone`. The runtime clones
/// the collector and shares it across async tasks.
pub trait MetricsCollector: Send + Sync + Clone {
    // -- VM lifecycle --

    /// Record a VM boot duration in seconds.
    fn record_vm_boot(&self, duration_secs: f64);

    /// Increment the count of VMs in a given state.
    fn inc_vm_state(&self, state: &str);

    /// Decrement the count of VMs in a given state.
    fn dec_vm_state(&self, state: &str);

    /// Increment total VMs created.
    fn inc_vm_created(&self);

    /// Increment total VMs destroyed.
    fn inc_vm_destroyed(&self);

    // -- Exec operations --

    /// Record an exec command completion.
    fn record_exec(&self, duration_secs: f64, success: bool);

    // -- Image operations --

    /// Record a rootfs cache hit.
    fn inc_cache_hit(&self);

    /// Record a rootfs cache miss.
    fn inc_cache_miss(&self);
}

/// No-op metrics collector for environments where metrics are disabled.
#[derive(Debug, Clone, Default)]
pub struct NoopMetrics;

impl MetricsCollector for NoopMetrics {
    fn record_vm_boot(&self, _duration_secs: f64) {}
    fn inc_vm_state(&self, _state: &str) {}
    fn dec_vm_state(&self, _state: &str) {}
    fn inc_vm_created(&self) {}
    fn inc_vm_destroyed(&self) {}
    fn record_exec(&self, _duration_secs: f64, _success: bool) {}
    fn inc_cache_hit(&self) {}
    fn inc_cache_miss(&self) {}
}
