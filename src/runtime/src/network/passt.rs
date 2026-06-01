//! Passt process management for virtio-net networking.
//!
//! Manages the lifecycle of `passt` daemon instances that provide
//! the virtio-net backend for bridge-mode networking. Each box gets
//! its own passt process with a dedicated Unix socket.

use a3s_box_core::error::{BoxError, Result};
use std::net::Ipv4Addr;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};

/// Manages a passt daemon instance for a single box.
#[derive(Debug)]
pub struct PasstManager {
    /// Path to the passt Unix socket.
    socket_path: PathBuf,
    /// Child process handle (None if not started).
    child: Option<Child>,
    /// PID file path for the passt process.
    pid_file: PathBuf,
}

impl PasstManager {
    /// Create a new PasstManager.
    ///
    /// The socket and PID file are placed directly in the provided runtime
    /// socket directory (the same directory that holds the exec/PTY control
    /// sockets, e.g. `/tmp/a3s-box-sockets/<box_id>`).
    ///
    /// This directory MUST be reachable by the user passt runs as. When passt
    /// is started as root it drops privileges to `nobody`, so the socket
    /// directory has to be world-traversable — the box's `~/.a3s/boxes/<id>`
    /// home is mode 0700 for root and would leave passt unable to bind its
    /// socket, silently breaking all bridge networking and Compose.
    pub fn new(socket_dir: &Path) -> Self {
        Self {
            socket_path: socket_dir.join("passt.sock"),
            pid_file: socket_dir.join("passt.pid"),
            child: None,
        }
    }

    /// Get the passt socket path.
    pub fn socket_path(&self) -> &Path {
        &self.socket_path
    }

    /// Spawn the passt daemon.
    ///
    /// Configures passt with:
    /// - Unix socket mode (no PID namespace)
    /// - The assigned IP, gateway, prefix length
    /// - DNS forwarding
    /// - No DHCP (static IP assignment)
    /// - Inbound TCP port forwarding for any published ports (`port_map`)
    pub fn spawn(
        &mut self,
        ip: Ipv4Addr,
        gateway: Ipv4Addr,
        prefix_len: u8,
        dns_servers: &[Ipv4Addr],
        port_map: &[String],
    ) -> Result<()> {
        // Ensure parent directory exists.
        if let Some(parent) = self.socket_path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| {
                BoxError::NetworkError(format!(
                    "failed to create socket directory {}: {}",
                    parent.display(),
                    e
                ))
            })?;

