//! `a3s-box compose` command — Multi-container orchestration.
//!
//! Subcommands: `up`, `down`, `ps`, `config`.

use std::collections::HashMap;
use std::path::PathBuf;

use a3s_box_core::compose::{ComposeConfig, ServiceConfig};
use a3s_box_core::event::EventEmitter;
use a3s_box_runtime::{ComposeProject, NetworkStore, VmManager};
use clap::{Args, Subcommand};

use super::common;
use crate::state::{BoxRecord, HealthCheck, StateFile};
use crate::status;

/// Label key for compose project name.
const LABEL_PROJECT: &str = "com.a3s.compose.project";
/// Label key for compose service name.
const LABEL_SERVICE: &str = "com.a3s.compose.service";

/// Default compose file names to search for.
const COMPOSE_FILES: &[&str] = &[
    "compose.yaml",
    "compose.yml",
    "docker-compose.yaml",
    "docker-compose.yml",
];

#[derive(Args)]
pub struct ComposeArgs {
    /// Path to compose file (default: compose.yaml or docker-compose.yml)
    #[arg(short = 'f', long = "file")]
    pub file: Option<PathBuf>,

    /// Project name (default: directory name)
    #[arg(short = 'p', long = "project-name")]
    pub project_name: Option<String>,

    #[command(subcommand)]
    pub command: ComposeCommand,
}

#[derive(Subcommand)]
pub enum ComposeCommand {
    /// Create and start all services
    Up(ComposeUpArgs),
    /// Stop and remove all services
    Down(ComposeDownArgs),
    /// List services and their status
    Ps,
    /// Validate and display the compose configuration
    Config,
    /// View logs from all services
    Logs(ComposeLogsArgs),
}

#[derive(Args)]
pub struct ComposeUpArgs {
    /// Run in detached mode (background)
    #[arg(short = 'd', long)]
    pub detach: bool,

    /// Timeout in seconds to wait for healthy dependencies (default: 120)
    #[arg(long, default_value = "120")]
    pub timeout: u64,
}

#[derive(Args)]
pub struct ComposeDownArgs {
    /// Remove named volumes declared in the compose file
    #[arg(short = 'v', long)]
    pub volumes: bool,
}

#[derive(Args)]
pub struct ComposeLogsArgs {
    /// Follow log output
    #[arg(short = 'f', long)]
    pub follow: bool,

    /// Number of lines to show from the end of the logs
    #[arg(long, default_value = "100")]
    pub tail: usize,

    /// Show logs for a specific service only
    pub service: Option<String>,
}

pub async fn execute(args: ComposeArgs) -> Result<(), Box<dyn std::error::Error>> {
    let (compose_path, config) = load_compose_file(args.file.as_deref())?;

    // Derive project name from flag or directory name
    let project_name = args.project_name.unwrap_or_else(|| {
        compose_path
            .parent()
            .and_then(|p| p.file_name())
            .and_then(|n| n.to_str())
            .unwrap_or("default")
            .to_string()
    });

    match args.command {
        ComposeCommand::Up(up_args) => {
            execute_up(&project_name, config, compose_path, up_args).await
        }
        ComposeCommand::Down(down_args) => execute_down(&project_name, down_args).await,
        ComposeCommand::Ps => execute_ps(&project_name).await,
        ComposeCommand::Config => execute_config(&project_name, config),
        ComposeCommand::Logs(logs_args) => execute_logs(&project_name, logs_args).await,
    }
}

/// Find and load the compose file.
fn load_compose_file(
    explicit_path: Option<&std::path::Path>,
) -> Result<(PathBuf, ComposeConfig), Box<dyn std::error::Error>> {
    let path = if let Some(p) = explicit_path {
        if !p.exists() {
            return Err(format!("Compose file not found: {}", p.display()).into());
        }
        p.to_path_buf()
    } else {
        // Search for default compose files in current directory
        let cwd = std::env::current_dir()?;
        COMPOSE_FILES
            .iter()
            .map(|name| cwd.join(name))
            .find(|p| p.exists())
            .ok_or_else(|| {
                format!(
                    "No compose file found. Looked for: {}",
                    COMPOSE_FILES.join(", ")
                )
            })?
    };

    let yaml = std::fs::read_to_string(&path)
        .map_err(|e| format!("Failed to read {}: {}", path.display(), e))?;

    let config = ComposeConfig::from_yaml_str(&yaml)
        .map_err(|e| format!("Failed to parse {}: {}", path.display(), e))?;

    Ok((path, config))
}

fn validate_compose_restart_policies(config: &ComposeConfig) -> Result<(), String> {
    for (service_name, service) in &config.services {
        service_restart_policy(service_name, Some(service))?;
    }
    Ok(())
}

fn service_restart_policy(
    service_name: &str,
    service: Option<&ServiceConfig>,
) -> Result<(String, u32), String> {
    let Some(restart) = service.and_then(|service| service.restart.as_deref()) else {
        return Ok(("no".to_string(), 0));
    };

    crate::state::parse_restart_policy(restart)
        .map_err(|error| format!("Service '{service_name}' has invalid restart policy: {error}"))
}

// ============================================================================
// compose up
// ============================================================================

