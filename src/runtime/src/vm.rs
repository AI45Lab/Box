//! VM Manager - Lifecycle management for MicroVM instances.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use a3s_box_core::config::{AgentType, BoxConfig, BusinessType, TeeConfig};
use a3s_box_core::error::{BoxError, Result};
use a3s_box_core::event::{BoxEvent, EventEmitter};
use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;
use tracing::Instrument;

use crate::cache::RootfsCache;
use crate::grpc::{AgentClient, ExecClient};
use crate::network::PasstManager;
use crate::oci::{OciImageConfig, OciRootfsBuilder};
use crate::rootfs::{GUEST_AGENT_PATH, GUEST_WORKDIR};
use crate::tee::TeeExtension;
use crate::vmm::{
    Entrypoint, FsMount, InstanceSpec, NetworkInstanceConfig, TeeInstanceConfig, VmController,
    VmHandler, VmmProvider, DEFAULT_SHUTDOWN_TIMEOUT_MS,
};
use crate::AGENT_VSOCK_PORT;

/// Box state machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum BoxState {
    /// Config captured, no VM started
    Created,

    /// VM booted, agent initialized, gRPC healthy
    Ready,

    /// A session is actively processing a prompt
    Busy,

    /// A session is compressing its context
    Compacting,

    /// VM terminated, resources freed
    Stopped,
}

/// Layout of directories for a box instance.
struct BoxLayout {
    /// Path to the root filesystem
    rootfs_path: PathBuf,
    /// Path to the gRPC Unix socket
    socket_path: PathBuf,
    /// Path to the exec Unix socket
    exec_socket_path: PathBuf,
    /// Path to the PTY Unix socket
    pty_socket_path: PathBuf,
    /// Path to the attestation Unix socket
    attest_socket_path: PathBuf,
    /// Path to the workspace directory
    workspace_path: PathBuf,
    /// Path to the skills directory
    skills_path: PathBuf,
    /// Path to console output file (optional)
    console_output: Option<PathBuf>,
    /// OCI image config for agent (if using OCI image)
    agent_oci_config: Option<OciImageConfig>,
    /// Whether guest init is installed for namespace isolation
    has_guest_init: bool,
    /// TEE instance configuration (if TEE is enabled)
    tee_instance_config: Option<TeeInstanceConfig>,
    /// Whether the OCI image is extracted at rootfs root (true) or under /agent (false).
    /// Images from OCI registries are extracted at root so absolute symlinks work.
    image_at_root: bool,
}

/// VM manager - orchestrates VM lifecycle.
pub struct VmManager {
    /// Box configuration
    config: BoxConfig,

    /// Unique box identifier
    box_id: String,

    /// Current state
    state: Arc<RwLock<BoxState>>,

    /// Event emitter
    event_emitter: EventEmitter,

    /// VMM provider (spawns VMs via pluggable backend)
    provider: Option<Box<dyn VmmProvider>>,

    /// VM handler (runtime operations on running VM)
    handler: Arc<RwLock<Option<Box<dyn VmHandler>>>>,

    /// gRPC client for communicating with the guest agent
    agent_client: Option<AgentClient>,

    /// Exec client for executing commands in the guest
    exec_client: Option<ExecClient>,

    /// Passt manager for bridge networking (None if TSI mode)
    passt_manager: Option<PasstManager>,

    /// A3S home directory (~/.a3s)
    home_dir: PathBuf,

    /// Anonymous volume names created during boot (from OCI VOLUME directives)
    anonymous_volumes: Vec<String>,

    /// TEE extension (attestation, sealing, secret injection)
    tee: Option<Box<dyn TeeExtension>>,

    /// Path to the exec Unix socket (set after boot)
    exec_socket_path: Option<PathBuf>,

    /// Path to the PTY Unix socket (set after boot)
    pty_socket_path: Option<PathBuf>,

    /// Prometheus metrics (optional, for instrumented deployments).
    prom: Option<crate::prom::RuntimeMetrics>,
}

impl VmManager {
    /// Create a new VM manager.
    pub fn new(config: BoxConfig, event_emitter: EventEmitter) -> Self {
        let box_id = uuid::Uuid::new_v4().to_string();
        let home_dir = dirs_home().unwrap_or_else(|| PathBuf::from(".a3s"));

        Self {
            config,
            box_id,
            state: Arc::new(RwLock::new(BoxState::Created)),
            event_emitter,
            provider: None,
            handler: Arc::new(RwLock::new(None)),
            agent_client: None,
            exec_client: None,
            passt_manager: None,
            home_dir,
            anonymous_volumes: Vec::new(),
            tee: None,
            exec_socket_path: None,
            pty_socket_path: None,
            prom: None,
        }
    }

    /// Create a new VM manager with a specific box ID.
    pub fn with_box_id(config: BoxConfig, event_emitter: EventEmitter, box_id: String) -> Self {
        let home_dir = dirs_home().unwrap_or_else(|| PathBuf::from(".a3s"));

        Self {
            config,
            box_id,
            state: Arc::new(RwLock::new(BoxState::Created)),
            event_emitter,
            provider: None,
            handler: Arc::new(RwLock::new(None)),
            agent_client: None,
            exec_client: None,
            passt_manager: None,
            home_dir,
            anonymous_volumes: Vec::new(),
            tee: None,
            exec_socket_path: None,
            pty_socket_path: None,
            prom: None,
        }
    }

    /// Create a new VM manager with a custom VMM provider.
    pub fn with_provider(
        config: BoxConfig,
        event_emitter: EventEmitter,
        provider: Box<dyn VmmProvider>,
    ) -> Self {
        let box_id = uuid::Uuid::new_v4().to_string();
        let home_dir = dirs_home().unwrap_or_else(|| PathBuf::from(".a3s"));
        Self {
            config, box_id,
            state: Arc::new(RwLock::new(BoxState::Created)),
            event_emitter,
            provider: Some(provider),
            handler: Arc::new(RwLock::new(None)),
            agent_client: None, exec_client: None, passt_manager: None,
            home_dir, anonymous_volumes: Vec::new(), tee: None,
            exec_socket_path: None, pty_socket_path: None, prom: None,
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

    /// Get the agent client, if connected.
    pub fn agent_client(&self) -> Option<&AgentClient> {
        self.agent_client.as_ref()
    }

    /// Get the exec client, if connected.
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

    /// Execute a command in the guest VM.
    ///
    /// Requires the VM to be in Ready, Busy, or Compacting state.
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
            prom.exec_duration.observe(exec_start.elapsed().as_secs_f64());
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
                return Err(BoxError::Other("VM already booted".to_string()));
            }
        }

        let boot_start = std::time::Instant::now();

        tracing::info!(parent: &boot_span, box_id = %self.box_id, "Booting VM");

        // 1. Prepare filesystem layout
        let layout = {
            let _span = tracing::info_span!(parent: &boot_span, "prepare_layout").entered();
            self.prepare_layout()?
        };

        // 1.5. Override /etc/resolv.conf with configured DNS
        let resolv_content = a3s_box_core::dns::generate_resolv_conf(&self.config.dns);
        let resolv_path = layout.rootfs_path.join("etc/resolv.conf");
        std::fs::write(&resolv_path, &resolv_content).map_err(|e| {
            BoxError::Other(format!("Failed to write {}: {}", resolv_path.display(), e))
        })?;
        tracing::debug!(parent: &boot_span, dns = %resolv_content.trim(), "Configured guest DNS");

        // 2. Build InstanceSpec
        let mut spec = self.build_instance_spec(&layout)?;

        // 2.5. Configure bridge networking if requested
        let bridge_network = match &self.config.network {
            a3s_box_core::NetworkMode::Bridge { network } => Some(network.clone()),
            _ => None,
        };
        if let Some(network_name) = bridge_network {
            let net_config = self.setup_bridge_network(&network_name)?;

            // Write /etc/hosts for DNS service discovery
            self.write_hosts_file(&layout, &network_name)?;

            // Inject network env vars into entrypoint so they are passed via
            // krun_set_exec's envp (not krun_set_env which overwrites all vars).
            let ip_cidr = format!("{}/{}", net_config.ip_address, net_config.prefix_len);
            spec.entrypoint.env.push(("A3S_NET_IP".to_string(), ip_cidr));
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
            let shim_path = VmController::find_shim()?;
            let controller = VmController::new(shim_path)?;
            self.provider = Some(Box::new(controller));
        }

        // 4. Start VM via provider
        let handler = {
            let vm_start_span = tracing::info_span!(parent: &boot_span, "vm_start");
            async {
                self.provider.as_ref().unwrap().start(&spec).await
            }
            .instrument(vm_start_span)
            .await?
        };

        // Store handler
        *self.handler.write().await = Some(handler);

