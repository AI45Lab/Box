//! VM Manager - Lifecycle management for MicroVM instances.

mod layout;
mod network;
mod ready;
pub mod reap;
mod spec;

use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Callback type for image pull progress: `(current, total, digest, size_bytes)`.
type PullProgressFn = Arc<dyn Fn(usize, usize, &str, i64) + Send + Sync>;

use a3s_box_core::config::BoxConfig;
#[cfg(unix)]
use a3s_box_core::config::TeeConfig;
use a3s_box_core::error::{BoxError, Result};
use a3s_box_core::event::{BoxEvent, EventEmitter};
use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;
use tracing::Instrument;

#[cfg(unix)]
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
    /// Path to the CRI port-forward Unix socket
    pub(crate) port_forward_socket_path: PathBuf,
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

    /// Anonymous volumes newly created by the current boot attempt.
    ///
    /// Reused anonymous volumes must survive failed restarts because they may
    /// contain data from an existing stopped box.
    pub(crate) created_anonymous_volumes: Vec<String>,

    /// OCI image config resolved during the last successful boot.
    pub(crate) image_config: Option<crate::oci::OciImageConfig>,

    /// TEE extension (attestation, sealing, secret injection)
    #[cfg(unix)]
    pub(crate) tee: Option<Box<dyn TeeExtension>>,

    /// Rootfs provider (overlay or copy)
    pub(crate) rootfs_provider: Box<dyn crate::rootfs::RootfsProvider>,

    /// Path to the exec Unix socket (set after boot)
    pub(crate) exec_socket_path: Option<PathBuf>,

    /// Path to the PTY Unix socket (set after boot)
    pub(crate) pty_socket_path: Option<PathBuf>,

    /// Path to the CRI port-forward Unix socket (set after boot)
    pub(crate) port_forward_socket_path: Option<PathBuf>,

    /// Prometheus metrics (optional, for instrumented deployments).
    pub(crate) prom: Option<crate::prom::RuntimeMetrics>,

    /// Exit code captured from the shim process after it exits.
    pub(crate) shim_exit_code: Option<i32>,

    /// Optional progress callback for image pulls: `(current, total, digest, size_bytes)`.
    pub(crate) pull_progress_fn: Option<PullProgressFn>,

    /// Logging driver config, threaded into the InstanceSpec so the shim runs
    /// the log processor for the box's lifetime (set by the CLI via
    /// [`VmManager::set_log_config`]).
    pub(crate) log_config: a3s_box_core::log::LogConfig,
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
            created_anonymous_volumes: Vec::new(),
            image_config: None,
            #[cfg(unix)]
            tee: None,
            rootfs_provider: crate::rootfs::default_provider(),
            exec_socket_path: None,
            pty_socket_path: None,
            port_forward_socket_path: None,
            prom: None,
            shim_exit_code: None,
            pull_progress_fn: None,
            log_config: a3s_box_core::log::LogConfig::default(),
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
            created_anonymous_volumes: Vec::new(),
            image_config: None,
            #[cfg(unix)]
            tee: None,
            rootfs_provider: crate::rootfs::default_provider(),
            exec_socket_path: None,
            pty_socket_path: None,
            port_forward_socket_path: None,
            prom: None,
            shim_exit_code: None,
            pull_progress_fn: None,
            log_config: a3s_box_core::log::LogConfig::default(),
        }
    }

    /// Remove host-side boot artifacts after a failed boot attempt.
    async fn cleanup_boot_failure(&mut self) {
        if let Some(mut handler) = self.handler.write().await.take() {
            if let Err(error) = handler.stop(default_stop_signal(), DEFAULT_SHUTDOWN_TIMEOUT_MS) {
                tracing::warn!(
                    box_id = %self.box_id,
                    error = %error,
                    "Failed to stop VM handler after boot failure"
                );
            }
            self.shim_exit_code = handler.exit_code();
        }

        if let Some(mut net_manager) = self.net_manager.take() {
            net_manager.stop();
        }

        self.cleanup_created_anonymous_volumes();
        self.cleanup_box_dir();
    }

    fn cleanup_created_anonymous_volumes(&mut self) {
        if self.created_anonymous_volumes.is_empty() {
            return;
        }

        let created = std::mem::take(&mut self.created_anonymous_volumes);
        let created_set: std::collections::HashSet<_> = created.iter().cloned().collect();
        let store = crate::volume::VolumeStore::new(
            self.home_dir.join("volumes.json"),
            self.home_dir.join("volumes"),
        );

        for volume_name in &created {
            if let Err(error) = store.remove(volume_name, true) {
                tracing::debug!(
                    box_id = %self.box_id,
                    volume = volume_name,
                    error = %error,
                    "Failed to remove anonymous volume after boot failure"
                );
            }
        }

        self.anonymous_volumes
            .retain(|name| !created_set.contains(name));
    }

    /// Remove the box directory on the host.
    fn cleanup_box_dir(&self) {
        let box_dir = self.home_dir.join("boxes").join(&self.box_id);

        // Reap the box's passt daemon (Linux bridge mode) BEFORE removing its
        // socket dir. A boot that fails after passt spawned but before
        // `self.net_manager` was assigned leaves `net_manager.stop()` a no-op, so
        // passt would otherwise survive holding the published port — the
        // "Address already in use" on the next start. terminate_passt reads
        // `socket_dir/passt.pid` and is a no-op when there is no passt.
        #[cfg(target_os = "linux")]
        crate::network::terminate_passt(&self.socket_dir());

        if let Err(error) = self.rootfs_provider.cleanup(&box_dir, false) {
            tracing::warn!(
                box_id = %self.box_id,
                path = %box_dir.display(),
                error = %error,
                "Failed to cleanup rootfs provider after boot failure"
            );
        }

        match std::fs::remove_dir_all(&box_dir) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                tracing::warn!(
                    box_id = %self.box_id,
                    path = %box_dir.display(),
                    error = %error,
                    "Failed to cleanup box directory after boot failure"
                );
            }
        }

        let socket_dir = self.socket_dir();
        if socket_dir != box_dir.join("sockets") {
            match std::fs::remove_dir_all(&socket_dir) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => {
                    tracing::debug!(
                        box_id = %self.box_id,
                        path = %socket_dir.display(),
                        error = %error,
                        "Failed to cleanup socket directory after boot failure"
                    );
                }
            }
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
            created_anonymous_volumes: Vec::new(),
            image_config: None,
            #[cfg(unix)]
            tee: None,
            rootfs_provider: crate::rootfs::default_provider(),
            exec_socket_path: None,
            pty_socket_path: None,
            port_forward_socket_path: None,
            prom: None,
            shim_exit_code: None,
            pull_progress_fn: None,
            log_config: a3s_box_core::log::LogConfig::default(),
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

    /// Attach this manager to an already-running shim process.
    ///
    /// This is useful for crash recovery or control-plane restart flows where
    /// the workload VM is still alive and only the host-side manager state
    /// needs to be reconstructed.
    #[cfg(unix)]
    pub async fn attach_running_process(
        &mut self,
        pid: u32,
        exec_socket_path: PathBuf,
        pty_socket_path: Option<PathBuf>,
    ) -> Result<()> {
        let port_forward_socket_path = exec_socket_path.with_file_name("portfwd.sock");
        let handler = crate::vmm::ShimHandler::from_pid(pid, self.box_id.clone());
        if !handler.is_running() {
            return Err(BoxError::StateError(format!(
                "Cannot attach to non-running VM process {pid}"
            )));
        }

        self.exec_client = match ExecClient::connect(&exec_socket_path).await {
            Ok(client) => Some(client),
            Err(error) => {
                tracing::debug!(
                    box_id = %self.box_id,
                    socket_path = %exec_socket_path.display(),
                    error = %error,
                    "Failed to reconnect exec client while attaching to running VM"
                );
                None
            }
        };
        self.exec_socket_path = Some(exec_socket_path);
        self.pty_socket_path = pty_socket_path;
        self.port_forward_socket_path = Some(port_forward_socket_path);
        *self.handler.write().await = Some(Box::new(handler));
        *self.state.write().await = BoxState::Ready;
        Ok(())
    }

    /// Get the exec socket path, if the VM has been booted.
    pub fn exec_socket_path(&self) -> Option<&Path> {
        self.exec_socket_path.as_deref()
    }

    /// Get the PTY socket path, if the VM has been booted.
    pub fn pty_socket_path(&self) -> Option<&Path> {
        self.pty_socket_path.as_deref()
    }

    /// Get the CRI port-forward socket path, if the VM has been booted.
    pub fn port_forward_socket_path(&self) -> Option<&Path> {
        self.port_forward_socket_path.as_deref()
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
    pub fn set_pull_progress_fn(&mut self, f: PullProgressFn) {
        self.pull_progress_fn = Some(f);
    }

    /// Attach Prometheus metrics to this VM manager.
    pub fn set_metrics(&mut self, metrics: crate::prom::RuntimeMetrics) {
        self.prom = Some(metrics);
    }

    /// Set the logging driver config. Threaded into the InstanceSpec so the shim
    /// runs the log processor for the box's lifetime.
    pub fn set_log_config(&mut self, log_config: a3s_box_core::log::LogConfig) {
        self.log_config = log_config;
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

    /// Get the OCI image config resolved during boot.
    pub fn image_config(&self) -> Option<&crate::oci::OciImageConfig> {
        self.image_config.as_ref()
    }

    /// Get the exit code of the container, if it has exited.
    ///
    /// Returns `Some(code)` after `destroy()` has been called and the shim
    /// process exited naturally (not killed). Returns `None` if the VM has not
    /// yet stopped or the exit code could not be determined.
    pub fn exit_code(&self) -> Option<i32> {
        self.shim_exit_code
    }

    /// Poll the owned VM process for natural exit without sending a signal.
    ///
    /// This is used by foreground CLI flows where the container command may
    /// finish on its own and the CLI should clean up instead of waiting for
    /// a Ctrl-C.
    pub async fn try_wait_exit(&mut self) -> Result<Option<i32>> {
        let mut handler = self.handler.write().await;
        let Some(handler) = handler.as_mut() else {
            return Ok(self.shim_exit_code);
        };

        if let Some(code) = handler.try_wait_exit()? {
            self.shim_exit_code = Some(code);
            return Ok(Some(code));
        }

        Ok(None)
    }

    /// Run a command as the container MAIN in an IDLE-booted (deferred-main) VM.
    ///
    /// Sends the `spawn-main` control frame carrying `spec_json` (the command),
    /// waits for the main to exit (which halts the VM), and returns its real exit
    /// code + the box's json-file console logs split by stream. This is the full-
    /// box-semantics counterpart to [`Self::exec_command`] (whose output is piped
    /// over the exec stream, not the json-file logs).
    #[cfg(unix)]
    pub async fn run_deferred_main(
        &mut self,
        spec_json: &[u8],
        timeout: std::time::Duration,
    ) -> Result<a3s_box_core::exec::ExecOutput> {
        let acked = {
            let client = self
                .exec_client
                .as_ref()
                .ok_or_else(|| BoxError::ExecError("Exec client not connected".to_string()))?;
            client.spawn_main(Some(spec_json)).await?
        };
        if !acked {
            return Err(BoxError::ExecError(
                "spawn-main was not acknowledged by the guest".to_string(),
            ));
        }

        // Wait for the main to exit — guest-init persists the code and halts the VM.
        let start = std::time::Instant::now();
        let exit_code = loop {
            if let Some(code) = self.try_wait_exit().await? {
                break code;
            }
            if start.elapsed() >= timeout {
                return Err(BoxError::ExecError(
                    "deferred main did not exit within the timeout".to_string(),
                ));
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        };

        // Let the shim's log processor finish draining console.log into the json
        // file (it flushes as the VM halts): poll until container.json stops
        // growing for one interval (bounded at 1s) instead of a fixed sleep —
        // fast when the drain is already done, safe when it lags.
        let json_path = self
            .home_dir
            .join("boxes")
            .join(&self.box_id)
            .join("logs")
            .join("container.json");
        let drain_start = std::time::Instant::now();
        let mut last_len = u64::MAX;
        loop {
            let len = std::fs::metadata(&json_path).map(|m| m.len()).unwrap_or(0);
            if len == last_len || drain_start.elapsed() >= std::time::Duration::from_secs(1) {
                break;
            }
            last_len = len;
            tokio::time::sleep(std::time::Duration::from_millis(40)).await;
        }
        let (stdout, stderr) = self.read_container_logs();
        Ok(a3s_box_core::exec::ExecOutput {
            stdout,
            stderr,
            exit_code,
        })
    }

    /// Read the box's json-file console logs, split into stdout/stderr by stream.
    fn read_container_logs(&self) -> (Vec<u8>, Vec<u8>) {
        let path = self
            .home_dir
            .join("boxes")
            .join(&self.box_id)
            .join("logs")
            .join("container.json");
        let (mut out, mut err) = (Vec::new(), Vec::new());
        if let Ok(content) = std::fs::read_to_string(&path) {
            for line in content.lines() {
                if let Ok(entry) = serde_json::from_str::<a3s_box_core::log::LogEntry>(line) {
                    if entry.stream == "stderr" {
                        err.extend_from_slice(entry.log.as_bytes());
                    } else {
                        out.extend_from_slice(entry.log.as_bytes());
                    }
                }
            }
        }
        (out, err)
    }

    /// Execute a command in the guest VM.
    ///
    /// Requires the VM to be in Ready, Busy, or Compacting state.
    #[cfg(unix)]
    #[tracing::instrument(skip(self, request), fields(box_id = %self.box_id))]
    pub async fn exec_request(
        &self,
        request: &a3s_box_core::exec::ExecRequest,
    ) -> Result<a3s_box_core::exec::ExecOutput> {
        if request.cmd.is_empty() {
            return Err(BoxError::ExecError(
                "Exec request requires a non-empty command".to_string(),
            ));
        }

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

        let exec_start = std::time::Instant::now();
        let result = client.exec_command(request).await;

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
        let request = a3s_box_core::exec::ExecRequest {
            cmd,
            timeout_ns,
            env: vec![],
            working_dir: None,
            rootfs: None,
            stdin: None,
            stdin_streaming: false,
            user: None,
            streaming: false,
        };

        self.exec_request(&request).await
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
        let layout = match self
            .prepare_layout()
            .instrument(tracing::info_span!(parent: &boot_span, "prepare_layout"))
            .await
        {
            Ok(layout) => layout,
            Err(error) => {
                self.cleanup_boot_failure().await;
                return Err(error);
            }
        };
        self.image_config = layout.oci_config.clone();

        // 1.5. Override /etc/resolv.conf with configured DNS
        let resolv_content = a3s_box_core::dns::generate_resolv_conf(&self.config.dns);
        let resolv_path = layout.rootfs_path.join("etc/resolv.conf");
        if let Err(e) = tokio::fs::write(&resolv_path, &resolv_content).await {
            self.cleanup_boot_failure().await;
            return Err(BoxError::IoError(e));
        }
        tracing::debug!(parent: &boot_span, dns = %resolv_content.trim(), "Configured guest DNS");

        // 1.6. Apply hostname and static hosts entries before the VM starts.
        if let Err(e) = self.write_hostname_file(&layout) {
            self.cleanup_boot_failure().await;
            return Err(e);
        }
        if let Err(e) = self.write_standalone_hosts_file(&layout) {
            self.cleanup_boot_failure().await;
            return Err(e);
        }

        // 2. Build InstanceSpec
        let mut spec = match self.build_instance_spec(&layout) {
            Ok(s) => s,
            Err(e) => {
                self.cleanup_boot_failure().await;
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
                    self.cleanup_boot_failure().await;
                    return Err(e);
                }
            };

            // Write /etc/hosts for DNS service discovery
            match self.write_hosts_file(&layout, &network_name) {
                Ok(()) => (),
                Err(e) => {
                    self.cleanup_boot_failure().await;
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
                    self.cleanup_boot_failure().await;
                    return Err(e);
                }
            };
            let controller = match VmController::new(shim_path) {
                Ok(c) => c,
                Err(e) => {
                    self.cleanup_boot_failure().await;
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
                    self.cleanup_boot_failure().await;
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

                // 5b. Become ready. A snapshot-restore boot resumes an already-booted
                // guest whose exec server won't re-signal readiness, so the cold-boot
                // wait would stall registration on its safety cap — do one best-effort
                // probe instead. A normal boot waits for the Heartbeat health check.
                #[cfg(unix)]
                if is_restore_mode(&self.config) {
                    self.probe_exec_ready_once(&layout.exec_socket_path).await;
                } else {
                    self.wait_for_exec_ready(&layout.exec_socket_path).await?;
                }
                Ok::<(), BoxError>(())
            }
            .instrument(wait_span)
            .await
            {
                self.cleanup_boot_failure().await;
                return Err(e);
            }
        }

        // Prototype: deferred-main-spawn. The guest booted IDLE (BOX_DEFERRED_MAIN);
        // now that the exec server is ready, tell it to spawn the container command
        // (already passed via BOX_EXEC_*) as the MAIN process — full box semantics
        // (exit code + json-file console logs) without a cold boot.
        // Auto-trigger spawn-main only for the env-driven `run` path, where the
        // command is known at boot. The pool sets config.deferred_main to boot the
        // VM IDLE but drives spawn-main EXPLICITLY per request (the per-request
        // command isn't known at pre-warm), so a pool VM must NOT auto-trigger here.
        // A restored guest's main is ALREADY running (captured in the snapshot), so
        // it must never re-spawn — doing so would start a duplicate main.
        #[cfg(unix)]
        if !is_restore_mode(&self.config)
            && std::env::var("BOX_DEFERRED_MAIN")
                .map(|v| v == "1")
                .unwrap_or(false)
        {
            if let Some(client) = self.exec_client.as_ref() {
                match client.spawn_main(None).await {
                    Ok(true) => tracing::info!("deferred container main spawned"),
                    Ok(false) => tracing::warn!("deferred spawn-main not acknowledged"),
                    Err(e) => tracing::warn!(error = %e, "deferred spawn-main failed"),
                }
            }
        }

        // 5b2. Store socket paths for CRI streaming access
        self.exec_socket_path = Some(layout.exec_socket_path.clone());
        self.pty_socket_path = Some(layout.pty_socket_path.clone());
        self.port_forward_socket_path = Some(layout.port_forward_socket_path.clone());

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
        self.destroy_with_options(default_stop_signal(), DEFAULT_SHUTDOWN_TIMEOUT_MS)
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
        // A stop failure must NOT skip the host-resource teardown below (network
        // backend, overlay unmount, socket + box dirs) — those are already
        // best-effort and would otherwise leak on every wedged stop. Capture the
        // error and surface it after teardown instead of returning early.
        let mut stop_error = None;
        if let Some(mut handler) = self.handler.write().await.take() {
            if let Err(e) = handler.stop(signal, timeout_ms) {
                tracing::error!(box_id = %self.box_id, error = %e, "Failed to stop VM handler; continuing teardown");
                stop_error = Some(e);
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

        let socket_dir = self.socket_dir();
        if let Err(e) = std::fs::remove_dir_all(&socket_dir) {
            tracing::debug!(
                box_id = %self.box_id,
                path = %socket_dir.display(),
                error = %e,
                "Failed to cleanup VM socket directory"
            );
        }

        // Remove the box working directory itself (overlay upper/work, logs,
        // leftover metadata) for non-persistent boxes. Without this, ephemeral
        // CRI pods leak their `boxes/<id>` directory on every destroy; the
        // accumulation slows later RunPodSandbox calls until they time out
        // (observed: pod #21 after churning 20). Persistent boxes keep their
        // dir intentionally.
        if !self.config.persistent {
            match std::fs::remove_dir_all(&box_dir) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => {
                    tracing::warn!(
                        box_id = %self.box_id,
                        path = %box_dir.display(),
                        error = %e,
                        "Failed to remove box directory on destroy"
                    );
                }
            }
        }

        // Record Prometheus metrics
        if let Some(ref prom) = self.prom {
            prom.vm_destroyed_total.inc();
            prom.vm_count.with_label_values(&["ready"]).dec();
        }

        // Emit stopped event
        self.event_emitter.emit(BoxEvent::empty("box.stopped"));

        // Host teardown above is complete; surface a handler-stop failure now so
        // the caller still learns the stop was imperfect.
        match stop_error {
            Some(e) => Err(e),
            None => Ok(()),
        }
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

/// Whether this boot is a snapshot-fork restore (the guest is resumed already-booted
/// rather than cold-booted). PER-VM: a pool / fork daemon sets `config.restore_from`
/// so one process can restore different VMs; the single-VM `run` path uses the
/// `KRUN_RESTORE_FROM` env. Either source means restore mode.
#[cfg(unix)]
fn is_restore_mode(config: &BoxConfig) -> bool {
    config
        .restore_from
        .as_deref()
        .is_some_and(|s| !s.is_empty())
        || std::env::var("KRUN_RESTORE_FROM")
            .map(|v| !v.is_empty())
            .unwrap_or(false)
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

#[cfg(unix)]
fn default_stop_signal() -> i32 {
    libc::SIGTERM
}

#[cfg(windows)]
fn default_stop_signal() -> i32 {
    15
}

#[cfg(test)]
mod tests {
    use super::*;
    use a3s_box_core::event::EventEmitter;
    use std::sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    };

    struct RecordingHandler {
        stopped: Arc<AtomicBool>,
    }

    impl VmHandler for RecordingHandler {
        fn stop(&mut self, _signal: i32, _timeout_ms: u64) -> Result<()> {
            self.stopped.store(true, Ordering::SeqCst);
            Ok(())
        }

        fn metrics(&self) -> crate::vmm::VmMetrics {
            crate::vmm::VmMetrics::default()
        }

        fn is_running(&self) -> bool {
            true
        }

        fn pid(&self) -> u32 {
            42
        }
    }

    /// A handler whose `stop` always fails — models a wedged VM that won't halt.
    struct FailingHandler;

    impl VmHandler for FailingHandler {
        fn stop(&mut self, _signal: i32, _timeout_ms: u64) -> Result<()> {
            Err(BoxError::StateError("simulated stop failure".to_string()))
        }

        fn metrics(&self) -> crate::vmm::VmMetrics {
            crate::vmm::VmMetrics::default()
        }

        fn is_running(&self) -> bool {
            true
        }

        fn pid(&self) -> u32 {
            42
        }
    }

    // Regression: a handler-stop failure must still run the host teardown
    // (overlay unmount, socket + box dirs). Pre-fix, destroy_with_options
    // returned early on the stop error and leaked the box directory on every
    // wedged stop.
    #[tokio::test]
    async fn test_destroy_runs_host_teardown_even_when_handler_stop_fails() {
        let tmp = tempfile::tempdir().unwrap();
        let box_id = "box-stopfail".to_string();
        // persistent=false (the default) → the box dir is removed on destroy.
        let mut vm =
            VmManager::with_box_id(BoxConfig::default(), EventEmitter::new(16), box_id.clone());
        vm.home_dir = tmp.path().to_path_buf();
        *vm.handler.write().await = Some(Box::new(FailingHandler));

        let box_dir = tmp.path().join("boxes").join(&box_id);
        std::fs::create_dir_all(box_dir.join("logs")).unwrap();

        let result = vm.destroy_with_options(default_stop_signal(), 100).await;

        // The stop failure is still surfaced to the caller...
        assert!(
            result.is_err(),
            "a handler-stop failure must still be reported"
        );
        // ...but the host teardown ran anyway: handler taken + box dir removed.
        assert!(vm.handler.read().await.is_none());
        assert!(
            !box_dir.exists(),
            "non-persistent box dir must be removed even when the stop failed"
        );
    }

    #[tokio::test]
    async fn test_cleanup_boot_failure_stops_handler_and_removes_created_volumes() {
        let tmp = tempfile::tempdir().unwrap();
        let box_id = "box-test".to_string();
        let mut vm =
            VmManager::with_box_id(BoxConfig::default(), EventEmitter::new(16), box_id.clone());
        vm.home_dir = tmp.path().to_path_buf();
        vm.anonymous_volumes = vec!["created-volume".to_string(), "reused-volume".to_string()];
        vm.created_anonymous_volumes = vec!["created-volume".to_string()];

        let stopped = Arc::new(AtomicBool::new(false));
        *vm.handler.write().await = Some(Box::new(RecordingHandler {
            stopped: stopped.clone(),
        }));

        let box_dir = tmp.path().join("boxes").join(&box_id);
        std::fs::create_dir_all(box_dir.join("logs")).unwrap();

        let store = crate::volume::VolumeStore::new(
            tmp.path().join("volumes.json"),
            tmp.path().join("volumes"),
        );
        store
            .create(a3s_box_core::volume::VolumeConfig::new(
                "created-volume",
                "",
            ))
            .unwrap();
        store
            .create(a3s_box_core::volume::VolumeConfig::new("reused-volume", ""))
            .unwrap();

        vm.cleanup_boot_failure().await;

        assert!(stopped.load(Ordering::SeqCst));
        assert!(vm.handler.read().await.is_none());
        assert!(vm.created_anonymous_volumes.is_empty());
        assert_eq!(vm.anonymous_volumes, vec!["reused-volume".to_string()]);
        assert!(store.get("created-volume").unwrap().is_none());
        assert!(store.get("reused-volume").unwrap().is_some());
        assert!(!box_dir.exists());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn test_attach_running_process_infers_port_forward_socket_path() {
        let mut vm = VmManager::with_box_id(
            BoxConfig::default(),
            EventEmitter::new(16),
            "box-test".to_string(),
        );
        let tmp = tempfile::tempdir().unwrap();
        let exec_socket_path = tmp.path().join("exec.sock");
        let pty_socket_path = Some(tmp.path().join("pty.sock"));

        vm.attach_running_process(
            std::process::id(),
            exec_socket_path.clone(),
            pty_socket_path.clone(),
        )
        .await
        .unwrap();

        assert_eq!(vm.exec_socket_path(), Some(exec_socket_path.as_path()));
        assert_eq!(vm.pty_socket_path(), pty_socket_path.as_deref());
        assert_eq!(
            vm.port_forward_socket_path(),
            Some(exec_socket_path.with_file_name("portfwd.sock").as_path())
        );
        assert_eq!(vm.state().await, BoxState::Ready);
    }
}
