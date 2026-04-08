//! VM Manager - Lifecycle management for MicroVM instances.

mod layout;
mod network;
mod ready;
mod spec;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use a3s_box_core::config::BoxConfig;
#[cfg(unix)]
use a3s_box_core::config::TeeConfig;
use a3s_box_core::error::{BoxError, Result};
use a3s_box_core::event::{BoxEvent, EventEmitter};
use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;
use tracing::Instrument;

use libc;

#[cfg(unix)]
use crate::grpc::ExecClient;
#[cfg(unix)]
use crate::tee::TeeExtension;
use crate::vmm::{VmController, VmHandler, VmmProvider, DEFAULT_SHUTDOWN_TIMEOUT_MS};

/// Box state machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum BoxState {
    /// Config captured, no VM started
    Created,

    /// VM booted, container initialized, gRPC healthy
    Ready,

    /// A session is actively processing a prompt
    Busy,

    /// A session is compressing its context
    Compacting,

    /// VM terminated, resources freed
    Stopped,
}

/// Layout of directories for a box instance.
pub(crate) struct BoxLayout {
    /// Path to the root filesystem
    pub(crate) rootfs_path: PathBuf,
    /// Path to the exec Unix socket
    pub(crate) exec_socket_path: PathBuf,
    /// Path to the PTY Unix socket
    pub(crate) pty_socket_path: PathBuf,
    /// Path to the attestation Unix socket
    pub(crate) attest_socket_path: PathBuf,
    /// Path to the workspace directory
    pub(crate) workspace_path: PathBuf,
    /// Path to console output file (optional)
    pub(crate) console_output: Option<PathBuf>,
    /// OCI image config (entrypoint, env, working dir, volumes)
    pub(crate) oci_config: Option<crate::oci::OciImageConfig>,
    /// TEE instance configuration (if TEE is enabled)
    pub(crate) tee_instance_config: Option<crate::vmm::TeeInstanceConfig>,
}

/// VM manager - orchestrates VM lifecycle.
pub struct VmManager {
    /// Box configuration
    pub(crate) config: BoxConfig,

    /// Unique box identifier
    pub(crate) box_id: String,

    /// Current state
    pub(crate) state: Arc<RwLock<BoxState>>,

    /// Event emitter
    pub(crate) event_emitter: EventEmitter,

    /// VMM provider (spawns VMs via pluggable backend)
    pub(crate) provider: Option<Box<dyn VmmProvider>>,

    /// VM handler (runtime operations on running VM)
    pub(crate) handler: Arc<RwLock<Option<Box<dyn VmHandler>>>>,

    /// Exec client for executing commands in the guest
    #[cfg(unix)]
    pub(crate) exec_client: Option<ExecClient>,

    /// Network backend manager for bridge networking (None if TSI mode).
    /// Platform-specific: passt on Linux, gvproxy on macOS.
    pub(crate) net_manager: Option<Box<dyn crate::network::NetworkBackend>>,

    /// A3S home directory (~/.a3s)
    pub(crate) home_dir: PathBuf,

    /// Anonymous volume names created during boot (from OCI VOLUME directives)
    pub(crate) anonymous_volumes: Vec<String>,

    /// TEE extension (attestation, sealing, secret injection)
    #[cfg(unix)]
    pub(crate) tee: Option<Box<dyn TeeExtension>>,

    /// Rootfs provider (overlay or copy)
    pub(crate) rootfs_provider: Box<dyn crate::rootfs::RootfsProvider>,

    /// Path to the exec Unix socket (set after boot)
    pub(crate) exec_socket_path: Option<PathBuf>,

    /// Path to the PTY Unix socket (set after boot)
    pub(crate) pty_socket_path: Option<PathBuf>,

    /// Prometheus metrics (optional, for instrumented deployments).
    pub(crate) prom: Option<crate::prom::RuntimeMetrics>,

    /// Exit code captured from the shim process after it exits.
    pub(crate) shim_exit_code: Option<i32>,

