//! Container monitor daemon for managing long-running containers.
//!
//! This module provides background monitoring for detached containers,
//! implementing restart policies and lifecycle management.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use tokio::sync::RwLock;
use tokio::time::sleep;
use tracing::{debug, error, info, warn};

use crate::state::{BoxRecord, StateFile};

/// Container monitor daemon that watches detached containers and applies restart policies.
pub struct ContainerMonitor {
    /// Path to the state file
    state_path: PathBuf,
    /// Monitored containers (box_id -> MonitoredContainer)
    containers: Arc<RwLock<HashMap<String, MonitoredContainer>>>,
    /// Whether the monitor is running
    running: Arc<RwLock<bool>>,
}

/// A container being monitored by the daemon.
struct MonitoredContainer {
    /// Box record
    record: BoxRecord,
    /// Last known PID
    last_pid: Option<u32>,
    /// Number of restart attempts
    restart_attempts: u32,
    /// Last restart timestamp
    last_restart: Option<chrono::DateTime<Utc>>,
}

impl ContainerMonitor {
    /// Create a new container monitor.
    pub fn new(state_path: PathBuf) -> Self {
        Self {
            state_path,
            containers: Arc::new(RwLock::new(HashMap::new())),
            running: Arc::new(RwLock::new(false)),
        }
    }

    /// Start the monitor daemon.
    ///
    /// This spawns a background task that periodically checks container status
    /// and applies restart policies.
    pub async fn start(&self) -> Result<(), Box<dyn std::error::Error>> {
        let mut running = self.running.write().await;
        if *running {
            return Err("Monitor is already running".into());
        }
        *running = true;
        drop(running);

        info!("Starting container monitor daemon");

        // Load existing containers from state file
        self.load_containers().await?;

        // Spawn monitoring loop
        let containers = Arc::clone(&self.containers);
        let running = Arc::clone(&self.running);
        let state_path = self.state_path.clone();

        tokio::spawn(async move {
            while *running.read().await {
                if let Err(e) = Self::monitor_cycle(&containers, &state_path).await {
                    error!(error = %e, "Monitor cycle failed");
                }
                sleep(Duration::from_secs(5)).await;
            }
            info!("Container monitor daemon stopped");
        });

        Ok(())
    }

    /// Stop the monitor daemon.
    pub async fn stop(&self) {
        let mut running = self.running.write().await;
        *running = false;
        info!("Stopping container monitor daemon");
    }

    /// Load containers from state file.
    async fn load_containers(&self) -> Result<(), Box<dyn std::error::Error>> {
        let state = StateFile::load(&self.state_path)?;
        let mut containers = self.containers.write().await;

        for record in state.list(true) {
            // Only monitor containers with restart policies
            if should_monitor(&record) {
                debug!(
                    box_id = %record.id,
                    name = %record.name,
                    restart_policy = %record.restart_policy,
                    "Loading container for monitoring"
                );

                containers.insert(
                    record.id.clone(),
                    MonitoredContainer {
                        last_pid: record.pid,
                        restart_attempts: record.restart_count,
                        last_restart: record.started_at,
                        record: record.clone(),
                    },
                );
            }
        }

        info!(count = containers.len(), "Loaded containers for monitoring");
        Ok(())
    }

    /// Run one monitoring cycle.
    async fn monitor_cycle(
        containers: &Arc<RwLock<HashMap<String, MonitoredContainer>>>,
        state_path: &PathBuf,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let mut containers_guard = containers.write().await;
        let mut to_restart = Vec::new();

        // Check each monitored container
        for (box_id, monitored) in containers_guard.iter_mut() {
            if let Some(pid) = monitored.last_pid {
                // Check if process is still running
                if !is_process_running(pid) {
                    debug!(
                        box_id = %box_id,
                        name = %monitored.record.name,
                        pid = pid,
                        "Container process exited"
                    );

                    // Update state file
                    if let Ok(mut state) = StateFile::load(state_path) {
                        state.mark_stopped(box_id, None);
                        let _ = state.save();
                    }

                    // Check if should restart
                    if should_restart(&monitored.record, monitored.restart_attempts) {
                        to_restart.push(box_id.clone());
                    } else {
                        debug!(
                            box_id = %box_id,
                            name = %monitored.record.name,
                            restart_policy = %monitored.record.restart_policy,
                            restart_attempts = monitored.restart_attempts,
                            "Container will not be restarted"
                        );
                    }

                    monitored.last_pid = None;
                }
            }
        }

        drop(containers_guard);

        // Restart containers that need it
        for box_id in to_restart {
            if let Err(e) = restart_container(&box_id, containers, state_path).await {
                error!(box_id = %box_id, error = %e, "Failed to restart container");
            }
        }

        Ok(())
    }

