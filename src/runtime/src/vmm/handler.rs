//! ShimHandler — concrete VmHandler for a libkrun shim subprocess.

pub use a3s_box_core::vmm::{DEFAULT_SHUTDOWN_TIMEOUT_MS, VmHandler, VmMetrics};

use a3s_box_core::error::Result;
use std::process::Child;
use std::sync::Mutex;
use sysinfo::{Pid, System};

/// Handler for a running VM subprocess (shim process).
///
/// Provides lifecycle operations (stop, metrics, status) for a VM identified by PID.
pub struct ShimHandler {
    pid: u32,
    box_id: String,
    /// Child process handle for proper lifecycle management.
    /// When we spawn the process, we keep the Child to properly wait() on stop.
    /// When we attach to an existing process, this is None.
    process: Option<Child>,
    /// Shared System instance for CPU metrics calculation across calls.
    /// CPU usage requires comparing snapshots over time, so we must reuse the same System.
    metrics_sys: Mutex<System>,
    /// Exit code of the shim process, set when stop() collects the exit status.
    exit_code: Option<i32>,
}

impl ShimHandler {
    /// Create a handler for a spawned VM with process ownership.
    ///
    /// This constructor takes ownership of the Child process handle for proper
    /// lifecycle management (clean shutdown with wait()).
    pub fn from_child(process: Child, box_id: String) -> Self {
        let pid = process.id();
        Self {
            pid,
            box_id,
            process: Some(process),
            metrics_sys: Mutex::new(System::new()),
            exit_code: None,
        }
    }

    /// Create a handler for an existing VM (attach mode).
    ///
    /// Used when reconnecting to a running box. We don't have a Child handle,
    /// so we manage the process by PID only.
    pub fn from_pid(pid: u32, box_id: String) -> Self {
        Self {
            pid,
            box_id,
            process: None,
            metrics_sys: Mutex::new(System::new()),
            exit_code: None,
        }
    }

    /// Get the box ID.
    pub fn box_id(&self) -> &str {
        &self.box_id
    }
}

impl VmHandler for ShimHandler {
    fn pid(&self) -> u32 {
        self.pid
    }

    fn stop(&mut self, signal: i32, timeout_ms: u64) -> Result<()> {
        // Graceful shutdown: send configured signal first, wait, then SIGKILL if needed.
        // This gives libkrun time to flush its virtio-blk buffers to disk.

        if let Some(mut process) = self.process.take() {
            // Step 1: Send configured stop signal for graceful shutdown
            let pid = process.id();
            tracing::debug!(pid, box_id = %self.box_id, signal, "Sending stop signal to VM process");
            unsafe {
                libc::kill(pid as i32, signal);
            }

            // Step 2: Wait with timeout for process to exit
            let start = std::time::Instant::now();
            loop {
                match process.try_wait() {
                    Ok(Some(status)) => {
                        tracing::debug!(pid, ?status, "VM process exited gracefully");
                        self.exit_code = status.code();
                        return Ok(());
                    }
                    Ok(None) => {
                        // Still running, check timeout
                        if start.elapsed().as_millis() > timeout_ms as u128 {
                            tracing::warn!(
                                pid,
                                timeout_ms,
                                "VM process did not exit gracefully, sending SIGKILL"
                            );
                            let _ = process.kill();
                            if let Ok(status) = process.wait() {
                                self.exit_code = status.code();
                            }
                            return Ok(());
                        }
                        // Brief sleep before checking again
                        std::thread::sleep(std::time::Duration::from_millis(50));
                    }
                    Err(e) => {
                        tracing::warn!(pid, error = %e, "Error checking process status, forcing kill");
                        let _ = process.kill();
                        let _ = process.wait();
                        return Ok(());
                    }
                }
            }
        } else {
            // Attached mode: use configured signal then SIGKILL with polling
            tracing::debug!(pid = self.pid, box_id = %self.box_id, signal, "Sending stop signal to attached VM process");
            unsafe {
                libc::kill(self.pid as i32, signal);
            }

            // Poll for exit with timeout
            let start = std::time::Instant::now();
            loop {
                let mut status: i32 = 0;
                let result = unsafe { libc::waitpid(self.pid as i32, &mut status, libc::WNOHANG) };

                if result > 0 {
                    tracing::debug!(pid = self.pid, "VM process exited gracefully");
                    return Ok(());
                }
                if result < 0 {
                    // Error - process may not be our child (common in attached mode)
                    let exists = unsafe { libc::kill(self.pid as i32, 0) } == 0;
                    if !exists {
                        return Ok(()); // Already dead
                    }
                }

                if start.elapsed().as_millis() > timeout_ms as u128 {
                    tracing::warn!(
                        pid = self.pid,
                        timeout_ms,
                        "VM process did not exit gracefully, sending SIGKILL"
                    );
                    unsafe {
                        libc::kill(self.pid as i32, libc::SIGKILL);
                    }
                    return Ok(());
                }

                std::thread::sleep(std::time::Duration::from_millis(50));
            }
        }
    }

