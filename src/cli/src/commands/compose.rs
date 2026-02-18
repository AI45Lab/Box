//! `a3s-box compose` command — Multi-container orchestration.
//!
//! Subcommands: `up`, `down`, `ps`, `config`.

use std::collections::HashMap;
use std::path::PathBuf;

use a3s_box_core::compose::ComposeConfig;
use a3s_box_core::event::EventEmitter;
use a3s_box_runtime::{ComposeProject, NetworkStore, VmManager};
use clap::{Args, Subcommand};

use crate::state::{BoxRecord, StateFile};

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
}

#[derive(Args)]
pub struct ComposeUpArgs {
    /// Run in detached mode (background)
    #[arg(short = 'd', long)]
    pub detach: bool,
}

#[derive(Args)]
pub struct ComposeDownArgs {
    /// Remove named volumes declared in the compose file
    #[arg(short = 'v', long)]
    pub volumes: bool,
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
        ComposeCommand::Up(up_args) => execute_up(&project_name, config, up_args).await,
        ComposeCommand::Down(down_args) => execute_down(&project_name, down_args).await,
        ComposeCommand::Ps => execute_ps(&project_name).await,
        ComposeCommand::Config => execute_config(&project_name, config),
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

// ============================================================================
// compose up
// ============================================================================

/// `compose up` — Create networks and start services in dependency order.
async fn execute_up(
    project_name: &str,
    config: ComposeConfig,
    _up_args: ComposeUpArgs,
) -> Result<(), Box<dyn std::error::Error>> {
    let project = ComposeProject::new(project_name, config)?;
    let mut state = StateFile::load_default()?;

    // Check for already-running services
    let existing = state.find_by_label(LABEL_PROJECT, project_name);
    let running: Vec<_> = existing.iter().filter(|r| r.status == "running").collect();
    if !running.is_empty() {
        let names: Vec<_> = running
            .iter()
            .filter_map(|r| r.labels.get(LABEL_SERVICE))
            .collect();
        return Err(format!(
            "Project '{}' already has running services: {}. Run `compose down` first.",
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
    for (i, net_name) in networks.iter().enumerate() {
        if net_store.get(net_name)?.is_none() {
            // Auto-assign subnets: 10.89.{i}.0/24
            let subnet = format!("10.89.{}.0/24", 100 + i);
            let config = a3s_box_core::network::NetworkConfig::new(net_name, &subnet)
                .map_err(|e| format!("Failed to create network '{}': {}", net_name, e))?;
            net_store.create(config)?;
            println!("  [+] Network {} ({})", net_name, subnet);
        }
    }

    // Step 2: Boot services in dependency order
    let default_net = project.default_network_name();
    let home = dirs::home_dir()
        .map(|h| h.join(".a3s"))
        .unwrap_or_else(|| PathBuf::from(".a3s"));

    println!(
        "Starting project '{}' ({} services)...",
        project_name,
        project.service_order.len()
    );

    for svc_name in &project.service_order {
        let box_config = project.build_box_config(svc_name, Some(&default_net))?;
        let image = box_config.image.clone();

        // Create VmManager and boot
        let emitter = EventEmitter::new(256);
        let box_name = format!("{}-{}", project_name, svc_name);
        let mut vm = VmManager::new(box_config, emitter);
        let box_id = vm.box_id().to_string();
        let box_dir = home.join("boxes").join(&box_id);

        // Create box directory structure
        std::fs::create_dir_all(box_dir.join("sockets"))?;
        std::fs::create_dir_all(box_dir.join("logs"))?;

        // Connect to network before boot
        if let Ok(Some(mut net_config)) = net_store.get(&default_net) {
            if let Ok(endpoint) = net_config.connect(&box_id, &box_name) {
                net_store.update(&net_config)?;
                print!(
                    "  [+] {} (image={}, ip={})",
                    svc_name, image, endpoint.ip_address
                );
            }
        }

        vm.boot()
            .await
            .map_err(|e| format!("Failed to start service '{}': {}", svc_name, e))?;

        let pid = vm.pid().await;

        // Build labels with compose metadata
        let mut labels = HashMap::new();
        labels.insert(LABEL_PROJECT.to_string(), project_name.to_string());
        labels.insert(LABEL_SERVICE.to_string(), svc_name.to_string());

        // Get service config for extra fields
        let svc = project.config.services.get(svc_name);
        let env: HashMap<String, String> = svc
            .map(|s| s.environment.to_pairs().into_iter().collect())
            .unwrap_or_default();
        let volumes: Vec<String> = svc.map(|s| s.volumes.clone()).unwrap_or_default();
        let port_map: Vec<String> = svc.map(|s| s.ports.clone()).unwrap_or_default();

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
            volumes,
            env,
            cmd: svc
                .and_then(|s| s.command.as_ref())
                .map(|c| c.to_vec())
                .unwrap_or_default(),
            entrypoint: svc.and_then(|s| s.entrypoint.as_ref()).map(|e| e.to_vec()),
            box_dir: box_dir.clone(),
            exec_socket_path: box_dir.join("sockets").join("exec.sock"),
            console_log: box_dir.join("logs").join("console.log"),
            created_at: chrono::Utc::now(),
            started_at: Some(chrono::Utc::now()),
            auto_remove: false,
            hostname: None,
            user: None,
            workdir: svc.and_then(|s| s.working_dir.clone()),
            restart_policy: svc
                .and_then(|s| s.restart.as_deref())
                .unwrap_or("no")
                .to_string(),
            port_map,
            labels,
            stopped_by_user: false,
            restart_count: 0,
            max_restart_count: 0,
            exit_code: None,
            health_check: None,
            health_status: "none".to_string(),
            health_retries: 0,
            health_last_check: None,
            network_mode: a3s_box_core::NetworkMode::Bridge {
                network: default_net.clone(),
            },
            network_name: Some(default_net.clone()),
            volume_names: vec![],
            tmpfs: svc.map(|s| s.tmpfs.to_vec()).unwrap_or_default(),
            anonymous_volumes: vec![],
            resource_limits: Default::default(),
            log_config: Default::default(),
            add_host: vec![],
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
            stop_signal: None,
            stop_timeout: None,
            oom_kill_disable: false,
            oom_score_adj: None,
        };

        state.add(record)?;

        // Spawn log processor
        let log_dir = box_dir.join("logs");
        let _ = a3s_box_runtime::log::spawn_log_processor(
            box_dir.join("logs").join("console.log"),
            log_dir,
            Default::default(),
        );

        println!(" ✓");
    }

    println!("All {} services started.", project.service_order.len());
    Ok(())
}

// ============================================================================
// compose down
// ============================================================================

/// `compose down` — Stop and remove all services, networks, and optionally volumes.
async fn execute_down(
    project_name: &str,
    down_args: ComposeDownArgs,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut state = StateFile::load_default()?;

    // Find all boxes belonging to this project
    #[allow(clippy::type_complexity)]
    let project_boxes: Vec<(
        String,
        String,
        Option<u32>,
        String,
        PathBuf,
        Option<String>,
        Vec<String>,
    )> = state
        .find_by_label(LABEL_PROJECT, project_name)
        .iter()
        .map(|r| {
            (
                r.id.clone(),
                r.labels.get(LABEL_SERVICE).cloned().unwrap_or_default(),
                r.pid,
                r.status.clone(),
                r.box_dir.clone(),
                r.network_name.clone(),
                r.volume_names.clone(),
            )
        })
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
    for (box_id, svc_name, pid, status, box_dir, network_name, volume_names) in
        project_boxes.iter().rev()
    {
        print!("  [-] Stopping {}...", svc_name);

        // Kill the process if running
        if status == "running" {
            if let Some(pid) = pid {
                crate::process::graceful_stop(*pid, libc::SIGTERM, 10).await;
            }
        }

        // Clean up resources
        crate::cleanup::cleanup_box_resources(box_id, volume_names, network_name.as_deref());

        // Remove box directory and state record
        let _ = std::fs::remove_dir_all(box_dir);
        state.remove(box_id)?;

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
        // Collect all volume names from project services
        let mut removed = 0u32;
        for (_box_id, _svc_name, _pid, _status, _box_dir, _network_name, volume_names) in
            &project_boxes
        {
            for vol_name in volume_names {
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
        "{:<20} {:<30} {:<12} {:<10}",
        "SERVICE", "IMAGE", "STATUS", "PID"
    );
    println!("{}", "-".repeat(72));

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
            "{:<20} {:<30} {:<12} {:<10}",
            svc_name, record.image, record.status, pid_str
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
}