    /// Add a container to monitoring.
    pub async fn add_container(&self, record: BoxRecord) {
        if should_monitor(&record) {
            let mut containers = self.containers.write().await;
            debug!(
                box_id = %record.id,
                name = %record.name,
                restart_policy = %record.restart_policy,
                "Adding container to monitoring"
            );

            containers.insert(
                record.id.clone(),
                MonitoredContainer {
                    last_pid: record.pid,
                    restart_attempts: record.restart_count,
                    last_restart: record.started_at,
                    record,
                },
            );
        }
    }

    /// Remove a container from monitoring.
    pub async fn remove_container(&self, box_id: &str) {
        let mut containers = self.containers.write().await;
        if containers.remove(box_id).is_some() {
            debug!(box_id = %box_id, "Removed container from monitoring");
        }
    }

    /// Update container state after restart.
    pub async fn update_container(&self, box_id: &str, pid: u32) {
        let mut containers = self.containers.write().await;
        if let Some(monitored) = containers.get_mut(box_id) {
            monitored.last_pid = Some(pid);
            monitored.restart_attempts += 1;
            monitored.last_restart = Some(Utc::now());
            debug!(
                box_id = %box_id,
                pid = pid,
                restart_attempts = monitored.restart_attempts,
                "Updated container state after restart"
            );
        }
    }
}

/// Check if a container should be monitored.
fn should_monitor(record: &BoxRecord) -> bool {
    // Monitor containers with restart policies other than "no"
    record.restart_policy != "no" && record.status != "dead"
}

/// Check if a container should be restarted based on its policy.
fn should_restart(record: &BoxRecord, restart_attempts: u32) -> bool {
    match record.restart_policy.as_str() {
        "no" => false,
        "always" => true,
        "unless-stopped" => !record.stopped_by_user,
        policy if policy.starts_with("on-failure") => {
            // Only restart on non-zero exit code
            if let Some(exit_code) = record.exit_code {
                if exit_code == 0 {
                    return false;
                }
            }

            // Check max restart count
            if record.max_restart_count > 0 {
                restart_attempts < record.max_restart_count
            } else {
                true
            }
        }
        _ => {
            warn!(
                restart_policy = %record.restart_policy,
                "Unknown restart policy, treating as 'no'"
            );
            false
        }
    }
}

/// Check if a process is running.
fn is_process_running(pid: u32) -> bool {
    #[cfg(unix)]
    {
        // Use kill with signal 0 to check if process exists
        unsafe {
            libc::kill(pid as i32, 0) == 0
        }
    }

    #[cfg(windows)]
    {
        // On Windows, try to open the process handle
        use std::ptr;
        use windows::Win32::Foundation::{CloseHandle, HANDLE};
        use windows::Win32::System::Threading::{OpenProcess, PROCESS_QUERY_INFORMATION};

        unsafe {
            let handle = OpenProcess(PROCESS_QUERY_INFORMATION, false, pid);
            if handle.is_invalid() {
                return false;
            }
            let _ = CloseHandle(handle);
            true
        }
    }

    #[cfg(not(any(unix, windows)))]
    {
        // Fallback: assume process is running
        true
    }
}