/// `compose up` — Create networks and start services in dependency order.
///
/// When a service declares `depends_on: { svc: { condition: service_healthy } }`,
/// we wait for the dependency to reach "healthy" status before booting the dependent.
async fn execute_up(
    project_name: &str,
    config: ComposeConfig,
    compose_path: PathBuf,
    up_args: ComposeUpArgs,
) -> Result<(), Box<dyn std::error::Error>> {
    let base_dir = compose_path
        .parent()
        .unwrap_or_else(|| std::path::Path::new("."));
    validate_compose_restart_policies(&config)
        .map_err(|e| -> Box<dyn std::error::Error> { e.into() })?;
    let project = ComposeProject::with_base_dir(project_name, config, base_dir)?;
    let mut state = StateFile::load_default()?;

    // Check for already-active services
    let existing = state.find_by_label(LABEL_PROJECT, project_name);
    let active: Vec<_> = existing
        .iter()
        .filter(|record| status::is_active(record))
        .collect();
    if !active.is_empty() {
        let names: Vec<_> = active
            .iter()
            .filter_map(|r| r.labels.get(LABEL_SERVICE))
            .collect();
        return Err(format!(
            "Project '{}' already has active services: {}. Run `compose down` first.",
            project_name,
            names
                .iter()
                .map(|s| s.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        )
        .into());
    }

    // Step 1: Create networks
    let networks = project.required_networks();
    let net_store = NetworkStore::default_path()?;
    let mut created_networks = Vec::new();
    let mut started_services = Vec::new();
    for (i, net_name) in networks.iter().enumerate() {
        let existing_network = match net_store.get(net_name) {
            Ok(network) => network,
            Err(error) => {
                return rollback_compose_up(
                    &mut state,
                    &started_services,
                    &created_networks,
                    error,
                )
                .await;
            }
        };
        if let Some(config) = existing_network.as_ref() {
            if let Err(error) = super::network::validate_attachable_network(config) {
                return rollback_compose_up(
                    &mut state,
                    &started_services,
                    &created_networks,
                    error,
                )
                .await;
            }
        } else {
            let subnet = format!("10.89.{}.0/24", 100 + i);
            let config = match a3s_box_core::network::NetworkConfig::new(net_name, &subnet) {
                Ok(config) => config,
                Err(error) => {
                    return rollback_compose_up(
                        &mut state,
                        &started_services,
                        &created_networks,
                        format!("Failed to create network '{}': {}", net_name, error),
                    )
                    .await;
                }
            };
            if let Err(error) = super::network::validate_attachable_network(&config) {
                return rollback_compose_up(
                    &mut state,
                    &started_services,
                    &created_networks,
                    error,
                )
                .await;
            }
            if let Err(error) = net_store.create(config) {
                return rollback_compose_up(
                    &mut state,
                    &started_services,
                    &created_networks,
                    error,
                )
                .await;
            }
            created_networks.push(net_name.clone());
            println!("  [+] Network {} ({})", net_name, subnet);
        }
    }

    // Step 2: Boot services in dependency order
    let default_net = project.default_network_name();
    let home = a3s_box_core::dirs_home();

    println!(
        "Starting project '{}' ({} services)...",
        project_name,
        project.service_order.len()
    );

    for svc_name in &project.service_order {
        // Wait for healthy dependencies before booting this service
        let health_deps = project.health_wait_deps(svc_name);
        if !health_deps.is_empty() {
            print!(
                "  [~] Waiting for {} to be healthy...",
                health_deps.join(", ")
            );
            if let Err(error) = wait_for_healthy(project_name, &health_deps, up_args.timeout).await
            {
                return rollback_compose_up(
                    &mut state,
                    &started_services,
                    &created_networks,
                    error,
                )
                .await;
            }
            println!(" ✓");
        }

        // Wait for dependencies that must run to completion (exit 0) first.
        let completed_deps = project.completed_wait_deps(svc_name);
        if !completed_deps.is_empty() {
            print!(
                "  [~] Waiting for {} to complete...",
                completed_deps.join(", ")
            );
            if let Err(error) =
                wait_for_completed(project_name, &completed_deps, up_args.timeout).await
            {
                return rollback_compose_up(
                    &mut state,
                    &started_services,
                    &created_networks,
                    error,
                )
                .await;
            }
            println!(" ✓");
        }

        let mut box_config = match project.build_box_config(svc_name, Some(&default_net)) {
            Ok(config) => config,
            Err(error) => {
                return rollback_compose_up(
                    &mut state,
                    &started_services,
                    &created_networks,
                    error,
                )
                .await;
            }
        };
        let (resolved_volumes, volume_names) = match resolve_service_volumes(&box_config.volumes) {
            Ok(volumes) => volumes,
            Err(error) => {
                return rollback_compose_up(
                    &mut state,
                    &started_services,
                    &created_networks,
                    error,
                )
                .await;
            }
        };
        box_config.volumes = resolved_volumes.clone();
        let image = box_config.image.clone();
        let record_env: HashMap<String, String> = box_config.extra_env.iter().cloned().collect();
        let record_hostname = box_config.hostname.clone();
        let record_add_hosts = box_config.add_hosts.clone();
        let network_mode = box_config.network.clone();
        let network_name = match &network_mode {
            a3s_box_core::NetworkMode::Bridge { network } => Some(network.clone()),
            _ => None,
        };

        // Create VmManager and boot
        let emitter = EventEmitter::new(256);
        let box_name = format!("{}-{}", project_name, svc_name);
        let mut vm = VmManager::new(box_config, emitter);
        let box_id = vm.box_id().to_string();
        let box_dir = home.join("boxes").join(&box_id);

        // Create box directory structure
        if let Err(error) = std::fs::create_dir_all(box_dir.join("sockets")) {
            return rollback_compose_up(&mut state, &started_services, &created_networks, error)
                .await;
        }
        if let Err(error) = std::fs::create_dir_all(box_dir.join("logs")) {
            return rollback_compose_up(&mut state, &started_services, &created_networks, error)
                .await;
        }

        // Connect to network before boot
        if let Some(net_name) = network_name.as_deref() {
            let mut net_config = match net_store.get(net_name) {
                Ok(Some(config)) => config,
                Ok(None) => {
                    return rollback_compose_up(
                        &mut state,
                        &started_services,
                        &created_networks,
                        format!("Compose network '{}' was not created", net_name),
                    )
                    .await;
                }
                Err(error) => {
                    return rollback_compose_up(
                        &mut state,
                        &started_services,
                        &created_networks,
                        error,
                    )
                    .await;
                }
            };
            if let Err(error) = super::network::validate_attachable_network(&net_config) {
                return rollback_compose_up(
                    &mut state,
                    &started_services,
                    &created_networks,
                    error,
                )
                .await;
            }
            // Register the bare service name as a DNS alias so peers can reach
            // this service as `svc` (Docker Compose behavior), not only as the
            // `{project}-{svc}` box name.
            let endpoint = match net_config.connect_with_aliases(
                &box_id,
                &box_name,
                std::slice::from_ref(svc_name),
            ) {
                Ok(endpoint) => endpoint,
                Err(error) => {
                    return rollback_compose_up(
                        &mut state,
                        &started_services,
                        &created_networks,
                        format!(
                            "Failed to connect service '{}' to network: {error}",
                            svc_name
                        ),
                    )
                    .await;
                }
            };
            if let Err(error) = net_store.update(&net_config) {
                return rollback_compose_up(
                    &mut state,
                    &started_services,
                    &created_networks,
                    error,
                )
                .await;
            }
            print!(
                "  [+] {} (image={}, ip={})",
                svc_name, image, endpoint.ip_address
            );
        }

        if let Err(e) = vm.boot().await {
            crate::cleanup::cleanup_box_resources(&box_id, &volume_names, network_name.as_deref());
            crate::cleanup::cleanup_external_socket_dir(
                &box_dir,
                &box_dir.join("sockets/exec.sock"),
            );
            // Release the overlay mount before deleting the box dir, else
            // remove_dir_all recurses into the live mount ("Stale file handle")
            // and leaks it — the same class as the rm/restart leak fixed in #33,
            // which cleanup_box_resources (volumes + network only) does not cover.
            a3s_box_runtime::rootfs::unmount_box_overlay(&box_dir.join("merged"));
            let _ = std::fs::remove_dir_all(&box_dir);
            return rollback_compose_up(
                &mut state,
                &started_services,
                &created_networks,
                format!("Failed to start service '{}': {}", svc_name, e),
            )
            .await;
        }

        let pid = vm.pid().await;
        let exec_socket_path = vm
            .exec_socket_path()
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| box_dir.join("sockets").join("exec.sock"));
        let anonymous_volumes = vm.anonymous_volumes().to_vec();
        let image_health_check = vm
            .image_config()
            .and_then(|config| config.health_check.clone());
        let image_stop_signal = vm
            .image_config()
            .and_then(|config| config.stop_signal.clone());

        // Build labels with compose metadata
        let svc = project.config.services.get(svc_name);
        let mut labels = svc.map(|s| s.labels.to_map()).unwrap_or_default();
        labels.insert(LABEL_PROJECT.to_string(), project_name.to_string());
        labels.insert(LABEL_SERVICE.to_string(), svc_name.to_string());

        // Get service config for extra fields
        let port_map: Vec<String> = svc.map(|s| s.ports.clone()).unwrap_or_default();

        // Compose healthcheck overrides image HEALTHCHECK; disable blocks fallback.
        let service_health_check = project.healthcheck(svc_name).map(|hc| HealthCheck {
            cmd: hc.cmd,
            interval_secs: hc.interval_secs,
            timeout_secs: hc.timeout_secs,
            retries: hc.retries,
            start_period_secs: hc.start_period_secs,
        });
        let healthcheck_disabled = project.healthcheck_disabled(svc_name);
        let health_check = if healthcheck_disabled {
            None
        } else {
            service_health_check.or_else(|| {
                image_health_check
                    .as_ref()
                    .and_then(common::health_check_from_oci)
            })
        };

        let health_status = if health_check.is_some() {
            "starting".to_string()
        } else {
            "none".to_string()
        };
        let (restart_policy, max_restart_count) = service_restart_policy(svc_name, svc)
            .map_err(|e| -> Box<dyn std::error::Error> { e.into() })?;

        let record = BoxRecord {
            id: box_id.clone(),
            short_id: BoxRecord::make_short_id(&box_id),
            name: box_name,
            image,
            status: "running".to_string(),
            pid,
            cpus: svc.and_then(|s| s.cpus).unwrap_or(2),
            memory_mb: svc
                .and_then(|s| s.mem_limit.as_ref())
                .and_then(|m| crate::output::parse_memory(m).ok())
                .unwrap_or(512),
            volumes: resolved_volumes,
            env: record_env,
            cmd: svc
                .and_then(|s| s.command.as_ref())
                .map(|c| c.to_vec())
                .unwrap_or_default(),
            entrypoint: svc.and_then(|s| s.entrypoint.as_ref()).map(|e| e.to_vec()),
            box_dir: box_dir.clone(),
            exec_socket_path: exec_socket_path.clone(),
            console_log: box_dir.join("logs").join("console.log"),
            created_at: chrono::Utc::now(),
            started_at: Some(chrono::Utc::now()),
            auto_remove: false,
            hostname: record_hostname,
            user: None,
            workdir: svc.and_then(|s| s.working_dir.clone()),
            restart_policy,
            port_map,
            labels,
            stopped_by_user: false,
            restart_count: 0,
            max_restart_count,
            exit_code: None,
            health_check: health_check.clone(),
            healthcheck_disabled,
            health_status,
            health_retries: 0,
            health_last_check: None,
            network_mode,
            network_name: network_name.clone(),
            volume_names: volume_names.clone(),
            tmpfs: svc.map(|s| s.tmpfs.to_vec()).unwrap_or_default(),
            anonymous_volumes,
            resource_limits: Default::default(),
            log_config: Default::default(),
            add_host: record_add_hosts,
            platform: None,
            init: false,
            read_only: false,
            cap_add: svc.map(|s| s.cap_add.clone()).unwrap_or_default(),
            cap_drop: svc.map(|s| s.cap_drop.clone()).unwrap_or_default(),
            security_opt: vec![],
            privileged: svc.map(|s| s.privileged).unwrap_or(false),
            devices: vec![],
            gpus: None,
            shm_size: None,
            stop_signal: image_stop_signal,
            stop_timeout: None,
            oom_kill_disable: false,
            oom_score_adj: None,
        };

        let service_box = ServiceBox::from_record(&record);
        // Health checks run concurrently while `compose up` waits for later
        // services. Reload before appending so we do not overwrite dependency
        // health transitions captured by the checker.
        state = match StateFile::load_default() {
            Ok(state) => state,
            Err(error) => {
                let rollback_services = rollback_with_current(&started_services, service_box);
                return rollback_compose_up(
                    &mut state,
                    &rollback_services,
                    &created_networks,
                    error,
                )
                .await;
            }
        };
        // Atomic append under the state lock (load-fresh + push + save): a plain
        // state.add() saved a snapshot loaded before concurrent health/sibling
        // writes, clobbering them (the lost-registration → orphan-VM race).
        if let Err(error) = StateFile::add_record(record) {
            let rollback_services = rollback_with_current(&started_services, service_box);
            return rollback_compose_up(&mut state, &rollback_services, &created_networks, error)
                .await;
        }
        if let Err(error) = super::volume::attach_volumes(&volume_names, &box_id) {
            let rollback_services = rollback_with_current(&started_services, service_box);
            return rollback_compose_up(&mut state, &rollback_services, &created_networks, error)
                .await;
        }
        started_services.push(service_box);

        // Spawn health checker if configured
        if let Some(ref hc) = health_check {
            crate::health::spawn_health_checker(
                box_id.clone(),
                exec_socket_path.clone(),
                hc.clone(),
            );
        }

        // Ensure the log dir exists; the shim runs the log processor (default
        // json-file driver) for each service box's lifetime.
        let _ = std::fs::create_dir_all(box_dir.join("logs"));

        println!(" ✓");
    }

    println!("All {} services started.", project.service_order.len());
    Ok(())
}