    fn metrics(&self) -> VmMetrics {
        let pid = Pid::from_u32(self.pid);

        // Use the shared System instance for stateful CPU tracking
        let mut sys = match self.metrics_sys.lock() {
            Ok(guard) => guard,
            Err(e) => {
                tracing::warn!(error = %e, "metrics_sys lock poisoned");
                return VmMetrics::default();
            }
        };

        // Refresh process info - this updates the internal state for delta calculation
        sys.refresh_process(pid);

        // Try to get process information
        if let Some(proc_info) = sys.process(pid) {
            return VmMetrics {
                cpu_percent: Some(proc_info.cpu_usage()),
                memory_bytes: Some(proc_info.memory()),
            };
        }

        // Process not found or not running - return empty metrics
        VmMetrics::default()
    }

    fn is_running(&self) -> bool {
        // Check if process exists by sending signal 0
        unsafe { libc::kill(self.pid as i32, 0) == 0 }
    }

    fn exit_code(&self) -> Option<i32> {
        self.exit_code
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_vm_metrics_default() {
        let m = VmMetrics::default();
        assert!(m.cpu_percent.is_none());
        assert!(m.memory_bytes.is_none());
    }

    #[test]
    fn test_vm_metrics_clone() {
        let m = VmMetrics {
            cpu_percent: Some(50.0),
            memory_bytes: Some(1024 * 1024),
        };
        let cloned = m.clone();
        assert_eq!(cloned.cpu_percent, Some(50.0));
        assert_eq!(cloned.memory_bytes, Some(1024 * 1024));
    }

    #[test]
    fn test_shim_handler_from_pid() {
        let handler = ShimHandler::from_pid(12345, "box-abc".to_string());
        assert_eq!(handler.pid(), 12345);
        assert_eq!(handler.box_id(), "box-abc");
        assert_eq!(handler.exit_code(), None);
    }

    #[test]
    fn test_shim_handler_is_running_nonexistent_pid() {
        // PID 999999999 should not exist
        let handler = ShimHandler::from_pid(999_999_999, "test".to_string());
        assert!(!handler.is_running());
    }

    #[test]
    fn test_shim_handler_metrics_nonexistent_pid() {
        let handler = ShimHandler::from_pid(999_999_999, "test".to_string());
        let m = handler.metrics();
        // Non-existent process should return default metrics
        assert!(m.cpu_percent.is_none() || m.cpu_percent == Some(0.0));
    }

    #[test]
    fn test_shim_handler_is_running_current_process() {
        // Current process PID should be running
        let pid = std::process::id();
        let handler = ShimHandler::from_pid(pid, "self".to_string());
        assert!(handler.is_running());
    }

    #[test]
    fn test_default_shutdown_timeout() {
        assert_eq!(DEFAULT_SHUTDOWN_TIMEOUT_MS, 10_000);
    }

    #[test]
    fn test_vm_metrics_debug() {
        let m = VmMetrics {
            cpu_percent: Some(25.5),
            memory_bytes: Some(512),
        };
        let debug = format!("{:?}", m);
        assert!(debug.contains("25.5"));
        assert!(debug.contains("512"));
    }
}