            // passt drops privileges to `nobody` when launched as root, so the
            // directory it binds its socket (and writes its PID file) in must be
            // writable by that user. Widen the directory permissions; the path
            // is an ephemeral, per-box runtime directory under a world-traversable
            // base, so this only affects this box's control sockets.
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                if let Err(e) =
                    std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o777))
                {
                    tracing::warn!(
                        dir = %parent.display(),
                        error = %e,
                        "Failed to widen passt socket directory permissions; \
                         passt may be unable to bind its socket after dropping privileges"
                    );
                }
            }
        }

        // Remove stale socket if it exists
        if self.socket_path.exists() {
            std::fs::remove_file(&self.socket_path).ok();
        }

        let mut cmd = Command::new("passt");
        cmd.arg("--socket")
            .arg(&self.socket_path)
            .arg("--pid")
            .arg(&self.pid_file)
            // Run in foreground (we manage the process)
            .arg("--foreground")
            // Configure the network
            .arg("--address")
            .arg(ip.to_string())
            .arg("--gateway")
            .arg(gateway.to_string())
            .arg("--netmask")
            .arg(format!("{}", prefix_to_netmask(prefix_len)));

        // Add DNS servers
        for dns in dns_servers {
            cmd.arg("--dns").arg(dns.to_string());
        }

        // Forward published TCP ports into the guest. libkrun discards the
        // TSI host_port_map once a virtio-net device is attached, so passt is
        // what actually publishes `-p host:guest` in bridge mode. Auto-assigned
        // host ports (host_port == 0) cannot be forwarded by passt and are
        // skipped. passt accepts a comma-separated `host:guest,...` spec.
        let tcp_specs: Vec<String> = port_map
            .iter()
            .filter_map(|m| a3s_box_core::parse_port_mapping(m).ok())
            .filter(|m| m.host_port != 0)
            .map(|m| format!("{}:{}", m.host_port, m.guest_port))
            .collect();
        if !tcp_specs.is_empty() {
            let spec = tcp_specs.join(",");
            tracing::info!(tcp_ports = %spec, "Configuring passt inbound TCP port forwarding");
            cmd.arg("--tcp-ports").arg(spec);
        }

        // Capture passt's stderr to a log file so spawn failures (bad args,
        // unsupported flags, permission errors after dropping privileges) are
        // diagnosable instead of silently discarded to /dev/null.
        cmd.stdout(std::process::Stdio::null());
        match self
            .socket_path
            .parent()
            .map(|p| p.join("passt.stderr.log"))
            .and_then(|p| std::fs::File::create(p).ok())
        {
            Some(file) => {
                cmd.stderr(std::process::Stdio::from(file));
            }
            None => {
                cmd.stderr(std::process::Stdio::null());
            }
        }

        let child = cmd.spawn().map_err(|e| {
            BoxError::NetworkError(format!(
                "failed to spawn passt: {} (is passt installed?)",
                e
            ))
        })?;

        tracing::info!(
            pid = child.id(),
            socket = %self.socket_path.display(),
            ip = %ip,
            gateway = %gateway,
            "Passt daemon started"
        );

        self.child = Some(child);

        // Wait briefly for the socket to appear
        self.wait_for_socket()?;

        Ok(())
    }

    /// Wait for the passt socket to become available.
    ///
    /// Also detects immediate passt exit (e.g. bad args or a permission failure
    /// after dropping privileges) so the real cause is surfaced instead of a
    /// misleading 5-second timeout.
    fn wait_for_socket(&mut self) -> Result<()> {
        let stderr_path = self
            .socket_path
            .parent()
            .map(|p| p.join("passt.stderr.log"));
        let read_stderr = |path: &Option<PathBuf>| -> String {
            path.as_ref()
                .and_then(|p| std::fs::read_to_string(p).ok())
                .map(|s| {
                    let mut tail: Vec<&str> = s.lines().rev().take(4).collect();
                    tail.reverse();
                    tail.join("; ")
                })
                .filter(|s| !s.trim().is_empty())
                .map(|s| format!(" (passt stderr: {s})"))
                .unwrap_or_default()
        };

        let max_attempts = 50; // 5 seconds total
        for _ in 0..max_attempts {
            if self.socket_path.exists() {
                return Ok(());
            }
            if let Some(child) = self.child.as_mut() {
                if let Ok(Some(status)) = child.try_wait() {
                    return Err(BoxError::NetworkError(format!(
                        "passt exited early with {status} before creating its socket{}",
                        read_stderr(&stderr_path)
                    )));
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }

        Err(BoxError::NetworkError(format!(
            "passt socket {} did not appear within 5 seconds{}",
            self.socket_path.display(),
            read_stderr(&stderr_path)
        )))
    }

    /// Stop the passt daemon.
    pub fn stop(&mut self) {
        if let Some(ref mut child) = self.child {
            let pid = child.id();
            if let Err(e) = child.kill() {
                tracing::warn!(pid, error = %e, "Failed to kill passt process");
            } else {
                // Reap the child to avoid zombies
                let _ = child.wait();
                tracing::info!(pid, "Passt daemon stopped");
            }
        }
        self.child = None;

        // Clean up socket and PID file
        std::fs::remove_file(&self.socket_path).ok();
        std::fs::remove_file(&self.pid_file).ok();
    }

    /// Check if the passt process is still running.
    pub fn is_running(&mut self) -> bool {
        match self.child {
            Some(ref mut child) => child.try_wait().ok().flatten().is_none(),
            None => false,
        }
    }
}

impl Drop for PasstManager {
    fn drop(&mut self) {
        // Intentionally does NOT kill passt. passt must outlive the process that
        // spawned it: a detached `run -d` returns while the box keeps running,
        // and the VM (driven by the shim) outlives the CLI. Killing on drop here
        // is exactly what previously left detached bridge boxes with dead
        // networking. passt is reaped on box stop/rm via `terminate_passt`, which
        // uses the PID file as the source of truth.
    }
}

/// Terminate a passt daemon by its PID file and remove its socket/PID files.
///
/// passt outlives the `PasstManager` that launched it (so detached boxes keep
/// working after the CLI exits), so box teardown cannot rely on a live handle —
/// the PID file written into the box's runtime socket directory is authoritative.
pub fn terminate_passt(socket_dir: &Path) {
    let pid_file = socket_dir.join("passt.pid");
    if let Ok(contents) = std::fs::read_to_string(&pid_file) {
        if let Ok(pid) = contents.trim().parse::<i32>() {
            if pid > 1 {
                // SIGTERM; passt exits and is reaped by its (re)parent.
                #[cfg(unix)]
                unsafe {
                    libc::kill(pid, libc::SIGTERM);
                }
                tracing::info!(pid, "Terminated passt daemon");
            }
        }
    }
    let _ = std::fs::remove_file(&pid_file);
    let _ = std::fs::remove_file(socket_dir.join("passt.sock"));
}

impl super::NetworkBackend for PasstManager {
    fn socket_path(&self) -> &std::path::Path {
        self.socket_path()
    }

    fn stop(&mut self) {
        self.stop();
    }
}

/// Convert a prefix length to a dotted-decimal netmask string.
fn prefix_to_netmask(prefix: u8) -> Ipv4Addr {
    if prefix == 0 {
        return Ipv4Addr::new(0, 0, 0, 0);
    }
    let mask = !((1u32 << (32 - prefix)) - 1);
    Ipv4Addr::from(mask)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_prefix_to_netmask() {
        assert_eq!(prefix_to_netmask(24), Ipv4Addr::new(255, 255, 255, 0));
        assert_eq!(prefix_to_netmask(16), Ipv4Addr::new(255, 255, 0, 0));
        assert_eq!(prefix_to_netmask(8), Ipv4Addr::new(255, 0, 0, 0));
        assert_eq!(prefix_to_netmask(32), Ipv4Addr::new(255, 255, 255, 255));
        assert_eq!(prefix_to_netmask(0), Ipv4Addr::new(0, 0, 0, 0));
        assert_eq!(prefix_to_netmask(28), Ipv4Addr::new(255, 255, 255, 240));
    }

    #[test]
    fn test_passt_manager_new() {
        let dir = tempfile::tempdir().unwrap();
        let mgr = PasstManager::new(dir.path());
        assert_eq!(mgr.socket_path(), dir.path().join("passt.sock"));
    }

    #[test]
    fn test_passt_manager_not_running_initially() {
        let dir = tempfile::tempdir().unwrap();
        let mut mgr = PasstManager::new(dir.path());
        assert!(!mgr.is_running());
    }

    #[test]
    fn test_passt_manager_stop_when_not_started() {
        let dir = tempfile::tempdir().unwrap();
        let mut mgr = PasstManager::new(dir.path());
        // Should not panic
        mgr.stop();
        assert!(!mgr.is_running());
    }

    #[test]
    fn test_passt_manager_socket_path() {
        let dir = tempfile::tempdir().unwrap();
        let box_dir = dir.path().join("boxes").join("test-box-id");
        let mgr = PasstManager::new(&box_dir);
        assert_eq!(mgr.socket_path(), box_dir.join("passt.sock"));
    }

    #[test]
    fn test_terminate_passt_removes_socket_and_pid_files() {
        let dir = tempfile::tempdir().unwrap();
        let socket_path = dir.path().join("passt.sock");
        let pid_path = dir.path().join("passt.pid");

        // A non-existent PID so the SIGTERM is a harmless no-op (ESRCH).
        std::fs::write(&socket_path, "fake").unwrap();
        std::fs::write(&pid_path, "2147483647").unwrap();

        terminate_passt(dir.path());

        assert!(!socket_path.exists());
        assert!(!pid_path.exists());
    }
}