fn resolve_service_volumes(
    volume_specs: &[String],
) -> Result<(Vec<String>, Vec<String>), Box<dyn std::error::Error>> {
    let mut resolved = Vec::new();
    let mut names = Vec::new();

    for spec in volume_specs {
        let (resolved_spec, volume_name) = super::volume::resolve_named_volume(spec)?;
        if let Some(name) = volume_name {
            names.push(name);
        }
        resolved.push(resolved_spec);
    }

    Ok((resolved, names))
}

/// Wait for all named services to reach "healthy" status in the state file.
///
/// Polls the state file every 2 seconds until all services are healthy or timeout.
async fn wait_for_healthy(
    project_name: &str,
    service_names: &[String],
    timeout_secs: u64,
) -> Result<(), Box<dyn std::error::Error>> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(timeout_secs);

    loop {
        if std::time::Instant::now() > deadline {
            return Err(format!(
                "Timed out waiting for services to become healthy: {}",
                service_names.join(", ")
            )
            .into());
        }

        let state = StateFile::load_default()?;
        let all_healthy = service_names.iter().all(|svc_name| {
            // Find the box for this service by label
            state
                .find_by_label(LABEL_SERVICE, svc_name)
                .iter()
                .any(|r| {
                    r.labels.get(LABEL_PROJECT).map(String::as_str) == Some(project_name)
                        && r.health_status == "healthy"
                })
        });

        if all_healthy {
            return Ok(());
        }

        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    }
}