        // 5. Wait for guest ready
        {
            let wait_span = tracing::info_span!(parent: &boot_span, "wait_for_ready");
            async {
                if layout.image_at_root {
                    self.wait_for_vm_running().await?;
                } else {
                    self.wait_for_agent_socket(&layout.socket_path).await?;
                }

                // 5b. Wait for exec server to become ready (Heartbeat health check)
                self.wait_for_exec_ready(&layout.exec_socket_path).await?;
                Ok::<(), BoxError>(())
            }
            .instrument(wait_span)
            .await?;
        }

        // 5b2. Store socket paths for CRI streaming access
        self.exec_socket_path = Some(layout.exec_socket_path.clone());
        self.pty_socket_path = Some(layout.pty_socket_path.clone());

        // 5c. Initialize TEE extension for TEE environments
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

    /// Set up bridge networking by looking up the network, spawning passt,
    /// and building the NetworkInstanceConfig for the VM spec.
    fn setup_bridge_network(&mut self, network_name: &str) -> Result<NetworkInstanceConfig> {
        use crate::network::NetworkStore;

        let store = NetworkStore::default_path()?;
        let net_config = store.get(network_name)?.ok_or_else(|| {
            BoxError::NetworkError(format!("network '{}' not found", network_name))
        })?;

        // Find this box's endpoint in the network
        let endpoint = net_config
            .endpoints
            .get(&self.box_id)
            .ok_or_else(|| BoxError::NetworkError(format!(
                "box '{}' is not connected to network '{}'; run with --network or use 'network connect'",
                self.box_id, network_name
            )))?;

        let ip = endpoint.ip_address;
        let gateway = net_config.gateway;

        // Parse prefix length from subnet CIDR
        let prefix_len: u8 = net_config
            .subnet
            .split('/')
            .nth(1)
            .and_then(|s| s.parse().ok())
            .unwrap_or(24);

        // Parse MAC address from hex string "02:42:0a:58:00:02" → [u8; 6]
        let mac_address = parse_mac(&endpoint.mac_address).map_err(|e| {
            BoxError::NetworkError(format!(
                "invalid MAC address '{}': {}",
                endpoint.mac_address, e
            ))
        })?;

        // Determine DNS servers
        let dns_servers: Vec<std::net::Ipv4Addr> = if !self.config.dns.is_empty() {
            self.config
                .dns
                .iter()
                .filter_map(|s| s.parse().ok())
                .collect()
        } else {
            vec![std::net::Ipv4Addr::new(8, 8, 8, 8)]
        };

        // Spawn passt daemon
        let box_dir = self.home_dir.join("boxes").join(&self.box_id);
        let mut passt = PasstManager::new(&box_dir);
        passt.spawn(ip, gateway, prefix_len, &dns_servers)?;

        let socket_path = passt.socket_path().to_path_buf();
        self.passt_manager = Some(passt);

        tracing::info!(
            network = network_name,
            ip = %ip,
            gateway = %gateway,
            "Bridge networking configured via passt"
        );

        Ok(NetworkInstanceConfig {
            passt_socket_path: socket_path,
            ip_address: ip,
            gateway,
            prefix_len,
            mac_address,
            dns_servers,
        })
    }

    /// Write /etc/hosts to the guest rootfs for DNS service discovery.
    ///
    /// Looks up the box's own endpoint and all peer endpoints in the network,
    /// then generates a hosts file mapping IPs to box names.
    fn write_hosts_file(&self, layout: &BoxLayout, network_name: &str) -> Result<()> {
        use crate::network::NetworkStore;

        let store = NetworkStore::default_path()?;
        let net_config = store.get(network_name)?.ok_or_else(|| {
            BoxError::NetworkError(format!("network '{}' not found", network_name))
        })?;

        let endpoint = net_config.endpoints.get(&self.box_id).ok_or_else(|| {
            BoxError::NetworkError(format!(
                "box '{}' not connected to network '{}'",
                self.box_id, network_name
            ))
        })?;

        let own_ip = endpoint.ip_address.to_string();
        let own_name = endpoint.box_name.clone();
        let peers = net_config.peer_endpoints(&self.box_id);

        let hosts_content = a3s_box_core::dns::generate_hosts_file(&own_ip, &own_name, &peers);
        let hosts_path = layout.rootfs_path.join("etc/hosts");
        std::fs::write(&hosts_path, &hosts_content).map_err(|e| {
            BoxError::Other(format!("Failed to write {}: {}", hosts_path.display(), e))
        })?;
        tracing::debug!(hosts = %hosts_content.trim(), "Configured guest /etc/hosts for DNS discovery");

        Ok(())
    }

    /// Destroy the VM with the default shutdown timeout.
    pub async fn destroy(&mut self) -> Result<()> {
        self.destroy_with_timeout(DEFAULT_SHUTDOWN_TIMEOUT_MS).await
    }

    /// Destroy the VM with a custom shutdown timeout.
    ///
    /// Sends SIGTERM to the shim process and waits up to `timeout_ms` for it
    /// to exit gracefully before sending SIGKILL.
    #[tracing::instrument(skip(self), fields(box_id = %self.box_id))]
    pub async fn destroy_with_timeout(&mut self, timeout_ms: u64) -> Result<()> {
        let mut state = self.state.write().await;

        if *state == BoxState::Stopped {
            return Ok(());
        }

        tracing::info!(box_id = %self.box_id, timeout_ms, "Destroying VM");

        // Stop the VM handler
        if let Some(mut handler) = self.handler.write().await.take() {
            handler.stop(timeout_ms)?;
        }

        // Stop passt daemon if running
        if let Some(ref mut passt) = self.passt_manager {
            passt.stop();
        }
        self.passt_manager = None;

        *state = BoxState::Stopped;

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
            return Err(BoxError::Other("VM not ready".to_string()));
        }

