//! A3S Box CRI - Kubernetes Container Runtime Interface binary.
//!
//! Serves CRI RuntimeService and ImageService over a Unix domain socket,
//! allowing kubelet to schedule pods onto A3S Box microVMs.

use std::path::PathBuf;
use std::sync::Arc;

use clap::Parser;
use tracing_subscriber::EnvFilter;

use a3s_box_runtime::oci::{ImageStore, RegistryAuth};

use a3s_box_cri::config_mapper::DEFAULT_AGENT_IMAGE;
use a3s_box_cri::runtime_service::CriRuntimeOptions;
use a3s_box_cri::server::CriServer;

const AGENT_IMAGE_ENV: &str = "A3S_BOX_CRI_AGENT_IMAGE";

/// A3S Box CRI Runtime
#[derive(Parser, Debug)]
#[command(name = "a3s-box-cri", about = "A3S Box CRI Runtime")]
struct Args {
    /// Path to the Unix domain socket for CRI communication.
    #[arg(long, default_value = "/var/run/a3s-box/a3s-box.sock")]
    socket: PathBuf,

    /// Directory for storing pulled OCI images.
    #[arg(long, default_value = "~/.a3s/images")]
    image_dir: String,

    /// Maximum image cache size in bytes (default: 10GB).
    #[arg(long, default_value = "10737418240")]
    image_cache_size: u64,

    /// Default sandbox VM agent/rootfs image used when Pods omit a3s.box/agent-image.
    #[arg(long)]
    agent_image: Option<String>,

    /// RuntimeClass-specific agent image override, formatted as HANDLER=IMAGE.
    #[arg(long = "runtime-handler-agent-image", value_name = "HANDLER=IMAGE")]
    runtime_handler_agent_image: Vec<String>,
}

fn parse_runtime_handler_agent_images(
    values: &[String],
) -> Result<std::collections::HashMap<String, String>, String> {
    let mut images = std::collections::HashMap::new();
    for value in values {
        let Some((handler, image)) = value.split_once('=') else {
            return Err(format!(
                "Invalid --runtime-handler-agent-image '{}': expected HANDLER=IMAGE",
                value
            ));
        };

        let handler = handler.trim();
        let image = image.trim();
        if handler.is_empty() || image.is_empty() {
            return Err(format!(
                "Invalid --runtime-handler-agent-image '{}': handler and image must be non-empty",
                value
            ));
        }

        images.insert(handler.to_string(), image.to_string());
    }
    Ok(images)
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    // Initialize tracing
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let args = Args::parse();
    let default_agent_image = args
        .agent_image
        .clone()
        .or_else(|| std::env::var(AGENT_IMAGE_ENV).ok())
        .unwrap_or_else(|| DEFAULT_AGENT_IMAGE.to_string());
    let runtime_handler_agent_images =
        parse_runtime_handler_agent_images(&args.runtime_handler_agent_image)
            .map_err(|e| format!("Invalid CRI runtime options: {e}"))?;
    let runtime_options = CriRuntimeOptions {
        default_agent_image,
        runtime_handler_agent_images,
    };

    // Resolve image directory (expand ~)
    let image_dir = if args.image_dir.starts_with('~') {
        let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));
        home.join(args.image_dir.strip_prefix("~/").unwrap_or(&args.image_dir))
    } else {
        PathBuf::from(&args.image_dir)
    };

    tracing::info!(
        socket = %args.socket.display(),
        image_dir = %image_dir.display(),
        cache_size = args.image_cache_size,
        agent_image = %runtime_options.default_agent_image,
        runtime_handler_overrides = runtime_options.runtime_handler_agent_images.len(),
        "Starting A3S Box CRI Runtime"
    );

    // Initialize image store
    let image_store = Arc::new(
        ImageStore::new(&image_dir, args.image_cache_size)
            .map_err(|e| format!("Failed to initialize image store: {}", e))?,
    );

    // Use environment-based auth
    let auth = RegistryAuth::from_env();

    // Create and start CRI server
    let server =
        CriServer::new(args.socket, image_store, auth).with_runtime_options(runtime_options);
    server.serve().await?;

    Ok(())
}