/// Wait for dependency services to run to completion (Docker's
/// `service_completed_successfully`).
///
/// A dependency is "completed" once it is no longer active — preferring the
/// record's terminal status (set by the monitor) and falling back to shim-PID
/// liveness for the daemonless case. If an exit code was recorded and is
/// non-zero, the dependency failed and the wait errors.
async fn wait_for_completed(
    project_name: &str,
    service_names: &[String],
    timeout_secs: u64,
) -> Result<(), Box<dyn std::error::Error>> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(timeout_secs);

    loop {
        if std::time::Instant::now() > deadline {
            return Err(format!(
                "Timed out waiting for services to complete: {}",
                service_names.join(", ")
            )
            .into());
        }

        let state = StateFile::load_default()?;
        let mut all_done = true;
        for svc_name in service_names {
            let records = state.find_by_label(LABEL_SERVICE, svc_name);
            let Some(record) = records
                .iter()
                .find(|r| r.labels.get(LABEL_PROJECT).map(String::as_str) == Some(project_name))
            else {
                all_done = false;
                continue;
            };

            // A detached box's shim becomes a zombie under this process when its
            // VM halts; is_process_exited is zombie-aware (is_process_alive /
            // kill(pid,0) is not), so a completed dependency is detected.
            let exited = !status::is_active(record)
                || record
                    .pid
                    .map(crate::process::is_process_exited)
                    .unwrap_or(true);
            if !exited {
                all_done = false;
                continue;
            }

            if let Some(code) = record.exit_code {
                if code != 0 {
                    return Err(format!(
                        "dependency service '{}' did not complete successfully (exit code {})",
                        svc_name, code
                    )
                    .into());
                }
            }
        }

        if all_done {
            return Ok(());
        }

        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    }
}