/// Restart a container.
async fn restart_container(
    box_id: &str,
    containers: &Arc<RwLock<HashMap<String, MonitoredContainer>>>,
    state_path: &PathBuf,
) -> Result<(), Box<dyn std::error::Error>> {
    let containers_guard = containers.read().await;
    let monitored = containers_guard
        .get(box_id)
        .ok_or_else(|| format!("Container {} not found in monitor", box_id))?;

    info!(
        box_id = %box_id,
        name = %monitored.record.name,
        restart_attempts = monitored.restart_attempts,
        "Restarting container"
    );

    // TODO: Implement actual restart logic
    // This will be integrated with the VmManager to restart the container
    // For now, we just log the restart attempt

    // Update state file
    let mut state = StateFile::load(state_path)?;
    state.increment_restart_count(box_id);
    state.save()?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_should_monitor() {
        let mut record = BoxRecord {
            id: "test".to_string(),
            short_id: "test".to_string(),
            name: "test".to_string(),
            image: "alpine".to_string(),
            status: "running".to_string(),
            pid: Some(1234),
            cpus: 1,
            memory_mb: 512,
            volumes: vec![],
            env: Default::default(),
            cmd: vec![],
            entrypoint: None,
            box_dir: PathBuf::new(),
            exec_socket_path: PathBuf::new(),
            console_log: PathBuf::new(),
            created_at: Utc::now(),
            started_at: Some(Utc::now()),
            auto_remove: false,
            hostname: None,
            user: None,
            workdir: None,
            restart_policy: "no".to_string(),
            port_map: vec![],
            labels: Default::default(),
            stopped_by_user: false,
            restart_count: 0,
            max_restart_count: 0,
            exit_code: None,
            health_check: None,
            health_status: "none".to_string(),
            health_retries: 0,
            health_last_check: None,
            network_mode: a3s_box_core::NetworkMode::Tsi,
            network_name: None,
            volume_names: vec![],
            tmpfs: vec![],
            anonymous_volumes: vec![],
            resource_limits: Default::default(),
            log_config: Default::default(),
            add_host: vec![],
            platform: None,
            init: false,
            read_only: false,
            cap_add: vec![],
            cap_drop: vec![],
            security_opt: vec![],
            privileged: false,
            devices: vec![],
            gpus: None,
            shm_size: None,
            stop_signal: None,
            stop_timeout: None,
            oom_kill_disable: false,
            oom_score_adj: None,
        };

        // Should not monitor with "no" policy
        assert!(!should_monitor(&record));

        // Should monitor with "always" policy
        record.restart_policy = "always".to_string();
        assert!(should_monitor(&record));

        // Should not monitor dead containers
        record.status = "dead".to_string();
        assert!(!should_monitor(&record));
    }

    #[test]
    fn test_should_restart_always() {
        let record = BoxRecord {
            restart_policy: "always".to_string(),
            stopped_by_user: false,
            exit_code: Some(0),
            max_restart_count: 0,
            ..Default::default()
        };

        assert!(should_restart(&record, 0));
        assert!(should_restart(&record, 10));
    }

    #[test]
    fn test_should_restart_unless_stopped() {
        let mut record = BoxRecord {
            restart_policy: "unless-stopped".to_string(),
            stopped_by_user: false,
            exit_code: Some(0),
            max_restart_count: 0,
            ..Default::default()
        };

        assert!(should_restart(&record, 0));

        record.stopped_by_user = true;
        assert!(!should_restart(&record, 0));
    }

    #[test]
    fn test_should_restart_on_failure() {
        let mut record = BoxRecord {
            restart_policy: "on-failure".to_string(),
            stopped_by_user: false,
            exit_code: Some(1),
            max_restart_count: 0,
            ..Default::default()
        };

        // Should restart on non-zero exit code
        assert!(should_restart(&record, 0));

        // Should not restart on zero exit code
        record.exit_code = Some(0);
        assert!(!should_restart(&record, 0));

        // Should respect max restart count
        record.exit_code = Some(1);
        record.max_restart_count = 3;
        assert!(should_restart(&record, 0));
        assert!(should_restart(&record, 2));
        assert!(!should_restart(&record, 3));
    }
}