    /// Optional progress callback for image pulls: `(current, total, digest, size_bytes)`.
    pub(crate) pull_progress_fn: Option<Arc<dyn Fn(usize, usize, &str, i64) + Send + Sync>>,
}

impl VmManager {
    /// Create a new VM manager.
    pub fn new(config: BoxConfig, event_emitter: EventEmitter) -> Self {
        let box_id = uuid::Uuid::new_v4().to_string();
        let home_dir = a3s_box_core::dirs_home();

        Self {
            config,
            box_id,
            state: Arc::new(RwLock::new(BoxState::Created)),
            event_emitter,
            provider: None,
            handler: Arc::new(RwLock::new(None)),
            #[cfg(unix)]
            exec_client: None,
            net_manager: None,
            home_dir,
            anonymous_volumes: Vec::new(),
            #[cfg(unix)]
            tee: None,
            rootfs_provider: crate::rootfs::default_provider(),
            exec_socket_path: None,
            pty_socket_path: None,
            prom: None,
            shim_exit_code: None,
            pull_progress_fn: None,
        }
    }

    /// Create a new VM manager with a specific box ID.
    pub fn with_box_id(config: BoxConfig, event_emitter: EventEmitter, box_id: String) -> Self {
        let home_dir = a3s_box_core::dirs_home();

        Self {
            config,
            box_id,
            state: Arc::new(RwLock::new(BoxState::Created)),
            event_emitter,
            provider: None,
            handler: Arc::new(RwLock::new(None)),
            #[cfg(unix)]
            exec_client: None,
            net_manager: None,
            home_dir,
            anonymous_volumes: Vec::new(),
            #[cfg(unix)]
            tee: None,
            rootfs_provider: crate::rootfs::default_provider(),
            exec_socket_path: None,
            pty_socket_path: None,
            prom: None,
            shim_exit_code: None,
            pull_progress_fn: None,
        }
    }

    /// Remove the box directory on the host.
    ///
    /// Called when boot fails after `prepare_layout` succeeded to avoid leaving
    /// orphaned directories on disk.
    fn cleanup_box_dir(&self) {
        let box_dir = self.home_dir.join("boxes").join(&self.box_id);
        if let Err(e) = std::fs::remove_dir_all(&box_dir) {
            tracing::warn!(
                box_id = %self.box_id,
                path = %box_dir.display(),
                error = %e,
                "Failed to cleanup box directory after boot failure"
            );
        }
    }

    /// Create a new VM manager with a custom VMM provider.
    pub fn with_provider(
        config: BoxConfig,
        event_emitter: EventEmitter,
        provider: Box<dyn VmmProvider>,
    ) -> Self {
        let box_id = uuid::Uuid::new_v4().to_string();
        let home_dir = a3s_box_core::dirs_home();
        Self {
            config,
            box_id,
            state: Arc::new(RwLock::new(BoxState::Created)),
            event_emitter,
            provider: Some(provider),
            handler: Arc::new(RwLock::new(None)),
            #[cfg(unix)]
            exec_client: None,
            net_manager: None,
            home_dir,
            anonymous_volumes: Vec::new(),
            #[cfg(unix)]
            tee: None,
            rootfs_provider: crate::rootfs::default_provider(),
            exec_socket_path: None,
            pty_socket_path: None,
            prom: None,
            shim_exit_code: None,
            pull_progress_fn: None,
        }
    }

    /// Get the box ID.
    pub fn box_id(&self) -> &str {
        &self.box_id
    }

    /// Get current state.
    pub async fn state(&self) -> BoxState {
        *self.state.read().await
    }

    /// Get the exec client, if connected.
    #[cfg(unix)]
    pub fn exec_client(&self) -> Option<&ExecClient> {
        self.exec_client.as_ref()
    }

    /// Get the exec socket path, if the VM has been booted.
    pub fn exec_socket_path(&self) -> Option<&Path> {
        self.exec_socket_path.as_deref()
    }