// ============================================================================
// compose down
// ============================================================================

/// Snapshot of a compose service box for the `down` operation.
#[derive(Clone)]
struct ServiceBox {
    box_id: String,
    svc_name: String,
    pid: Option<u32>,
    status: String,
    box_dir: PathBuf,
    exec_socket_path: PathBuf,
    network_name: Option<String>,
    volume_names: Vec<String>,
    anonymous_volumes: Vec<String>,
    stop_signal: Option<String>,
    stop_timeout: Option<u64>,
}

impl ServiceBox {
    fn from_record(record: &BoxRecord) -> Self {
        Self {
            box_id: record.id.clone(),
            svc_name: record
                .labels
                .get(LABEL_SERVICE)
                .cloned()
                .unwrap_or_default(),
            pid: record.pid,
            status: record.status.clone(),
            box_dir: record.box_dir.clone(),
            exec_socket_path: record.exec_socket_path.clone(),
            network_name: crate::cleanup::record_network_name(record).map(str::to_string),
            volume_names: record.volume_names.clone(),
            anonymous_volumes: record.anonymous_volumes.clone(),
            stop_signal: record.stop_signal.clone(),
            stop_timeout: record.stop_timeout,
        }
    }

    fn is_active(&self) -> bool {
        status::is_active_status(&self.status)
    }
}

fn cleanup_service_box(svc: &ServiceBox) {
    crate::cleanup::cleanup_box_resources(
        &svc.box_id,
        &svc.volume_names,
        svc.network_name.as_deref(),
    );
    crate::cleanup::cleanup_anonymous_volumes(&svc.anonymous_volumes);
    // Release the overlay mount before deleting the box dir, else remove_dir_all
    // recurses into the live mount ("Stale file handle") and leaks it (#33).
    // cleanup_box_resources above only detaches volumes + network, not the mount.
    a3s_box_runtime::rootfs::unmount_box_overlay(&svc.box_dir.join("merged"));
    let _ = std::fs::remove_dir_all(&svc.box_dir);
    crate::cleanup::cleanup_external_socket_dir(&svc.box_dir, &svc.exec_socket_path);
}

fn rollback_with_current(started_services: &[ServiceBox], current: ServiceBox) -> Vec<ServiceBox> {
    let mut rollback_services = started_services.to_vec();
    rollback_services.push(current);
    rollback_services
}

async fn rollback_compose_up<T>(
    state: &mut StateFile,
    started_services: &[ServiceBox],
    created_networks: &[String],
    error: impl Into<Box<dyn std::error::Error>>,
) -> Result<T, Box<dyn std::error::Error>> {
    rollback_started_services(state, started_services).await;
    cleanup_created_networks(created_networks);
    Err(error.into())
}

async fn rollback_started_services(state: &mut StateFile, started_services: &[ServiceBox]) {
    if started_services.is_empty() {
        return;
    }

    eprintln!(
        "  [!] Rolling back {} started service(s)...",
        started_services.len()
    );

    for svc in started_services.iter().rev() {
        stop_service_process(svc).await;

        cleanup_service_box(svc);
        let _ = state.remove(&svc.box_id);
    }
}

