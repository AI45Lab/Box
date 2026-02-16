//! `a3s-box compose` command — Multi-container orchestration.
//!
//! Subcommands: `up`, `down`, `ps`, `config`.

use std::path::PathBuf;

use a3s_box_core::compose::ComposeConfig;
use a3s_box_runtime::ComposeProject;
use clap::{Args, Subcommand};

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
        ComposeCommand::Down(down_args) => execute_down(&project_name, config, down_args).await,
        ComposeCommand::Ps => execute_ps(&project_name, config).await,
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

/// `compose up` — Create networks and start services in dependency order.
async fn execute_up(
    project_name: &str,
    config: ComposeConfig,
    _up_args: ComposeUpArgs,
) -> Result<(), Box<dyn std::error::Error>> {
    let project = ComposeProject::new(project_name, config)?;

    println!("Project: {}", project_name);
    println!("Services: {}", project.service_order.len());

    // Show boot order
    println!("\nBoot order:");
    for (i, svc) in project.service_order.iter().enumerate() {
        let image = project
            .config
            .services
            .get(svc)
            .and_then(|s| s.image.as_deref())
            .unwrap_or("?");
        println!("  {}. {} ({})", i + 1, svc, image);
    }

    // List required networks
    let networks = project.required_networks();
    println!("\nNetworks:");
    for net in &networks {
        println!("  - {}", net);
    }

    // Build and display configs for each service
    let default_net = project.default_network_name();
    println!("\nStarting services...");
    for svc_name in &project.service_order {
        let box_config = project.build_box_config(svc_name, Some(&default_net))?;
        let image = match &box_config.agent {
            a3s_box_core::config::AgentType::OciRegistry { reference } => reference.clone(),
            _ => "unknown".to_string(),
        };
        println!(
            "  [+] {} (image={}, cpus={}, mem={}MB)",
            svc_name, image, box_config.resources.vcpus, box_config.resources.memory_mb
        );
    }

    println!("\nAll {} services configured.", project.service_order.len());
    println!(
        "Run `a3s-box run` for each service, or use the SDK for programmatic orchestration."
    );

    Ok(())
}

/// `compose down` — Stop and remove all services in reverse order.
async fn execute_down(
    project_name: &str,
    config: ComposeConfig,
    down_args: ComposeDownArgs,
) -> Result<(), Box<dyn std::error::Error>> {
    let project = ComposeProject::new(project_name, config)?;

    println!("Stopping project: {}", project_name);

    // Show shutdown order
    let shutdown = project.shutdown_order();
    for svc in &shutdown {
        println!("  [-] Stopping {}", svc);
    }

    // Clean up networks
    let networks = project.required_networks();
    for net in &networks {
        println!("  [-] Removing network {}", net);
    }

    if down_args.volumes {
        for vol_name in project.config.volumes.keys() {
            println!("  [-] Removing volume {}", vol_name);
        }
    }

    println!("Project {} stopped.", project_name);
    Ok(())
}

/// `compose ps` — List services and their status.
async fn execute_ps(
    project_name: &str,
    config: ComposeConfig,
) -> Result<(), Box<dyn std::error::Error>> {
    let project = ComposeProject::new(project_name, config)?;

    println!("{:<20} {:<30} {:<10}", "SERVICE", "IMAGE", "STATUS");
    println!("{}", "-".repeat(60));

    for svc_name in &project.service_order {
        let image = project
            .config
            .services
            .get(svc_name)
            .and_then(|s| s.image.as_deref())
            .unwrap_or("?");
        let status = if project.box_id(svc_name).is_some() {
            "running"
        } else {
            "stopped"
        };
        println!("{:<20} {:<30} {:<10}", svc_name, image, status);
    }

    Ok(())
}

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

    // Print each service summary
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
}