    /// Get the PTY socket path, if the VM has been booted.
    pub fn pty_socket_path(&self) -> Option<&Path> {
        self.pty_socket_path.as_deref()
    }

    /// Inject a custom VMM provider (e.g., a VmController with a known shim path).
    ///
    /// If set before `boot()`, the injected provider is used instead of the
    /// default `VmController::find_shim()` fallback.
    pub fn set_provider(&mut self, provider: Box<dyn VmmProvider>) {
        self.provider = Some(provider);
    }

    /// Override the rootfs provider (overlay or copy).
    ///
    /// By default, `default_provider()` auto-detects the best available provider.
    /// Call this before `boot()` to force a specific provider.
    pub fn set_rootfs_provider(&mut self, provider: Box<dyn crate::rootfs::RootfsProvider>) {
        self.rootfs_provider = provider;
    }

    /// Get the name of the active rootfs provider.
    pub fn rootfs_provider_name(&self) -> &str {
        self.rootfs_provider.name()
    }

    /// Set a progress callback for image pulls: `(current, total, digest, size_bytes)`.
    /// Called once per layer when `run` pulls an image that is not yet cached.
    pub fn set_pull_progress_fn(&mut self, f: Arc<dyn Fn(usize, usize, &str, i64) + Send + Sync>) {
        self.pull_progress_fn = Some(f);
    }

    /// Attach Prometheus metrics to this VM manager.
    pub fn set_metrics(&mut self, metrics: crate::prom::RuntimeMetrics) {
        self.prom = Some(metrics);
    }

    /// Get the attached Prometheus metrics (if any).
    pub fn metrics_prom(&self) -> Option<&crate::prom::RuntimeMetrics> {
        self.prom.as_ref()
    }

    /// Get the names of anonymous volumes created during boot.
    ///
    /// These are auto-created from OCI VOLUME directives and should be tracked
    /// for cleanup when the box is removed.
    pub fn anonymous_volumes(&self) -> &[String] {
        &self.anonymous_volumes
    }

    /// Get the exit code of the container, if it has exited.
    ///
    /// Returns `Some(code)` after `destroy()` has been called and the shim
    /// process exited naturally (not killed). Returns `None` if the VM has not
    /// yet stopped or the exit code could not be determined.
    pub fn exit_code(&self) -> Option<i32> {
        self.shim_exit_code
    }

    /// Execute a command in the guest VM.
    ///
    /// Requires the VM to be in Ready, Busy, or Compacting state.
    #[cfg(unix)]
    #[tracing::instrument(skip(self, cmd), fields(box_id = %self.box_id))]
    pub async fn exec_command(
        &self,
        cmd: Vec<String>,
        timeout_ns: u64,
    ) -> Result<a3s_box_core::exec::ExecOutput> {
        let state = self.state.read().await;
        match *state {
            BoxState::Ready | BoxState::Busy | BoxState::Compacting => {}
            BoxState::Created => {
                return Err(BoxError::ExecError("VM not yet booted".to_string()));
            }
            BoxState::Stopped => {
                return Err(BoxError::ExecError("VM is stopped".to_string()));
            }
        }
        drop(state);

        let client = self
            .exec_client
            .as_ref()
            .ok_or_else(|| BoxError::ExecError("Exec client not connected".to_string()))?;

        let request = a3s_box_core::exec::ExecRequest {
            cmd,
            timeout_ns,
            env: vec![],
            working_dir: None,
            stdin: None,
            user: None,
            streaming: false,
        };

        let exec_start = std::time::Instant::now();
        let result = client.exec_command(&request).await;

        // Record Prometheus metrics
        if let Some(ref prom) = self.prom {
            prom.exec_total.inc();
            prom.exec_duration
                .observe(exec_start.elapsed().as_secs_f64());
            if result.is_err() || result.as_ref().is_ok_and(|o| o.exit_code != 0) {
                prom.exec_errors_total.inc();
            }
        }

        result
    }