async fn stop_service_process(svc: &ServiceBox) {
    if !svc.is_active() {
        return;
    }

    let Some(pid) = svc.pid else {
        eprintln!(
            "  Warning: service {} is {} but has no recorded PID; removing stale service state.",
            svc.svc_name, svc.status
        );
        return;
    };

    if svc.status == "paused" {
        #[cfg(unix)]
        if let Err(error) = crate::process::send_signal(pid, libc::SIGCONT) {
            eprintln!(
                "  Warning: failed to resume paused service {} before stopping: {}",
                svc.svc_name, error
            );
        }
    }

    let stop_signal = svc
        .stop_signal
        .as_deref()
        .map(a3s_box_core::vmm::parse_signal_name)
        .unwrap_or(libc::SIGTERM);
    let stop_timeout = svc.stop_timeout.unwrap_or(10);
    let exec_socket = if svc.exec_socket_path.as_os_str().is_empty() {
        svc.box_dir.join("sockets").join("exec.sock")
    } else {
        svc.exec_socket_path.clone()
    };
    crate::process::graceful_stop_via_guest(pid, &exec_socket, stop_signal, stop_timeout).await;
}

fn cleanup_created_networks(created_networks: &[String]) {
    if created_networks.is_empty() {
        return;
    }

    let Ok(net_store) = NetworkStore::default_path() else {
        return;
    };

    for net_name in created_networks.iter().rev() {
        if let Ok(Some(mut net_config)) = net_store.get(net_name) {
            let endpoint_ids: Vec<_> = net_config.endpoints.keys().cloned().collect();
            for endpoint_id in endpoint_ids {
                let _ = net_config.disconnect(&endpoint_id);
            }
            let _ = net_store.update(&net_config);
        }

        if let Err(error) = net_store.remove(net_name) {
            eprintln!(
                "  Warning: failed to roll back network {}: {}",
                net_name, error
            );
        }
    }
}

/// `compose down` — Stop and remove all services, networks, and optionally volumes.
async fn execute_down(
    project_name: &str,
    down_args: ComposeDownArgs,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut state = StateFile::load_default()?;

    // Find all boxes belonging to this project
    let project_boxes: Vec<ServiceBox> = state
        .find_by_label(LABEL_PROJECT, project_name)
        .iter()
        .map(|r| ServiceBox::from_record(r))
        .collect();

    if project_boxes.is_empty() {
        println!("No services found for project '{}'.", project_name);
        return Ok(());
    }

    println!(
        "Stopping project '{}' ({} services)...",
        project_name,
        project_boxes.len()
    );

    // Stop in reverse order (last started = first stopped)
    for svc in project_boxes.iter().rev() {
        print!("  [-] Stopping {}...", svc.svc_name);

        stop_service_process(svc).await;

        cleanup_service_box(svc);
        state.remove(&svc.box_id)?;

        println!(" ✓");
    }

    // Clean up networks
    if let Ok(net_store) = NetworkStore::default_path() {
        let prefix = format!("{}_", project_name);
        if let Ok(all_nets) = net_store.list() {
            for net in all_nets {
                if net.name.starts_with(&prefix) {
                    // Disconnect any remaining endpoints first
                    if !net.endpoints.is_empty() {
                        let mut net_config = net.clone();
                        let ids: Vec<_> = net_config.endpoints.keys().cloned().collect();
                        for id in ids {
                            net_config.disconnect(&id).ok();
                        }
                        let _ = net_store.update(&net_config);
                    }
                    if let Err(e) = net_store.remove(&net.name) {
                        eprintln!("  Warning: failed to remove network {}: {}", net.name, e);
                    } else {
                        println!("  [-] Network {} removed", net.name);
                    }
                }
            }
        }
    }

    // Optionally remove named volumes
    if down_args.volumes {
        let vol_store = a3s_box_runtime::volume::VolumeStore::default_path()?;
        let mut removed = 0u32;
        for svc in &project_boxes {
            for vol_name in &svc.volume_names {
                match vol_store.remove(vol_name, true) {
                    Ok(_) => {
                        println!("  [-] Volume {} removed", vol_name);
                        removed += 1;
                    }
                    Err(e) => {
                        eprintln!("  Warning: failed to remove volume {}: {}", vol_name, e);
                    }
                }
            }
        }
        if removed > 0 {
            println!("  Removed {} volume(s).", removed);
        }
    }

    println!("Project '{}' stopped.", project_name);
    Ok(())
}

// ============================================================================
// compose ps
// ============================================================================

/// `compose ps` — List services and their actual status.
async fn execute_ps(project_name: &str) -> Result<(), Box<dyn std::error::Error>> {
    let state = StateFile::load_default()?;
    let boxes = state.find_by_label(LABEL_PROJECT, project_name);

    if boxes.is_empty() {
        println!("No services found for project '{}'.", project_name);
        return Ok(());
    }

    println!(
        "{:<20} {:<30} {:<12} {:<12} {:<10}",
        "SERVICE", "IMAGE", "STATUS", "HEALTH", "PID"
    );
    println!("{}", "-".repeat(84));

    for record in &boxes {
        let svc_name = record
            .labels
            .get(LABEL_SERVICE)
            .map(|s| s.as_str())
            .unwrap_or("?");
        let pid_str = record
            .pid
            .map(|p| p.to_string())
            .unwrap_or_else(|| "-".to_string());
        println!(
            "{:<20} {:<30} {:<12} {:<12} {:<10}",
            svc_name, record.image, record.status, record.health_status, pid_str
        );
    }

    Ok(())
}