        *state = BoxState::Busy;
        Ok(())
    }

    /// Transition back to ready state.
    pub async fn set_ready(&self) -> Result<()> {
        let mut state = self.state.write().await;

        if *state != BoxState::Busy && *state != BoxState::Compacting {
            return Err(BoxError::Other("Invalid state transition".to_string()));
        }

        *state = BoxState::Ready;
        Ok(())
    }

    /// Transition to compacting state.
    pub async fn set_compacting(&self) -> Result<()> {
        let mut state = self.state.write().await;

        if *state != BoxState::Busy {
            return Err(BoxError::Other("VM not busy".to_string()));
        }

        *state = BoxState::Compacting;
        Ok(())
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
        self.handler
            .read()
            .await
            .as_ref()
            .map(|handler| handler.metrics())
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
    pub fn tee(&self) -> Option<&dyn TeeExtension> {
        self.tee.as_deref()
    }

    /// Get the TEE extension or return an error.
    pub fn require_tee(&self) -> Result<&dyn TeeExtension> {
        self.tee.as_deref().ok_or_else(|| {
            BoxError::AttestationError("TEE is not configured for this box".to_string())
        })
    }

    fn prepare_layout(&self) -> Result<BoxLayout> {
        // Create box-specific directories
        let box_dir = self.home_dir.join("boxes").join(&self.box_id);
        let socket_dir = box_dir.join("sockets");
        let logs_dir = box_dir.join("logs");

        std::fs::create_dir_all(&socket_dir).map_err(|e| BoxError::BoxBootError {
            message: format!("Failed to create socket directory: {}", e),
            hint: None,
        })?;

        std::fs::create_dir_all(&logs_dir).map_err(|e| BoxError::BoxBootError {
            message: format!("Failed to create logs directory: {}", e),
            hint: None,
        })?;

        // Resolve paths from config
        let workspace_path = PathBuf::from(&self.config.workspace);

        // Use first skills directory, or create a default one
        let skills_path = self
            .config
            .skills
            .first()
            .cloned()
            .unwrap_or_else(|| self.home_dir.join("skills"));

        // Ensure workspace exists
        if !workspace_path.exists() {
            std::fs::create_dir_all(&workspace_path).map_err(|e| BoxError::BoxBootError {
                message: format!("Failed to create workspace directory: {}", e),
                hint: None,
            })?;
        }

        // Ensure skills directory exists
        if !skills_path.exists() {
            std::fs::create_dir_all(&skills_path).map_err(|e| BoxError::BoxBootError {
                message: format!("Failed to create skills directory: {}", e),
                hint: None,
            })?;
        }

        // Canonicalize paths to absolute (libkrun requires absolute paths for virtiofs)
        let workspace_path = workspace_path
            .canonicalize()
            .map_err(|e| BoxError::BoxBootError {
                message: format!(
                    "Failed to resolve workspace path {}: {}",
                    workspace_path.display(),
                    e
                ),
                hint: None,
            })?;
        let skills_path = skills_path
            .canonicalize()
            .map_err(|e| BoxError::BoxBootError {
                message: format!(
                    "Failed to resolve skills path {}: {}",
                    skills_path.display(),
                    e
                ),
                hint: None,
            })?;

        // Prepare rootfs based on agent type
        let (rootfs_path, agent_oci_config, has_guest_init, image_at_root) = match &self
            .config
            .agent
        {
            AgentType::OciImage { path: agent_path } => {
                // Use OCI image for agent (extracted under /agent)
                let rootfs_path = box_dir.join("rootfs");

                // Try rootfs cache first
                let cache_key =
                    RootfsCache::compute_key(&agent_path.display().to_string(), &[], &[], &[]);
                if let Some(cached) = self.try_rootfs_cache(&cache_key, &rootfs_path)? {
                    tracing::info!(
                        cache_key = %&cache_key[..12],
                        "Rootfs cache hit, skipping OCI extraction"
                    );
                    let builder = OciRootfsBuilder::new(&rootfs_path)
                        .with_agent_image(agent_path)
                        .with_agent_target("/agent")
                        .with_business_target("/workspace");
                    let agent_config = builder.agent_config()?;
                    let has_guest_init = cached.join("sbin/init").exists();
                    (rootfs_path, Some(agent_config), has_guest_init, false)
                } else {
                    tracing::info!(
                        agent_image = %agent_path.display(),
                        rootfs = %rootfs_path.display(),
                        "Building rootfs from OCI images"
                    );

                    // Build rootfs using OciRootfsBuilder
                    let mut builder = OciRootfsBuilder::new(&rootfs_path)
                        .with_agent_image(agent_path)
                        .with_agent_target("/agent")
                        .with_business_target("/workspace");

                    // Add business image if specified
                    if let BusinessType::OciImage {
                        path: business_path,
                    } = &self.config.business
                    {
                        builder = builder.with_business_image(business_path);
                    }

                    // Add guest init if available
                    let has_guest_init = if let Ok(guest_init_path) = Self::find_guest_init() {
                        tracing::info!(
                            guest_init = %guest_init_path.display(),
                            "Using guest init for namespace isolation"
                        );
                        builder = builder.with_guest_init(guest_init_path);

                        // Also add nsexec if available
                        if let Ok(nsexec_path) = Self::find_nsexec() {
                            tracing::info!(
                                nsexec = %nsexec_path.display(),
                                "Installing nsexec for business code execution"
                            );
                            builder = builder.with_nsexec(nsexec_path);
                        }

                        true
                    } else {
                        false
                    };

                    // Build the rootfs
                    builder.build()?;

                    // Get agent OCI config for entrypoint/env extraction
                    let agent_config = builder.agent_config()?;

                    // Store in cache for next time
                    self.store_rootfs_cache(
                        &cache_key,
                        &rootfs_path,
                        &agent_path.display().to_string(),
                    );

                    (rootfs_path, Some(agent_config), has_guest_init, false)
                }
            }
            AgentType::OciRegistry { reference } => {
                // Pull image from registry and extract at rootfs root.
                // This preserves absolute symlinks and dynamic linker paths.
                let images_dir = self.home_dir.join("images");
                let store =
                    crate::oci::ImageStore::new(&images_dir, crate::DEFAULT_IMAGE_CACHE_SIZE)?;
                let puller = crate::oci::ImagePuller::new(
                    std::sync::Arc::new(store),
                    crate::oci::RegistryAuth::from_env(),
                );

                tracing::info!(
                    reference = %reference,
                    "Pulling OCI image from registry"
                );

                let oci_image = tokio::task::block_in_place(|| {
                    tokio::runtime::Handle::current().block_on(puller.pull(reference))
                })?;

                let agent_path = oci_image.root_dir().to_path_buf();
                let rootfs_path = box_dir.join("rootfs");

                // Try rootfs cache first
                let cache_key = RootfsCache::compute_key(reference, &[], &[], &[]);
                if let Some(cached) = self.try_rootfs_cache(&cache_key, &rootfs_path)? {
                    tracing::info!(
                        cache_key = %&cache_key[..12],
                        reference = %reference,
                        "Rootfs cache hit, skipping OCI extraction"
                    );
                    let builder = OciRootfsBuilder::new(&rootfs_path)
                        .with_agent_image(&agent_path)
                        .with_agent_target("/")
                        .with_business_target("/workspace");
                    let agent_config = builder.agent_config()?;
                    let has_guest_init = cached.join("sbin/init").exists();
                    (rootfs_path, Some(agent_config), has_guest_init, true)
                } else {
                    tracing::info!(
                        agent_image = %agent_path.display(),
                        rootfs = %rootfs_path.display(),
                        "Building rootfs from pulled OCI image"
                    );

                    // Extract at root ("/") so absolute symlinks and library paths work
                    let mut builder = OciRootfsBuilder::new(&rootfs_path)
                        .with_agent_image(&agent_path)
                        .with_agent_target("/")
                        .with_business_target("/workspace");

                    if let BusinessType::OciImage {
                        path: business_path,
                    } = &self.config.business
                    {
                        builder = builder.with_business_image(business_path);
                    }

                    let has_guest_init = if let Ok(guest_init_path) = Self::find_guest_init() {
                        tracing::info!(
                            guest_init = %guest_init_path.display(),
                            "Using guest init for namespace isolation"
                        );
                        builder = builder.with_guest_init(guest_init_path);

                        if let Ok(nsexec_path) = Self::find_nsexec() {
                            tracing::info!(
                                nsexec = %nsexec_path.display(),
                                "Installing nsexec for business code execution"
                            );
                            builder = builder.with_nsexec(nsexec_path);
                        }

                        true
                    } else {
                        false
                    };

                    builder.build()?;
                    let agent_config = builder.agent_config()?;

                    // Store in cache for next time
                    self.store_rootfs_cache(&cache_key, &rootfs_path, reference);

                    (rootfs_path, Some(agent_config), has_guest_init, true)
                }
            }
            AgentType::A3sCode | AgentType::LocalBinary { .. } | AgentType::RemoteBinary { .. } => {
                // Use default guest-rootfs (must be set up separately)
                let rootfs_path = self.home_dir.join("guest-rootfs");
                (rootfs_path, None, false, false)
            }
        };

        // Generate TEE configuration if enabled
        let tee_instance_config = self.generate_tee_config(&box_dir)?;

        Ok(BoxLayout {
            rootfs_path,
            socket_path: socket_dir.join("grpc.sock"),
            exec_socket_path: socket_dir.join("exec.sock"),
            pty_socket_path: socket_dir.join("pty.sock"),
            attest_socket_path: socket_dir.join("attest.sock"),
            workspace_path,
            skills_path,
            console_output: Some(logs_dir.join("console.log")),
            agent_oci_config,
            has_guest_init,
            tee_instance_config,
            image_at_root,
        })
    }

    /// Try to get a cached rootfs and copy it to the target path.
    ///
    /// Returns `Some(target_path)` if cache hit, `None` if cache miss.
    /// If caching is disabled in config, always returns `None`.
    fn try_rootfs_cache(&self, cache_key: &str, target_path: &Path) -> Result<Option<PathBuf>> {
        if !self.config.cache.enabled {
            return Ok(None);
        }

        let cache_dir = self.resolve_cache_dir().join("rootfs");
        let cache = match RootfsCache::new(&cache_dir) {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(error = %e, "Failed to open rootfs cache, skipping");
                return Ok(None);
            }
        };

        match cache.get(cache_key)? {
            Some(cached_path) => {
                // Copy cached rootfs to target
                crate::cache::layer_cache::copy_dir_recursive(&cached_path, target_path)?;
                Ok(Some(target_path.to_path_buf()))
            }
            None => Ok(None),
        }
    }

    /// Store a built rootfs in the cache for future reuse.
    ///
    /// Errors are logged but not propagated — caching is best-effort.
    fn store_rootfs_cache(&self, cache_key: &str, rootfs_path: &Path, description: &str) {
        if !self.config.cache.enabled {
            return;
        }

        let cache_dir = self.resolve_cache_dir().join("rootfs");
        let cache = match RootfsCache::new(&cache_dir) {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(error = %e, "Failed to open rootfs cache for storing");
                return;
            }
        };

        match cache.put(cache_key, rootfs_path, description) {
            Ok(_) => {
                tracing::debug!(
                    cache_key = %&cache_key[..cache_key.len().min(12)],
                    description = %description,
                    "Stored rootfs in cache"
                );
                // Prune if needed
                if let Err(e) = cache.prune(
                    self.config.cache.max_rootfs_entries,
                    self.config.cache.max_cache_bytes,
                ) {
                    tracing::warn!(error = %e, "Failed to prune rootfs cache");
                }
            }
            Err(e) => {
                tracing::warn!(error = %e, "Failed to store rootfs in cache");
            }
        }
    }

    /// Resolve the cache directory from config or default.
    fn resolve_cache_dir(&self) -> PathBuf {
        self.config
            .cache
            .cache_dir
            .clone()
            .unwrap_or_else(|| self.home_dir.join("cache"))
    }

    /// Generate TEE configuration file if TEE is enabled.
    fn generate_tee_config(&self, box_dir: &Path) -> Result<Option<TeeInstanceConfig>> {
        match &self.config.tee {
            TeeConfig::None => Ok(None),
            TeeConfig::SevSnp {
                workload_id,
                generation,
                simulate,
            } => {
                // In simulation mode, skip hardware check and TEE config
                // (the guest will generate simulated reports via A3S_TEE_SIMULATE env)
                if *simulate {
                    tracing::warn!("TEE simulation mode: skipping hardware check and TEE config");
                    return Ok(None);
                }

                // Verify hardware support
                crate::tee::require_sev_snp_support()?;

                // Generate TEE config JSON
                let config = serde_json::json!({
                    "workload_id": workload_id,
                    "cpus": self.config.resources.vcpus,
                    "ram_mib": self.config.resources.memory_mb,
                    "tee": "snp",
                    "tee_data": format!(r#"{{"gen":"{}"}}"#, generation.as_str()),
                    "attestation_url": ""  // Phase 2: Remote attestation
                });

                let config_path = box_dir.join("tee-config.json");
                std::fs::write(&config_path, serde_json::to_string_pretty(&config)?).map_err(
                    |e| {
                        BoxError::TeeConfig(format!(
                            "Failed to write TEE config to {}: {}",
                            config_path.display(),
                            e
                        ))
                    },
                )?;

                tracing::info!(
                    workload_id = %workload_id,
                    generation = %generation.as_str(),
                    config_path = %config_path.display(),
                    "Generated TEE configuration"
                );

                Ok(Some(TeeInstanceConfig {
                    config_path,
                    tee_type: "snp".to_string(),
                }))
            }
        }
    }

    /// Find the guest init binary in common locations.
    ///
    /// Searches in order:
    /// 1. Same directory as current executable
    /// 2. target/debug or target/release (for development)
    /// 3. PATH
    ///
    /// The binary must be a Linux ELF executable since it runs inside the VM.
    fn find_guest_init() -> Result<PathBuf> {
        let candidates = Self::find_binary_candidates("a3s-box-guest-init");
        for path in candidates {
            if Self::is_linux_elf(&path) {
                return Ok(path);
            }
            tracing::debug!(
                path = %path.display(),
                "Skipping guest init (not a Linux ELF binary)"
            );
        }

        Err(BoxError::BoxBootError {
            message: "Linux guest init binary not found".to_string(),
            hint: Some(
                "Cross-compile with: cargo build -p a3s-box-guest-init --target aarch64-unknown-linux-musl"
                    .to_string(),
            ),
        })
    }

    /// Find the nsexec binary in common locations.
    ///
    /// Searches in order:
    /// 1. Same directory as current executable
    /// 2. target/debug or target/release (for development)
    /// 3. PATH
    ///
    /// The binary must be a Linux ELF executable since it runs inside the VM.
    fn find_nsexec() -> Result<PathBuf> {
        let candidates = Self::find_binary_candidates("a3s-box-nsexec");
        for path in candidates {
            if Self::is_linux_elf(&path) {
                return Ok(path);
            }
            tracing::debug!(
                path = %path.display(),
                "Skipping nsexec (not a Linux ELF binary)"
            );
        }

        Err(BoxError::BoxBootError {
            message: "Linux nsexec binary not found".to_string(),
            hint: Some(
                "Cross-compile with: cargo build -p a3s-box-guest-init --bin a3s-box-nsexec --target aarch64-unknown-linux-musl"
                    .to_string(),
            ),
        })
    }

    /// Search common locations for a binary by name.
    fn find_binary_candidates(name: &str) -> Vec<PathBuf> {
        let mut candidates = Vec::new();

        // Try same directory as current executable
        if let Ok(exe_path) = std::env::current_exe() {
            if let Some(exe_dir) = exe_path.parent() {
                let path = exe_dir.join(name);
                if path.exists() {
                    candidates.push(path);
                }

                // Also search cross-compilation directories relative to the
                // exe's target root. When the exe is at target/debug/a3s-box,
                // cross-compiled guest binaries live at
                // target/aarch64-unknown-linux-musl/{debug,release}/.
                if let Some(target_root) = exe_dir.parent() {
                    let cross_dirs = [
                        "aarch64-unknown-linux-musl/debug",
                        "aarch64-unknown-linux-musl/release",
                        "x86_64-unknown-linux-musl/debug",
                        "x86_64-unknown-linux-musl/release",
                    ];
                    for dir in &cross_dirs {
                        let path = target_root.join(dir).join(name);
                        if path.exists() {
                            candidates.push(path);
                        }
                    }
                }
            }
        }

        // Try cross-compilation target directories relative to CWD (for development)
        let target_dirs = [
            "target/aarch64-unknown-linux-musl/debug",
            "target/aarch64-unknown-linux-musl/release",
            "target/x86_64-unknown-linux-musl/debug",
            "target/x86_64-unknown-linux-musl/release",
            "target/debug",
            "target/release",
        ];
        for dir in &target_dirs {
            let path = PathBuf::from(dir).join(name);
            if path.exists() {
                candidates.push(path);
            }
        }

        // Try PATH
        if let Ok(path_var) = std::env::var("PATH") {
            for dir in std::env::split_paths(&path_var) {
                let path = dir.join(name);
                if path.exists() {
                    candidates.push(path);
                }
            }
        }

        candidates
    }

    /// Check if a file is a Linux ELF binary by reading its magic bytes.
    fn is_linux_elf(path: &std::path::Path) -> bool {
        let Ok(file) = std::fs::File::open(path) else {
            return false;
        };
        use std::io::Read;
        let mut header = [0u8; 18];
        let Ok(_) = (&file).read_exact(&mut header) else {
            return false;
        };
        // ELF magic: 0x7f 'E' 'L' 'F'
        if header[0..4] != [0x7f, b'E', b'L', b'F'] {
            return false;
        }
        // EI_OSABI (byte 7): 0x00 = ELFOSABI_NONE (System V / Linux)
        // or 0x03 = ELFOSABI_LINUX
        matches!(header[7], 0x00 | 0x03)
    }

    /// Build InstanceSpec from config and layout.
    fn build_instance_spec(&mut self, layout: &BoxLayout) -> Result<InstanceSpec> {
        // Build filesystem mounts
        let mut fs_mounts = vec![
            FsMount {
                tag: "workspace".to_string(),
                host_path: layout.workspace_path.clone(),
                read_only: false,
            },
            FsMount {
                tag: "skills".to_string(),
                host_path: layout.skills_path.clone(),
                read_only: true,
            },
        ];

        // Add user-specified volume mounts (-v host:guest or -v host:guest:ro)
        for (i, vol) in self.config.volumes.iter().enumerate() {
            let mount = Self::parse_volume_mount(vol, i)?;
            fs_mounts.push(mount);
        }

        // Auto-create anonymous volumes for OCI VOLUME directives
        let user_guest_paths: std::collections::HashSet<String> = self
            .config
            .volumes
            .iter()
            .filter_map(|v| v.split(':').nth(1).map(String::from))
            .collect();
        let mut anon_vol_offset = self.config.volumes.len();

        if let Some(ref oci_config) = layout.agent_oci_config {
            for vol_path in &oci_config.volumes {
                // Skip if the user already mounted something at this path
                if user_guest_paths.contains(vol_path) {
                    tracing::debug!(
                        path = vol_path,
                        "Skipping anonymous volume — user volume already covers this path"
                    );
                    continue;
                }

                // Generate a deterministic anonymous volume name
                let path_hash = &format!("{:x}", md5_simple(vol_path))[..8];
                let short_box_id = &self.box_id[..8.min(self.box_id.len())];
                let anon_name = format!("anon_{}_{}", short_box_id, path_hash);

                // Create the volume via VolumeStore (best-effort)
                match self.create_anonymous_volume(&anon_name) {
                    Ok(host_path) => {
                        let tag = format!("vol{}", anon_vol_offset);
                        fs_mounts.push(FsMount {
                            tag: tag.clone(),
                            host_path: PathBuf::from(&host_path),
                            read_only: false,
                        });
                        self.anonymous_volumes.push(anon_name);
                        anon_vol_offset += 1;
                        tracing::info!(
                            volume = %tag,
                            guest_path = vol_path,
                            host_path = %host_path,
                            "Created anonymous volume for OCI VOLUME directive"
                        );
                    }
                    Err(e) => {
                        tracing::warn!(
                            path = vol_path,
                            error = %e,
                            "Failed to create anonymous volume, skipping"
                        );
                    }
                }
            }
        }

        // Build entrypoint based on agent type and OCI config
        let mut entrypoint = if layout.has_guest_init {
            // Use guest init as entrypoint for namespace isolation
            // Pass agent configuration via environment variables
            let (agent_exec, agent_args, agent_env) = match &layout.agent_oci_config {
                Some(oci_config) => {
                    let (exec, args) = Self::resolve_oci_entrypoint(
                        oci_config,
                        layout.image_at_root,
                        &self.config.cmd,
                        self.config.entrypoint_override.as_deref(),
                    );
                    (exec, args, oci_config.env.clone())
                }
                None => (
                    GUEST_AGENT_PATH.to_string(),
                    vec![
                        "--listen".to_string(),
                        format!("vsock://{}", AGENT_VSOCK_PORT),
                    ],
                    vec![],
                ),
            };

            // Build environment for guest init
            // Pass args as individual env vars to avoid spaces in values
            // being truncated by libkrun's env serialization
            let mut env: Vec<(String, String)> = vec![
                ("A3S_AGENT_EXEC".to_string(), agent_exec),
                ("A3S_AGENT_ARGC".to_string(), agent_args.len().to_string()),
            ];
            for (i, arg) in agent_args.iter().enumerate() {
                env.push((format!("A3S_AGENT_ARG_{}", i), arg.clone()));
            }

            // Pass the OCI working directory to guest init
            if let Some(ref oci_config) = layout.agent_oci_config {
                if let Some(ref wd) = oci_config.working_dir {
                    env.push(("A3S_AGENT_WORKDIR".to_string(), wd.clone()));
                }
            }

            // Add agent environment variables with A3S_AGENT_ENV_ prefix
            for (key, value) in agent_env {
                env.push((format!("A3S_AGENT_ENV_{}", key), value));
            }

            // Pass user volume mounts to guest init for mounting inside the VM
            // Format: A3S_VOL_<index>=<tag>:<guest_path>[:ro]
            for (i, vol) in self.config.volumes.iter().enumerate() {
                let parts: Vec<&str> = vol.split(':').collect();
                if parts.len() >= 2 {
                    let guest_path = parts[1];
                    let mode = if parts.len() >= 3 && parts[2] == "ro" {
                        ":ro"
                    } else {
                        ""
                    };
                    env.push((
                        format!("A3S_VOL_{}", i),
                        format!("vol{}:{}{}", i, guest_path, mode),
                    ));
                }
            }

            // Pass anonymous volume mounts (from OCI VOLUME directives) to guest init
            if let Some(ref oci_config) = layout.agent_oci_config {
                let mut anon_idx = self.config.volumes.len();
                for vol_path in &oci_config.volumes {
                    if user_guest_paths.contains(vol_path) {
                        continue;
                    }
                    env.push((
                        format!("A3S_VOL_{}", anon_idx),
                        format!("vol{}:{}", anon_idx, vol_path),
                    ));
                    anon_idx += 1;
                }
            }

            // Pass tmpfs mounts to guest init for mounting inside the VM
            // Format: A3S_TMPFS_<index>=<path>[:<options>]
            for (i, tmpfs_spec) in self.config.tmpfs.iter().enumerate() {
                env.push((format!("A3S_TMPFS_{}", i), tmpfs_spec.clone()));
            }

            // Pass security configuration to guest init
            let security_config = a3s_box_core::SecurityConfig::from_options(
                &self.config.security_opt,
                &self.config.cap_add,
                &self.config.cap_drop,
                self.config.privileged,
            );
            env.extend(security_config.to_env_vars());

            tracing::debug!(
                env = ?env,
                "Using guest init with agent configuration"
            );

            Entrypoint {
                executable: "/sbin/init".to_string(),
                args: vec![],
                env,
            }
        } else {
            // Direct agent execution (no namespace isolation)
            match &layout.agent_oci_config {
                Some(oci_config) => {
                    let (executable, args) = Self::resolve_oci_entrypoint(
                        oci_config,
                        layout.image_at_root,
                        &self.config.cmd,
                        self.config.entrypoint_override.as_deref(),
                    );
                    let env = oci_config.env.clone();

                    tracing::debug!(
                        executable = %executable,
                        args = ?args,
                        env_count = env.len(),
                        workdir = ?oci_config.working_dir,
                        "Using OCI image entrypoint"
                    );

                    Entrypoint {
                        executable,
                        args,
                        env,
                    }
                }
                None => {
                    // Use default A3S agent entrypoint
                    // The guest agent listens on vsock for gRPC commands
                    Entrypoint {
                        executable: GUEST_AGENT_PATH.to_string(),
                        args: vec![
                            "--listen".to_string(),
                            format!("vsock://{}", AGENT_VSOCK_PORT),
                        ],
                        env: vec![],
                    }
                }
            }
        };

        // Append user-specified environment variables (-e KEY=VALUE)
        if !self.config.extra_env.is_empty() {
            let mut env = entrypoint.env;
            for (key, value) in &self.config.extra_env {
                // Override existing keys or append new ones
                if let Some(existing) = env.iter_mut().find(|(k, _)| k == key) {
                    existing.1 = value.clone();
                } else {
                    env.push((key.clone(), value.clone()));
                }
            }
            entrypoint.env = env;
        }

        // Inject TEE simulation env var when simulate mode is enabled
        if matches!(self.config.tee, TeeConfig::SevSnp { simulate: true, .. }) {
            entrypoint
                .env
                .push(("A3S_TEE_SIMULATE".to_string(), "1".to_string()));
        }

        // Determine workdir
        let workdir = match &layout.agent_oci_config {
            Some(oci_config) => oci_config
                .working_dir
                .clone()
                .unwrap_or_else(|| GUEST_WORKDIR.to_string()),
            None => GUEST_WORKDIR.to_string(),
        };

        // Extract user from OCI config (USER directive)
        let user = layout
            .agent_oci_config
            .as_ref()
            .and_then(|c| c.user.clone());

        Ok(InstanceSpec {
            box_id: self.box_id.clone(),
            vcpus: self.config.resources.vcpus as u8,
            memory_mib: self.config.resources.memory_mb,
            rootfs_path: layout.rootfs_path.clone(),
            grpc_socket_path: layout.socket_path.clone(),
            exec_socket_path: layout.exec_socket_path.clone(),
            pty_socket_path: layout.pty_socket_path.clone(),
            attest_socket_path: layout.attest_socket_path.clone(),
            fs_mounts,
            entrypoint,
            console_output: layout.console_output.clone(),
            workdir,
            tee_config: layout.tee_instance_config.clone(),
            port_map: self.config.port_map.clone(),
            user,
            network: None, // Network config is set by CLI when --network is specified
            resource_limits: self.config.resource_limits.clone(),
        })
    }

    /// Resolve the executable and args from an OCI image config.
    ///
    /// Follows Docker semantics:
    /// - If `entrypoint_override` is set, it replaces the OCI ENTRYPOINT
    /// - If ENTRYPOINT is set: executable = ENTRYPOINT[0], args = ENTRYPOINT[1:] + CMD
    /// - If only CMD is set: executable = CMD[0], args = CMD[1:]
    /// - If neither: fall back to default agent path
    /// - If `cmd_override` is non-empty, it replaces the OCI CMD
    ///
    /// When `image_at_root` is false, paths are prefixed with `/agent` since the
    /// image is extracted under that directory. When true, paths are used as-is.
    fn resolve_oci_entrypoint(
        oci_config: &OciImageConfig,
        image_at_root: bool,
        cmd_override: &[String],
        entrypoint_override: Option<&[String]>,
    ) -> (String, Vec<String>) {
        let oci_entrypoint = match entrypoint_override {
            Some(ep) => ep,
            None => oci_config.entrypoint.as_deref().unwrap_or(&[]),
        };
        let oci_cmd = if cmd_override.is_empty() {
            oci_config.cmd.as_deref().unwrap_or(&[])
        } else {
            cmd_override
        };

        let maybe_prefix = |path: &str| -> String {
            if image_at_root {
                path.to_string()
            } else {
                Self::prefix_agent_path(path)
            }
        };

        if !oci_entrypoint.is_empty() {
            // ENTRYPOINT is set: use it as executable, CMD as additional args
            let exec = maybe_prefix(&oci_entrypoint[0]);
            let mut args: Vec<String> = oci_entrypoint.iter().skip(1).cloned().collect();
            args.extend(oci_cmd.iter().cloned());
            (exec, args)
        } else if !oci_cmd.is_empty() {
            // Only CMD is set: use CMD[0] as executable, CMD[1:] as args
            let exec = maybe_prefix(&oci_cmd[0]);
            let args: Vec<String> = oci_cmd.iter().skip(1).cloned().collect();
            (exec, args)
        } else {
            // Neither set: fall back to default agent path
            (GUEST_AGENT_PATH.to_string(), vec![])
        }
    }

    /// Prefix a path with /agent to make it relative to the agent rootfs.
    fn prefix_agent_path(path: &str) -> String {
        if path.starts_with('/') {
            format!("/agent{}", path)
        } else {
            format!("/agent/{}", path)
        }
    }

    /// Parse a volume mount string into an FsMount.
    ///
    /// Supported formats:
    /// - `host_path:guest_path` (read-write)
    /// - `host_path:guest_path:ro` (read-only)
    /// - `host_path:guest_path:rw` (read-write, explicit)
    fn parse_volume_mount(volume: &str, index: usize) -> Result<FsMount> {
        let parts: Vec<&str> = volume.split(':').collect();

        let (host_path_str, _guest_path, read_only) = match parts.len() {
            2 => (parts[0], parts[1], false),
            3 => {
                let ro = match parts[2] {
                    "ro" => true,
                    "rw" => false,
                    other => {
                        return Err(BoxError::Other(format!(
                            "Invalid volume mode '{}' (expected 'ro' or 'rw'): {}",
                            other, volume
                        )));
                    }
                };
                (parts[0], parts[1], ro)
            }
            _ => {
                return Err(BoxError::Other(format!(
                    "Invalid volume format (expected host:guest[:ro|rw]): {}",
                    volume
                )));
            }
        };

        // Resolve and validate host path
        let host_path = PathBuf::from(host_path_str);
        if !host_path.exists() {
            std::fs::create_dir_all(&host_path).map_err(|e| BoxError::BoxBootError {
                message: format!(
                    "Failed to create volume host directory {}: {}",
                    host_path.display(),
                    e
                ),
                hint: None,
            })?;
        }
        let host_path = host_path
            .canonicalize()
            .map_err(|e| BoxError::BoxBootError {
                message: format!(
                    "Failed to resolve volume path {}: {}",
                    host_path.display(),
                    e
                ),
                hint: None,
            })?;

        // Use a unique tag for each user volume
        let tag = format!("vol{}", index);

        tracing::info!(
            tag = %tag,
            host = %host_path.display(),
            guest = _guest_path,
            read_only,
            "Adding user volume mount"
        );

        Ok(FsMount {
            tag,
            host_path,
            read_only,
        })
    }

    /// Create an anonymous volume via VolumeStore.
    ///
    /// Returns the host path of the created volume.
    fn create_anonymous_volume(&self, name: &str) -> Result<String> {
        use crate::volume::VolumeStore;

        let store = VolumeStore::default_path()?;

        // If the volume already exists (e.g., from a previous run), reuse it
        if let Some(existing) = store.get(name)? {
            return Ok(existing.mount_point);
        }

        let mut config = a3s_box_core::volume::VolumeConfig::new(name, "");
        config
            .labels
            .insert("anonymous".to_string(), "true".to_string());
        config.attach(&self.box_id);
        let created = store.create(config)?;
        Ok(created.mount_point)
    }

    /// Wait for the VM process to be running (for generic OCI images without an agent).
    ///
    /// Gives the VM a brief moment to start, then verifies the process hasn't exited.
    async fn wait_for_vm_running(&self) -> Result<()> {
        const STABILIZE_MS: u64 = 1000;

        tracing::debug!("Waiting for VM process to stabilize");
        tokio::time::sleep(tokio::time::Duration::from_millis(STABILIZE_MS)).await;

        if let Some(ref handler) = *self.handler.read().await {
            if !handler.is_running() {
                return Err(BoxError::BoxBootError {
                    message: "VM process exited immediately after start".to_string(),
                    hint: Some("Check console output for errors".to_string()),
                });
            }
        }

        tracing::debug!("VM process is running");
        Ok(())
    }

    /// Wait for the guest agent to become ready.
    ///
    /// Phase 1: Wait for the Unix socket file to appear on disk.
    /// Phase 2: Connect via gRPC and perform a health check with retries.
    /// Wait for the agent socket to appear and be connectable.
    ///
    /// This only verifies the agent process has started and is listening.
    /// The actual health check is done via Heartbeat on the exec server.
    async fn wait_for_agent_socket(&mut self, socket_path: &std::path::Path) -> Result<()> {
        const MAX_WAIT_MS: u64 = 30000;
        const POLL_INTERVAL_MS: u64 = 100;

        tracing::debug!(
            socket_path = %socket_path.display(),
            "Waiting for agent socket to appear"
        );

        let start = std::time::Instant::now();

        // Wait for socket file to appear
        loop {
            if start.elapsed().as_millis() >= MAX_WAIT_MS as u128 {
                return Err(BoxError::TimeoutError(
                    "Timed out waiting for agent socket to appear".to_string(),
                ));
            }

            if socket_path.exists() {
                tracing::debug!("Agent socket file detected");
                break;
            }

            // Check if VM is still running
            if let Some(ref handler) = *self.handler.read().await {
                if !handler.is_running() {
                    return Err(BoxError::BoxBootError {
                        message: "VM process exited unexpectedly".to_string(),
                        hint: Some("Check console output for errors".to_string()),
                    });
                }
            }

            tokio::time::sleep(tokio::time::Duration::from_millis(POLL_INTERVAL_MS)).await;
        }

        // Try to connect (stores client for later use)
        let mut last_err = None;
        while start.elapsed().as_millis() < MAX_WAIT_MS as u128 {
            match AgentClient::connect(socket_path).await {
                Ok(client) => {
                    tracing::debug!("Agent socket connectable");
                    self.agent_client = Some(client);
                    return Ok(());
                }
                Err(e) => {
                    tracing::debug!(error = %e, "Agent connect failed, retrying");
                    last_err = Some(e);
                }
            }

            tokio::time::sleep(tokio::time::Duration::from_millis(POLL_INTERVAL_MS)).await;
        }

        Err(BoxError::TimeoutError(format!(
            "Timed out connecting to agent socket (last error: {})",
            last_err
                .map(|e| e.to_string())
                .unwrap_or_else(|| "none".to_string()),
        )))
    }

    /// Wait for the exec server socket to become ready.
    ///
    /// Polls for the socket file to appear, then verifies the exec server
    /// is healthy via a Frame Heartbeat round-trip. This is best-effort:
    /// if the exec socket never appears (e.g., older guest init without
    /// exec server), the VM still boots successfully.
    async fn wait_for_exec_ready(&mut self, exec_socket_path: &std::path::Path) -> Result<()> {
        const MAX_WAIT_MS: u64 = 10000;
        const POLL_INTERVAL_MS: u64 = 200;

        tracing::debug!(
            socket_path = %exec_socket_path.display(),
            "Waiting for exec server socket"
        );

        let start = std::time::Instant::now();

        // Phase 1: Wait for socket file to appear
        loop {
            if start.elapsed().as_millis() >= MAX_WAIT_MS as u128 {
                tracing::warn!("Exec socket did not appear, exec will not be available");
                return Ok(());
            }

            if exec_socket_path.exists() {
                tracing::debug!("Exec socket file detected");
                break;
            }

            // Check if VM is still running
            if let Some(ref handler) = *self.handler.read().await {
                if !handler.is_running() {
                    tracing::warn!("VM exited before exec socket appeared");
                    return Ok(());
                }
            }

            tokio::time::sleep(tokio::time::Duration::from_millis(POLL_INTERVAL_MS)).await;
        }

        // Phase 2: Connect and verify with Heartbeat health check
        while start.elapsed().as_millis() < MAX_WAIT_MS as u128 {
            match ExecClient::connect(exec_socket_path).await {
                Ok(client) => match client.heartbeat().await {
                    Ok(true) => {
                        tracing::debug!("Exec server heartbeat passed");
                        self.exec_client = Some(client);
                        return Ok(());
                    }
                    Ok(false) => {
                        tracing::debug!("Exec server heartbeat failed, retrying");
                    }
                    Err(e) => {
                        tracing::debug!(error = %e, "Exec heartbeat error, retrying");
                    }
                },
                Err(e) => {
                    tracing::debug!(error = %e, "Exec connect failed, retrying");
                }
            }

            tokio::time::sleep(tokio::time::Duration::from_millis(POLL_INTERVAL_MS)).await;
        }

        tracing::warn!("Exec socket appeared but heartbeat failed, exec will not be available");
        Ok(())
    }
}