    /// Boot the VM.
    pub async fn boot(&mut self) -> Result<()> {
        let boot_span = tracing::info_span!("vm_boot", box_id = %self.box_id);
        // Check and transition state: Created → booting
        {
            let state = self.state.read().await;
            if *state != BoxState::Created {
                return Err(BoxError::StateError("VM already booted".to_string()));
            }
        }

        let boot_start = std::time::Instant::now();

        tracing::info!(parent: &boot_span, box_id = %self.box_id, "Booting VM");

        // 1. Prepare filesystem layout
        let layout = self
            .prepare_layout()
            .instrument(tracing::info_span!(parent: &boot_span, "prepare_layout"))
            .await?;

        // 1.5. Override /etc/resolv.conf with configured DNS
        let resolv_content = a3s_box_core::dns::generate_resolv_conf(&self.config.dns);
        let resolv_path = layout.rootfs_path.join("etc/resolv.conf");
        if let Err(e) = tokio::fs::write(&resolv_path, &resolv_content).await {
            self.cleanup_box_dir();
            return Err(BoxError::IoError(e));
        }
        tracing::debug!(parent: &boot_span, dns = %resolv_content.trim(), "Configured guest DNS");

        // 2. Build InstanceSpec
        let mut spec = match self.build_instance_spec(&layout) {
            Ok(s) => s,
            Err(e) => {
                self.cleanup_box_dir();
                return Err(e);
            }
        };

        // 2.5. Configure bridge networking if requested
        let bridge_network = match &self.config.network {
            a3s_box_core::NetworkMode::Bridge { network } => Some(network.clone()),
            _ => None,
        };
        if let Some(network_name) = bridge_network {
            let net_config = match self.setup_bridge_network(&network_name) {
                Ok(n) => n,
                Err(e) => {
                    self.cleanup_box_dir();
                    return Err(e);
                }
            };

            // Write /etc/hosts for DNS service discovery
            match self.write_hosts_file(&layout, &network_name) {
                Ok(()) => (),
                Err(e) => {
                    self.cleanup_box_dir();
                    return Err(e);
                }
            };

            // Inject network env vars into entrypoint so they are passed via
            // krun_set_exec's envp (not krun_set_env which overwrites all vars).
            let ip_cidr = format!("{}/{}", net_config.ip_address, net_config.prefix_len);
            spec.entrypoint
                .env
                .push(("A3S_NET_IP".to_string(), ip_cidr));
            spec.entrypoint.env.push((
                "A3S_NET_GATEWAY".to_string(),
                net_config.gateway.to_string(),
            ));
            spec.entrypoint.env.push((
                "A3S_NET_DNS".to_string(),
                net_config
                    .dns_servers
                    .iter()
                    .map(|s| s.to_string())
                    .collect::<Vec<_>>()
                    .join(","),
            ));

            spec.network = Some(net_config);
        }

        // 3. Initialize VMM provider (use injected provider or default to VmController)
        if self.provider.is_none() {
            let shim_path = match VmController::find_shim() {
                Ok(p) => p,
                Err(e) => {
                    self.cleanup_box_dir();
                    return Err(e);
                }
            };
            let controller = match VmController::new(shim_path) {
                Ok(c) => c,
                Err(e) => {
                    self.cleanup_box_dir();
                    return Err(e);
                }
            };
            self.provider = Some(Box::new(controller));
        }

        // 4. Start VM via provider
        let handler = {
            let provider = self
                .provider
                .as_ref()
                .ok_or_else(|| BoxError::BoxBootError {
                    message: "VMM provider not initialized".to_string(),
                    hint: Some("Ensure VmManager has a provider set before boot".to_string()),
                })?;
            let vm_start_span = tracing::info_span!(parent: &boot_span, "vm_start");
            match async { provider.start(&spec).await }
                .instrument(vm_start_span)
                .await
            {
                Ok(h) => h,
                Err(e) => {
                    self.cleanup_box_dir();
                    return Err(e);
                }
            }
        };

        // Store handler
        *self.handler.write().await = Some(handler);

        // 5. Wait for guest ready
        {
            let wait_span = tracing::info_span!(parent: &boot_span, "wait_for_ready");
            if let Err(e) = async {
                self.wait_for_vm_running().await?;

                // 5b. Wait for exec server to become ready (Heartbeat health check)
                #[cfg(unix)]
                self.wait_for_exec_ready(&layout.exec_socket_path).await?;
                Ok::<(), BoxError>(())
            }
            .instrument(wait_span)
            .await
            {
                // Cleanup box_dir but leave handler for caller to destroy via destroy()
                self.cleanup_box_dir();
                return Err(e);
            }
        }

        // 5b2. Store socket paths for CRI streaming access
        self.exec_socket_path = Some(layout.exec_socket_path.clone());
        self.pty_socket_path = Some(layout.pty_socket_path.clone());

        // 5c. Initialize TEE extension for TEE environments
        #[cfg(unix)]
        if !matches!(self.config.tee, TeeConfig::None) {
            self.tee = Some(Box::new(crate::tee::SnpTeeExtension::new(
                self.box_id.clone(),
                layout.attest_socket_path.clone(),
            )));
        }

        // 6. Update state to Ready
        *self.state.write().await = BoxState::Ready;

        // Record Prometheus metrics
        if let Some(ref prom) = self.prom {
            let boot_duration = boot_start.elapsed().as_secs_f64();
            prom.vm_boot_duration.observe(boot_duration);
            prom.vm_created_total.inc();
            prom.vm_count.with_label_values(&["ready"]).inc();
        }

        // Emit ready event
        self.event_emitter.emit(BoxEvent::empty("box.ready"));

        tracing::info!(parent: &boot_span, box_id = %self.box_id, "VM ready");

        Ok(())
    }