// ============================================================================
// compose config
// ============================================================================

/// `compose config` — Validate and display the parsed compose configuration.
fn execute_config(
    project_name: &str,
    config: ComposeConfig,
) -> Result<(), Box<dyn std::error::Error>> {
    validate_compose_restart_policies(&config)
        .map_err(|e| -> Box<dyn std::error::Error> { e.into() })?;
    let project = ComposeProject::new(project_name, config)?;

    println!("Project: {}", project_name);
    println!("Services: {}", project.config.services.len());
    println!("Networks: {}", project.required_networks().len());
    println!("Volumes: {}", project.config.volumes.len());
    println!("\nBoot order: {}", project.service_order.join(" → "));

    for svc_name in &project.service_order {
        if let Some(svc) = project.config.services.get(svc_name) {
            println!("\n[{}]", svc_name);
            if let Some(ref img) = svc.image {
                println!("  image: {}", img);
            }
            if !svc.ports.is_empty() {
                println!("  ports: {}", svc.ports.join(", "));
            }
            if !svc.volumes.is_empty() {
                println!("  volumes: {}", svc.volumes.join(", "));
            }
            let deps = svc.depends_on.services();
            if !deps.is_empty() {
                println!("  depends_on: {}", deps.join(", "));
            }
            let env = svc.environment.to_pairs();
            if !env.is_empty() {
                println!("  environment:");
                for (k, v) in &env {
                    println!("    {}={}", k, v);
                }
            }
        }
    }

    println!("\nConfiguration is valid.");
    Ok(())
}

// ============================================================================
// compose logs
// ============================================================================