/// Get the A3S home directory (~/.a3s).
fn dirs_home() -> Option<PathBuf> {
    // Check A3S_HOME environment variable first
    if let Ok(home) = std::env::var("A3S_HOME") {
        return Some(PathBuf::from(home));
    }

    // Fall back to ~/.a3s
    dirs::home_dir().map(|h| h.join(".a3s"))
}

/// Simple FNV-1a hash for generating short deterministic hashes from strings.
fn md5_simple(input: &str) -> u64 {
    let mut hash: u64 = 0xcbf29ce484222325;
    for byte in input.bytes() {
        hash ^= byte as u64;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

/// Parse a MAC address string "02:42:0a:58:00:02" into [u8; 6].
fn parse_mac(mac_str: &str) -> std::result::Result<[u8; 6], String> {
    let parts: Vec<&str> = mac_str.split(':').collect();
    if parts.len() != 6 {
        return Err(format!("expected 6 octets, got {}", parts.len()));
    }

    let mut mac = [0u8; 6];
    for (i, part) in parts.iter().enumerate() {
        mac[i] =
            u8::from_str_radix(part, 16).map_err(|e| format!("invalid octet '{}': {}", part, e))?;
    }
    Ok(mac)
}

/// VM configuration for libkrun (legacy, kept for compatibility).
#[derive(Debug, Clone)]
pub struct VmConfig {
    /// Number of vCPUs
    pub vcpus: u32,

    /// Memory in MB
    pub memory_mb: u32,

    /// Kernel image path
    pub kernel_path: String,

    /// Init command
    pub init_cmd: Vec<String>,
}

impl From<&BoxConfig> for VmConfig {
    fn from(config: &BoxConfig) -> Self {
        Self {
            vcpus: config.resources.vcpus,
            memory_mb: config.resources.memory_mb,
            kernel_path: "/path/to/kernel".to_string(),
            init_cmd: vec![GUEST_AGENT_PATH.to_string()],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_parse_volume_mount_host_guest() {
        let temp = TempDir::new().unwrap();
        let host_path = temp.path().to_str().unwrap();
        let volume = format!("{}:/data", host_path);

        let mount = VmManager::parse_volume_mount(&volume, 0).unwrap();
        assert_eq!(mount.tag, "vol0");
        assert_eq!(mount.host_path, temp.path().canonicalize().unwrap());
        assert!(!mount.read_only);
    }

    #[test]
    fn test_parse_volume_mount_read_only() {
        let temp = TempDir::new().unwrap();
        let host_path = temp.path().to_str().unwrap();
        let volume = format!("{}:/data:ro", host_path);

        let mount = VmManager::parse_volume_mount(&volume, 1).unwrap();
        assert_eq!(mount.tag, "vol1");
        assert!(mount.read_only);
    }

    #[test]
    fn test_parse_volume_mount_explicit_rw() {
        let temp = TempDir::new().unwrap();
        let host_path = temp.path().to_str().unwrap();
        let volume = format!("{}:/data:rw", host_path);

        let mount = VmManager::parse_volume_mount(&volume, 2).unwrap();
        assert_eq!(mount.tag, "vol2");
        assert!(!mount.read_only);
    }

    #[test]
    fn test_parse_volume_mount_invalid_mode() {
        let temp = TempDir::new().unwrap();
        let host_path = temp.path().to_str().unwrap();
        let volume = format!("{}:/data:invalid", host_path);

        let result = VmManager::parse_volume_mount(&volume, 0);
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("Invalid volume mode"));
    }

    #[test]
    fn test_parse_volume_mount_invalid_format() {
        let result = VmManager::parse_volume_mount("invalid", 0);
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("Invalid volume format"));
    }

    #[test]
    fn test_parse_volume_mount_creates_missing_dir() {
        let temp = TempDir::new().unwrap();
        let host_path = temp.path().join("nonexistent");
        let volume = format!("{}:/data", host_path.display());

        assert!(!host_path.exists());
        let mount = VmManager::parse_volume_mount(&volume, 0).unwrap();
        assert!(host_path.exists());
        assert_eq!(mount.host_path, host_path.canonicalize().unwrap());
    }

    #[test]
    fn test_resolve_oci_entrypoint_with_entrypoint_and_cmd() {
        let config = OciImageConfig {
            entrypoint: Some(vec!["/bin/app".to_string()]),
            cmd: Some(vec!["--flag".to_string()]),
            env: vec![],
            working_dir: None,
            user: None,
            exposed_ports: vec![],
            labels: std::collections::HashMap::new(),
            volumes: vec![],
            stop_signal: None,
            health_check: None,
            onbuild: vec![],
        };

        let (exec, args) = VmManager::resolve_oci_entrypoint(&config, true, &[], None);
        assert_eq!(exec, "/bin/app");
        assert_eq!(args, vec!["--flag"]);
    }

    #[test]
    fn test_resolve_oci_entrypoint_cmd_only() {
        let config = OciImageConfig {
            entrypoint: None,
            cmd: Some(vec![
                "/bin/sh".to_string(),
                "-c".to_string(),
                "echo hi".to_string(),
            ]),
            env: vec![],
            working_dir: None,
            user: None,
            exposed_ports: vec![],
            labels: std::collections::HashMap::new(),
            volumes: vec![],
            stop_signal: None,
            health_check: None,
            onbuild: vec![],
        };

        let (exec, args) = VmManager::resolve_oci_entrypoint(&config, true, &[], None);
        assert_eq!(exec, "/bin/sh");
        assert_eq!(args, vec!["-c", "echo hi"]);
    }

    #[test]
    fn test_resolve_oci_entrypoint_neither() {
        let config = OciImageConfig {
            entrypoint: None,
            cmd: None,
            env: vec![],
            working_dir: None,
            user: None,
            exposed_ports: vec![],
            labels: std::collections::HashMap::new(),
            volumes: vec![],
            stop_signal: None,
            health_check: None,
            onbuild: vec![],
        };

        let (exec, _args) = VmManager::resolve_oci_entrypoint(&config, true, &[], None);
        assert_eq!(exec, GUEST_AGENT_PATH);
    }

    #[test]
    fn test_resolve_oci_entrypoint_cmd_override() {
        let config = OciImageConfig {
            entrypoint: None,
            cmd: Some(vec!["/bin/sh".to_string()]),
            env: vec![],
            working_dir: None,
            user: None,
            exposed_ports: vec![],
            labels: std::collections::HashMap::new(),
            volumes: vec![],
            stop_signal: None,
            health_check: None,
            onbuild: vec![],
        };

        let override_cmd = vec!["sleep".to_string(), "3600".to_string()];
        let (exec, args) = VmManager::resolve_oci_entrypoint(&config, true, &override_cmd, None);
        assert_eq!(exec, "sleep");
        assert_eq!(args, vec!["3600"]);
    }

    #[test]
    fn test_resolve_oci_entrypoint_image_not_at_root() {
        let config = OciImageConfig {
            entrypoint: None,
            cmd: Some(vec!["/bin/sh".to_string()]),
            env: vec![],
            working_dir: None,
            user: None,
            exposed_ports: vec![],
            labels: std::collections::HashMap::new(),
            volumes: vec![],
            stop_signal: None,
            health_check: None,
            onbuild: vec![],
        };

        let (exec, _) = VmManager::resolve_oci_entrypoint(&config, false, &[], None);
        assert_eq!(exec, "/agent/bin/sh");
    }

    #[test]
    fn test_resolve_oci_entrypoint_with_override() {
        let config = OciImageConfig {
            entrypoint: Some(vec!["/bin/app".to_string()]),
            cmd: Some(vec!["--flag".to_string()]),
            env: vec![],
            working_dir: None,
            user: None,
            exposed_ports: vec![],
            labels: std::collections::HashMap::new(),
            volumes: vec![],
            stop_signal: None,
            health_check: None,
            onbuild: vec![],
        };

        // Override replaces the image entrypoint entirely
        let override_ep = vec!["/bin/sh".to_string(), "-c".to_string()];
        let (exec, args) =
            VmManager::resolve_oci_entrypoint(&config, true, &[], Some(&override_ep));
        assert_eq!(exec, "/bin/sh");
        // args = entrypoint[1:] + cmd
        assert_eq!(args, vec!["-c", "--flag"]);
    }

    #[test]
    fn test_resolve_oci_entrypoint_override_with_cmd_override() {
        let config = OciImageConfig {
            entrypoint: Some(vec!["/bin/app".to_string()]),
            cmd: Some(vec!["--flag".to_string()]),
            env: vec![],
            working_dir: None,
            user: None,
            exposed_ports: vec![],
            labels: std::collections::HashMap::new(),
            volumes: vec![],
            stop_signal: None,
            health_check: None,
            onbuild: vec![],
        };

        // Both entrypoint and cmd overridden
        let override_ep = vec!["/bin/sh".to_string()];
        let cmd_override = vec!["echo".to_string(), "hello".to_string()];
        let (exec, args) =
            VmManager::resolve_oci_entrypoint(&config, true, &cmd_override, Some(&override_ep));
        assert_eq!(exec, "/bin/sh");
        assert_eq!(args, vec!["echo", "hello"]);
    }

    #[test]
    fn test_prefix_agent_path_absolute() {
        assert_eq!(VmManager::prefix_agent_path("/bin/sh"), "/agent/bin/sh");
    }

    #[test]
    fn test_prefix_agent_path_relative() {
        assert_eq!(VmManager::prefix_agent_path("bin/sh"), "/agent/bin/sh");
    }

    // --- Cache integration tests ---

    fn make_vm_manager_with_home(home_dir: &Path) -> VmManager {
        use a3s_box_core::event::EventEmitter;
        let config = BoxConfig::default();
        let emitter = EventEmitter::new(10);
        VmManager {
            config,
            box_id: "test-box".to_string(),
            state: Arc::new(RwLock::new(BoxState::Created)),
            event_emitter: emitter,
            provider: None,
            handler: Arc::new(RwLock::new(None)),
            agent_client: None,
            exec_client: None,
            passt_manager: None,
            home_dir: home_dir.to_path_buf(),
            anonymous_volumes: Vec::new(),
            tee: None,
            exec_socket_path: None,
            pty_socket_path: None,
            prom: None,
        }
    }

    #[test]
    fn test_resolve_cache_dir_default() {
        let tmp = TempDir::new().unwrap();
        let vm = make_vm_manager_with_home(tmp.path());

        let cache_dir = vm.resolve_cache_dir();
        assert_eq!(cache_dir, tmp.path().join("cache"));
    }

    #[test]
    fn test_resolve_cache_dir_custom() {
        let tmp = TempDir::new().unwrap();
        let mut vm = make_vm_manager_with_home(tmp.path());
        vm.config.cache.cache_dir = Some(PathBuf::from("/custom/cache"));

        let cache_dir = vm.resolve_cache_dir();
        assert_eq!(cache_dir, PathBuf::from("/custom/cache"));
    }

    #[test]
    fn test_try_rootfs_cache_disabled() {
        let tmp = TempDir::new().unwrap();
        let mut vm = make_vm_manager_with_home(tmp.path());
        vm.config.cache.enabled = false;

        let target = tmp.path().join("target");
        let result = vm.try_rootfs_cache("some_key", &target).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn test_try_rootfs_cache_miss() {
        let tmp = TempDir::new().unwrap();
        let vm = make_vm_manager_with_home(tmp.path());

        let target = tmp.path().join("target");
        let result = vm.try_rootfs_cache("nonexistent_key", &target).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn test_try_rootfs_cache_hit() {
        let tmp = TempDir::new().unwrap();
        let vm = make_vm_manager_with_home(tmp.path());

        // Pre-populate the cache
        let cache_dir = tmp.path().join("cache").join("rootfs");
        let cache = RootfsCache::new(&cache_dir).unwrap();
        let source = tmp.path().join("source_rootfs");
        std::fs::create_dir_all(&source).unwrap();
        std::fs::write(source.join("agent.bin"), "binary").unwrap();
        cache.put("test_key", &source, "test").unwrap();

        // Now try_rootfs_cache should hit
        let target = tmp.path().join("target_rootfs");
        let result = vm.try_rootfs_cache("test_key", &target).unwrap();
        assert!(result.is_some());
        assert_eq!(result.unwrap(), target);
        assert!(target.join("agent.bin").is_file());
        assert_eq!(
            std::fs::read_to_string(target.join("agent.bin")).unwrap(),
            "binary"
        );
    }

    #[test]
    fn test_store_rootfs_cache_disabled() {
        let tmp = TempDir::new().unwrap();
        let mut vm = make_vm_manager_with_home(tmp.path());
        vm.config.cache.enabled = false;

        let source = tmp.path().join("rootfs");
        std::fs::create_dir_all(&source).unwrap();
        std::fs::write(source.join("f.txt"), "data").unwrap();

        // Should not store anything
        vm.store_rootfs_cache("key", &source, "test");

        // Cache directory should not even be created
        let cache_dir = tmp.path().join("cache").join("rootfs");
        assert!(!cache_dir.exists());
    }

    #[test]
    fn test_store_rootfs_cache_success() {
        let tmp = TempDir::new().unwrap();
        let vm = make_vm_manager_with_home(tmp.path());

        let source = tmp.path().join("rootfs");
        std::fs::create_dir_all(&source).unwrap();
        std::fs::write(source.join("agent.bin"), "binary").unwrap();

        vm.store_rootfs_cache("store_key", &source, "test image");

        // Verify it was stored
        let cache_dir = tmp.path().join("cache").join("rootfs");
        let cache = RootfsCache::new(&cache_dir).unwrap();
        let result = cache.get("store_key").unwrap();
        assert!(result.is_some());
    }

    #[test]
    fn test_store_rootfs_cache_prunes_on_store() {
        let tmp = TempDir::new().unwrap();
        let mut vm = make_vm_manager_with_home(tmp.path());
        vm.config.cache.max_rootfs_entries = 2;

        let source = tmp.path().join("rootfs");
        std::fs::create_dir_all(&source).unwrap();
        std::fs::write(source.join("f.txt"), "data").unwrap();

        // Store 3 entries (exceeds max_rootfs_entries=2)
        for i in 0..3 {
            vm.store_rootfs_cache(&format!("key{}", i), &source, &format!("entry {}", i));
            std::thread::sleep(std::time::Duration::from_millis(10));
        }

        // After pruning, should have at most 2 entries
        let cache_dir = tmp.path().join("cache").join("rootfs");
        let cache = RootfsCache::new(&cache_dir).unwrap();
        assert!(cache.entry_count().unwrap() <= 2);
    }

    #[tokio::test]
    async fn test_exec_command_rejects_created_state() {
        let tmp = TempDir::new().unwrap();
        let vm = make_vm_manager_with_home(tmp.path());

        let result = vm.exec_command(vec!["echo".to_string()], 0).await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.to_string().contains("not yet booted"));
    }

    #[tokio::test]
    async fn test_exec_command_rejects_stopped_state() {
        let tmp = TempDir::new().unwrap();
        let vm = make_vm_manager_with_home(tmp.path());
        *vm.state.write().await = BoxState::Stopped;

        let result = vm.exec_command(vec!["echo".to_string()], 0).await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.to_string().contains("stopped"));
    }

    #[tokio::test]
    async fn test_exec_command_no_client() {
        let tmp = TempDir::new().unwrap();
        let vm = make_vm_manager_with_home(tmp.path());
        *vm.state.write().await = BoxState::Ready;

        let result = vm.exec_command(vec!["echo".to_string()], 0).await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.to_string().contains("not connected"));
    }

    #[test]
    fn test_try_and_store_roundtrip() {
        let tmp = TempDir::new().unwrap();
        let vm = make_vm_manager_with_home(tmp.path());

        // First call: cache miss
        let target1 = tmp.path().join("target1");
        let result = vm.try_rootfs_cache("roundtrip_key", &target1).unwrap();
        assert!(result.is_none());

        // Build rootfs manually
        let built_rootfs = tmp.path().join("built");
        std::fs::create_dir_all(&built_rootfs).unwrap();
        std::fs::write(built_rootfs.join("init"), "init_binary").unwrap();
        std::fs::create_dir_all(built_rootfs.join("etc")).unwrap();
        std::fs::write(built_rootfs.join("etc/config"), "config_data").unwrap();

        // Store in cache
        vm.store_rootfs_cache("roundtrip_key", &built_rootfs, "roundtrip test");

        // Second call: cache hit
        let target2 = tmp.path().join("target2");
        let result = vm.try_rootfs_cache("roundtrip_key", &target2).unwrap();
        assert!(result.is_some());
        assert!(target2.join("init").is_file());
        assert_eq!(
            std::fs::read_to_string(target2.join("init")).unwrap(),
            "init_binary"
        );
        assert_eq!(
            std::fs::read_to_string(target2.join("etc/config")).unwrap(),
            "config_data"
        );
    }

    #[test]
    fn test_parse_mac_valid() {
        let mac = parse_mac("02:42:0a:58:00:02").unwrap();
        assert_eq!(mac, [0x02, 0x42, 0x0a, 0x58, 0x00, 0x02]);
    }

    #[test]
    fn test_parse_mac_all_zeros() {
        let mac = parse_mac("00:00:00:00:00:00").unwrap();
        assert_eq!(mac, [0, 0, 0, 0, 0, 0]);
    }

    #[test]
    fn test_parse_mac_all_ff() {
        let mac = parse_mac("ff:ff:ff:ff:ff:ff").unwrap();
        assert_eq!(mac, [0xff, 0xff, 0xff, 0xff, 0xff, 0xff]);
    }

    #[test]
    fn test_parse_mac_too_few_octets() {
        assert!(parse_mac("02:42:0a").is_err());
    }

    #[test]
    fn test_parse_mac_too_many_octets() {
        assert!(parse_mac("02:42:0a:58:00:02:ff").is_err());
    }

    #[test]
    fn test_parse_mac_invalid_hex() {
        assert!(parse_mac("02:42:zz:58:00:02").is_err());
    }
}