    /// Destroy the VM with the default shutdown timeout and SIGTERM.
    pub async fn destroy(&mut self) -> Result<()> {
        self.destroy_with_options(libc::SIGTERM, DEFAULT_SHUTDOWN_TIMEOUT_MS)
            .await
    }

    /// Destroy the VM with a custom shutdown timeout and SIGTERM.
    pub async fn destroy_with_timeout(&mut self, timeout_ms: u64) -> Result<()> {
        self.destroy_with_options(libc::SIGTERM, timeout_ms).await
    }

    /// Destroy the VM with a specific stop signal and timeout.
    ///
    /// Sends `signal` to the shim process and waits up to `timeout_ms` for it
    /// to exit gracefully before sending SIGKILL.
    #[tracing::instrument(skip(self), fields(box_id = %self.box_id))]
    pub async fn destroy_with_options(&mut self, signal: i32, timeout_ms: u64) -> Result<()> {
        let mut state = self.state.write().await;

        if *state == BoxState::Stopped {
            return Ok(());
        }

        tracing::info!(box_id = %self.box_id, signal, timeout_ms, "Destroying VM");

        // Mark as stopped first — ensures state is correct even if handler.stop() fails.
        *state = BoxState::Stopped;

        // Stop the VM handler and capture its exit code before it's dropped.
        if let Some(mut handler) = self.handler.write().await.take() {
            if let Err(e) = handler.stop(signal, timeout_ms) {
                tracing::error!(box_id = %self.box_id, error = %e, "Failed to stop VM handler");
                self.shim_exit_code = handler.exit_code();
                return Err(e.into());
            }
            self.shim_exit_code = handler.exit_code();
        }

        // Stop network backend if running
        if let Some(ref mut net) = self.net_manager {
            net.stop();
        }
        self.net_manager = None;

        // Cleanup rootfs provider (unmount overlay if applicable)
        let box_dir = self.home_dir.join("boxes").join(&self.box_id);
        if let Err(e) = self
            .rootfs_provider
            .cleanup(&box_dir, self.config.persistent)
        {
            tracing::warn!(
                box_id = %self.box_id,
                error = %e,
                "Failed to cleanup rootfs provider"
            );
        }

        // Record Prometheus metrics
        if let Some(ref prom) = self.prom {
            prom.vm_destroyed_total.inc();
            prom.vm_count.with_label_values(&["ready"]).dec();
        }

        // Emit stopped event
        self.event_emitter.emit(BoxEvent::empty("box.stopped"));

        Ok(())
    }