/// `compose logs` — View logs from all (or one) service in the project.
async fn execute_logs(
    project_name: &str,
    logs_args: ComposeLogsArgs,
) -> Result<(), Box<dyn std::error::Error>> {
    let state = StateFile::load_default()?;
    let boxes = state.find_by_label(LABEL_PROJECT, project_name);

    if boxes.is_empty() {
        println!("No services found for project '{}'.", project_name);
        return Ok(());
    }

    // Filter to specific service if requested
    let targets: Vec<_> = if let Some(ref svc) = logs_args.service {
        boxes
            .iter()
            .filter(|r| {
                r.labels
                    .get(LABEL_SERVICE)
                    .map(|s| s == svc)
                    .unwrap_or(false)
            })
            .collect()
    } else {
        boxes.iter().collect()
    };

    if targets.is_empty() {
        if let Some(ref svc) = logs_args.service {
            return Err(
                format!("Service '{}' not found in project '{}'.", svc, project_name).into(),
            );
        }
    }

    for record in &targets {
        let svc_name = record
            .labels
            .get(LABEL_SERVICE)
            .map(|s| s.as_str())
            .unwrap_or("?");

        let log_path = record.console_log.clone();
        if !log_path.exists() {
            println!("[{}] (no logs)", svc_name);
            continue;
        }

        let content = std::fs::read_to_string(&log_path)
            .map_err(|e| format!("Failed to read logs for {}: {}", svc_name, e))?;

        let lines: Vec<&str> = content.lines().collect();
        let start = lines.len().saturating_sub(logs_args.tail);
        let prefix = if targets.len() > 1 {
            format!("{} | ", svc_name)
        } else {
            String::new()
        };

        for line in &lines[start..] {
            println!("{}{}", prefix, line);
        }
    }

    if logs_args.follow {
        println!("(follow mode: use Ctrl-C to stop)");
        // In follow mode, tail all log files concurrently
        // For simplicity, we poll every second
        let mut last_sizes: HashMap<String, u64> = HashMap::new();
        for record in &targets {
            let size = std::fs::metadata(&record.console_log)
                .map(|m| m.len())
                .unwrap_or(0);
            last_sizes.insert(record.id.clone(), size);
        }

        loop {
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;

            for record in &targets {
                let log_path = &record.console_log;
                let current_size = std::fs::metadata(log_path).map(|m| m.len()).unwrap_or(0);
                let last_size = last_sizes.get(&record.id).copied().unwrap_or(0);

                if current_size > last_size {
                    let svc_name = record
                        .labels
                        .get(LABEL_SERVICE)
                        .map(|s| s.as_str())
                        .unwrap_or("?");
                    let prefix = if targets.len() > 1 {
                        format!("{} | ", svc_name)
                    } else {
                        String::new()
                    };

                    if let Ok(file) = std::fs::File::open(log_path) {
                        use std::io::{Read, Seek, SeekFrom};
                        let mut file = file;
                        if file.seek(SeekFrom::Start(last_size)).is_ok() {
                            let mut buf = String::new();
                            if file.read_to_string(&mut buf).is_ok() {
                                for line in buf.lines() {
                                    println!("{}{}", prefix, line);
                                }
                            }
                        }
                    }

                    last_sizes.insert(record.id.clone(), current_size);
                }
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_load_compose_file_not_found() {
        let result = load_compose_file(Some(std::path::Path::new("/nonexistent/compose.yaml")));
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("not found"));
    }

    #[test]
    fn test_compose_files_constant() {
        assert_eq!(COMPOSE_FILES.len(), 4);
        assert!(COMPOSE_FILES.contains(&"compose.yaml"));
        assert!(COMPOSE_FILES.contains(&"docker-compose.yml"));
    }

    #[test]
    fn test_label_constants() {
        assert_eq!(LABEL_PROJECT, "com.a3s.compose.project");
        assert_eq!(LABEL_SERVICE, "com.a3s.compose.service");
    }

    #[test]
    fn test_service_restart_policy_normalizes_on_failure_limit() {
        let service = ServiceConfig {
            restart: Some("on-failure:3".to_string()),
            ..Default::default()
        };

        let (policy, max_count) = service_restart_policy("web", Some(&service)).unwrap();

        assert_eq!(policy, "on-failure");
        assert_eq!(max_count, 3);
    }

    #[test]
    fn test_validate_compose_restart_policies_rejects_invalid_service_policy() {
        let mut services = HashMap::new();
        services.insert(
            "web".to_string(),
            ServiceConfig {
                image: Some("docker.io/library/alpine:latest".to_string()),
                restart: Some("never".to_string()),
                ..Default::default()
            },
        );
        let config = ComposeConfig {
            version: None,
            services,
            volumes: HashMap::new(),
            networks: HashMap::new(),
        };

        let error = validate_compose_restart_policies(&config).unwrap_err();

        assert!(error.contains("Service 'web' has invalid restart policy"));
        assert!(error.contains("Invalid restart policy"));
    }

    #[test]
    fn test_service_box_from_record_captures_cleanup_fields() {
        let mut record = crate::test_helpers::fixtures::make_record(
            "compose-id",
            "project-web",
            "running",
            Some(123),
        );
        record
            .labels
            .insert(LABEL_SERVICE.to_string(), "web".to_string());
        record.network_name = Some("project_default".to_string());
        record.volume_names = vec!["data".to_string()];
        record.anonymous_volumes = vec!["anon".to_string()];
        record.stop_signal = Some("SIGINT".to_string());
        record.stop_timeout = Some(3);

        let service = ServiceBox::from_record(&record);

        assert_eq!(service.box_id, "compose-id");
        assert_eq!(service.svc_name, "web");
        assert_eq!(service.pid, Some(123));
        assert_eq!(service.network_name.as_deref(), Some("project_default"));
        assert_eq!(service.volume_names, vec!["data".to_string()]);
        assert_eq!(service.anonymous_volumes, vec!["anon".to_string()]);
        assert_eq!(service.stop_signal.as_deref(), Some("SIGINT"));
        assert_eq!(service.stop_timeout, Some(3));
        assert!(service.is_active());
    }

    #[test]
    fn test_service_box_from_record_uses_network_mode_fallback() {
        let mut record = crate::test_helpers::fixtures::make_record(
            "compose-id",
            "project-web",
            "running",
            None,
        );
        record.network_name = None;
        record.network_mode = a3s_box_core::NetworkMode::Bridge {
            network: "legacy_default".to_string(),
        };

        let service = ServiceBox::from_record(&record);

        assert_eq!(service.network_name.as_deref(), Some("legacy_default"));
    }

    #[test]
    fn test_rollback_with_current_appends_current_service() {
        let mut first_record =
            crate::test_helpers::fixtures::make_record("first-id", "project-db", "running", None);
        first_record
            .labels
            .insert(LABEL_SERVICE.to_string(), "db".to_string());
        let mut current_record = crate::test_helpers::fixtures::make_record(
            "current-id",
            "project-web",
            "running",
            None,
        );
        current_record
            .labels
            .insert(LABEL_SERVICE.to_string(), "web".to_string());

        let first = ServiceBox::from_record(&first_record);
        let current = ServiceBox::from_record(&current_record);
        let rollback_services = rollback_with_current(&[first], current);

        assert_eq!(rollback_services.len(), 2);
        assert_eq!(rollback_services[0].svc_name, "db");
        assert_eq!(rollback_services[1].svc_name, "web");
    }

    #[test]
    fn test_service_box_paused_is_active() {
        let mut record =
            crate::test_helpers::fixtures::make_record("compose-id", "project-web", "paused", None);
        record
            .labels
            .insert(LABEL_SERVICE.to_string(), "web".to_string());

        let service = ServiceBox::from_record(&record);

        assert!(service.is_active());
    }

    #[test]
    fn test_service_box_stopped_is_not_active() {
        let mut record = crate::test_helpers::fixtures::make_record(
            "compose-id",
            "project-web",
            "stopped",
            None,
        );
        record
            .labels
            .insert(LABEL_SERVICE.to_string(), "web".to_string());

        let service = ServiceBox::from_record(&record);

        assert!(!service.is_active());
    }
}