    /// Transition to busy state.
    pub async fn set_busy(&self) -> Result<()> {
        let mut state = self.state.write().await;

        if *state != BoxState::Ready {
            return Err(BoxError::StateError("VM not ready".to_string()));
        }

        *state = BoxState::Busy;
        Ok(())
    }

    /// Transition back to ready state.
    pub async fn set_ready(&self) -> Result<()> {
        let mut state = self.state.write().await;

        if *state != BoxState::Busy && *state != BoxState::Compacting {
            return Err(BoxError::StateError("Invalid state transition".to_string()));
        }

        *state = BoxState::Ready;
        Ok(())
    }

    /// Transition to compacting state.
    pub async fn set_compacting(&self) -> Result<()> {
        let mut state = self.state.write().await;

        if *state != BoxState::Busy {
            return Err(BoxError::StateError("VM not busy".to_string()));
        }

        *state = BoxState::Compacting;
        Ok(())
    }

    /// Pause the VM by sending SIGSTOP to the shim process.
    ///
    /// The VM must be in Ready, Busy, or Compacting state.
    #[cfg(unix)]
    pub async fn pause(&self) -> Result<()> {
        let state = self.state.read().await;
        match *state {
            BoxState::Ready | BoxState::Busy | BoxState::Compacting => {}
            BoxState::Created => {
                return Err(BoxError::StateError("VM not yet booted".to_string()));
            }
            BoxState::Stopped => {
                return Err(BoxError::StateError("VM is stopped".to_string()));
            }
        }
        drop(state);

        if let Some(pid) = self.pid().await {
            // Safety: sending SIGSTOP to pause the process
            let ret = unsafe { libc::kill(pid as i32, libc::SIGSTOP) };
            if ret != 0 {
                let err = std::io::Error::last_os_error();
                return Err(BoxError::ExecError(format!(
                    "Failed to send SIGSTOP to pid {}: {}",
                    pid, err
                )));
            }
            tracing::info!(box_id = %self.box_id, pid, "VM paused");
            Ok(())
        } else {
            Err(BoxError::StateError(
                "VM has no running process".to_string(),
            ))
        }
    }

    /// Resume the VM by sending SIGCONT to the shim process.
    ///
    /// Can be called on a paused VM to resume execution.
    #[cfg(unix)]
    pub async fn resume(&self) -> Result<()> {
        if let Some(pid) = self.pid().await {
            // Safety: sending SIGCONT to resume the process
            let ret = unsafe { libc::kill(pid as i32, libc::SIGCONT) };
            if ret != 0 {
                let err = std::io::Error::last_os_error();
                return Err(BoxError::ExecError(format!(
                    "Failed to send SIGCONT to pid {}: {}",
                    pid, err
                )));
            }
            tracing::info!(box_id = %self.box_id, pid, "VM resumed");
            Ok(())
        } else {
            Err(BoxError::StateError(
                "VM has no running process".to_string(),
            ))
        }
    }

    /// Pause the VM (Windows stub - not yet implemented).
    #[cfg(windows)]
    pub async fn pause(&self) -> Result<()> {
        Err(BoxError::StateError(
            "VM pause is not yet supported on Windows".to_string(),
        ))
    }

    /// Resume the VM (Windows stub - not yet implemented).
    #[cfg(windows)]
    pub async fn resume(&self) -> Result<()> {
        Err(BoxError::StateError(
            "VM resume is not yet supported on Windows".to_string(),
        ))
    }

    /// Check if VM is healthy.
    pub async fn health_check(&self) -> Result<bool> {
        let state = self.state.read().await;

        match *state {
            BoxState::Ready | BoxState::Busy | BoxState::Compacting => {
                // Check if handler reports VM is running
                if let Some(ref handler) = *self.handler.read().await {
                    Ok(handler.is_running())
                } else {
                    Ok(false)
                }
            }
            _ => Ok(false),
        }
    }

    /// Get VM metrics.
    pub async fn metrics(&self) -> Option<crate::vmm::VmMetrics> {
        let vm_metrics = self
            .handler
            .read()
            .await
            .as_ref()
            .map(|handler| handler.metrics())?;

        // Update per-VM Prometheus gauges if metrics are attached
        if let Some(ref prom) = self.prom {
            prom.vm_cpu_percent
                .with_label_values(&[&self.box_id])
                .set(vm_metrics.cpu_percent.unwrap_or(0.0) as f64);
            prom.vm_memory_bytes
                .with_label_values(&[&self.box_id])
                .set(vm_metrics.memory_bytes.unwrap_or(0) as f64);
        }

        Some(vm_metrics)
    }

    /// Get the PID of the VM shim process.
    pub async fn pid(&self) -> Option<u32> {
        self.handler
            .read()
            .await
            .as_ref()
            .map(|handler| handler.pid())
    }

    /// Get the TEE extension, if TEE is configured and VM is booted.
    #[cfg(unix)]
    pub fn tee(&self) -> Option<&dyn TeeExtension> {
        self.tee.as_deref()
    }

    /// Get the TEE extension or return an error.
    #[cfg(unix)]
    pub fn require_tee(&self) -> Result<&dyn TeeExtension> {
        self.tee.as_deref().ok_or_else(|| {
            BoxError::AttestationError("TEE is not configured for this box".to_string())
        })
    }

    /// Apply a live resource update to the running VM.
    ///
    /// Tier 1 changes (vCPU count, memory size) are rejected with a clear error
    /// because libkrun does not expose a hot-resize API.
    ///
    /// Tier 2 changes (cgroup-based limits) are applied by executing shell
    /// commands inside the guest that write to cgroup v2 control files.
    #[cfg(unix)]
    pub async fn update_resources(
        &self,
        update: &crate::resize::ResourceUpdate,
    ) -> Result<crate::resize::ResizeResult> {
        // Reject Tier 1 changes upfront
        crate::resize::validate_update(update)?;

        let mut result = crate::resize::ResizeResult {
            applied: Vec::new(),
            rejected: Vec::new(),
        };

        if !update.has_tier2_changes() {
            return Ok(result);
        }

        // Build cgroup commands and execute them inside the guest
        let commands = update.build_cgroup_commands();
        for cmd_str in &commands {
            let shell_cmd = vec!["sh".to_string(), "-c".to_string(), cmd_str.clone()];

            match self.exec_command(shell_cmd, 5_000_000_000).await {
                Ok(output) if output.exit_code == 0 => {
                    result.applied.push(cmd_str.clone());
                }
                Ok(output) => {
                    let stderr = String::from_utf8_lossy(&output.stderr);
                    let reason = if stderr.trim().is_empty() {
                        format!("exit code {}", output.exit_code)
                    } else {
                        stderr.trim().to_string()
                    };
                    tracing::warn!(
                        box_id = %self.box_id,
                        cmd = %cmd_str,
                        exit_code = output.exit_code,
                        stderr = %stderr,
                        "Cgroup update failed inside guest"
                    );
                    result.rejected.push((cmd_str.clone(), reason));
                }
                Err(e) => {
                    tracing::warn!(
                        box_id = %self.box_id,
                        cmd = %cmd_str,
                        error = %e,
                        "Failed to exec cgroup update in guest"
                    );
                    result.rejected.push((cmd_str.clone(), e.to_string()));
                }
            }
        }

        Ok(result)
    }
}

/// Simple FNV-1a hash for generating short deterministic hashes from strings.
pub(crate) fn fnv1a_hash(input: &str) -> u64 {
    let mut hash: u64 = 0xcbf29ce484222325;
    for byte in input.bytes() {
        hash ^= byte as u64;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}
