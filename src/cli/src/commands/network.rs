//! `a3s-box network` subcommands — Manage custom networks.
//!
//! Provides create/ls/rm/inspect/connect/disconnect for user-defined
//! bridge networks that enable container-to-container communication.

use a3s_box_core::network::{IsolationMode, NetworkConfig};
use a3s_box_runtime::NetworkStore;
use clap::{Args, Subcommand};

/// Manage networks.
#[derive(Args)]
pub struct NetworkArgs {
    #[command(subcommand)]
    pub command: NetworkCommand,
}

/// Network subcommands.
#[derive(Subcommand)]
pub enum NetworkCommand {
    /// Create a new network
    Create(CreateArgs),
    /// List networks
    Ls(LsArgs),
    /// Remove one or more networks
    Rm(RmArgs),
    /// Display detailed network information
    Inspect(InspectArgs),
    /// Connect a box to a network
    Connect(ConnectArgs),
    /// Disconnect a box from a network
    Disconnect(DisconnectArgs),
}

#[derive(Args)]
pub struct CreateArgs {
    /// Network name
    pub name: String,

    /// Subnet in CIDR notation (e.g., "10.89.0.0/24")
    #[arg(long, default_value = "10.89.0.0/24")]
    pub subnet: String,

    /// Network driver
    #[arg(long, default_value = "bridge")]
    pub driver: String,

    /// Network isolation mode: none, strict, or custom (default: none)
    #[arg(long, default_value = "none")]
    pub isolation: String,

    /// Set metadata labels (KEY=VALUE), can be repeated
    #[arg(short = 'l', long = "label")]
    pub labels: Vec<String>,
}

#[derive(Args)]
pub struct LsArgs {
    /// Only display network names
    #[arg(short, long)]
    pub quiet: bool,
}

#[derive(Args)]
pub struct RmArgs {
    /// Network name(s) to remove
    pub names: Vec<String>,

    /// Force removal (disconnect all endpoints first)
    #[arg(short, long)]
    pub force: bool,
}

#[derive(Args)]
pub struct InspectArgs {
    /// Network name
    pub name: String,
}

#[derive(Args)]
pub struct ConnectArgs {
    /// Network name
    pub network: String,

    /// Box name or ID
    pub container: String,
}

#[derive(Args)]
pub struct DisconnectArgs {
    /// Network name
    pub network: String,

    /// Box name or ID
    pub container: String,

    /// Force disconnection
    #[arg(short, long)]
    pub force: bool,
}

/// Dispatch network subcommands.
pub async fn execute(args: NetworkArgs) -> Result<(), Box<dyn std::error::Error>> {
    match args.command {
        NetworkCommand::Create(a) => execute_create(a).await,
        NetworkCommand::Ls(a) => execute_ls(a).await,
        NetworkCommand::Rm(a) => execute_rm(a).await,
        NetworkCommand::Inspect(a) => execute_inspect(a).await,
        NetworkCommand::Connect(a) => execute_connect(a).await,
        NetworkCommand::Disconnect(a) => execute_disconnect(a).await,
    }
}

async fn execute_create(args: CreateArgs) -> Result<(), Box<dyn std::error::Error>> {
    let store = NetworkStore::default_path()?;

    let mut config = NetworkConfig::new(&args.name, &args.subnet)
        .map_err(|e| format!("Invalid network configuration: {e}"))?;

    config.driver = args.driver;

    // Parse isolation mode
    config.policy.isolation = match args.isolation.as_str() {
        "none" => IsolationMode::None,
        "strict" => IsolationMode::Strict,
        "custom" => IsolationMode::Custom,
        other => {
            return Err(
                format!("Unknown isolation mode '{other}'. Use: none, strict, custom").into(),
            )
        }
    };

    // Parse labels
    for label in &args.labels {
        let (key, value) = label
            .split_once('=')
            .ok_or_else(|| format!("Invalid label (expected KEY=VALUE): {label}"))?;
        config.labels.insert(key.to_string(), value.to_string());
    }

    store.create(config)?;
    println!("{}", args.name);
    Ok(())
}

async fn execute_ls(args: LsArgs) -> Result<(), Box<dyn std::error::Error>> {
    let store = NetworkStore::default_path()?;
    let mut networks = store.list()?;
    networks.sort_by(|a, b| a.name.cmp(&b.name));

    if args.quiet {
        for net in &networks {
            println!("{}", net.name);
        }
        return Ok(());
    }

    let mut table = comfy_table::Table::new();
    table.load_preset(comfy_table::presets::NOTHING);
    table.set_header(vec![
        "NETWORK NAME",
        "DRIVER",
        "SUBNET",
        "GATEWAY",
        "ISOLATION",
        "ENDPOINTS",
    ]);

    for net in &networks {
        let isolation = format!("{:?}", net.policy.isolation).to_lowercase();
        table.add_row(vec![
            net.name.clone(),
            net.driver.clone(),
            net.subnet.clone(),
            net.gateway.to_string(),
            isolation,
            net.endpoints.len().to_string(),
        ]);
    }

    println!("{table}");
    Ok(())
}

async fn execute_rm(args: RmArgs) -> Result<(), Box<dyn std::error::Error>> {
    if args.names.is_empty() {
        return Err("requires at least 1 argument".into());
    }

    let store = NetworkStore::default_path()?;

    for name in &args.names {
        if args.force {
            // Force: disconnect all endpoints first
            if let Some(mut config) = store.get(name)? {
                let box_ids: Vec<String> = config.endpoints.keys().cloned().collect();
                for box_id in box_ids {
                    config.disconnect(&box_id).ok();
                }
                store.update(&config)?;
            }
        }

        match store.remove(name) {
            Ok(_) => println!("{name}"),
            Err(e) => eprintln!("Error removing network '{name}': {e}"),
        }
    }

    Ok(())
}

async fn execute_inspect(args: InspectArgs) -> Result<(), Box<dyn std::error::Error>> {
    let store = NetworkStore::default_path()?;

    let config = store
        .get(&args.name)?
        .ok_or_else(|| format!("network '{}' not found", args.name))?;

    let json = serde_json::to_string_pretty(&config)?;
    println!("{json}");
    Ok(())
}

async fn execute_connect(args: ConnectArgs) -> Result<(), Box<dyn std::error::Error>> {
    let store = NetworkStore::default_path()?;
    let state = crate::state::StateFile::load_default()?;

    // Resolve box name/ID using Docker-compatible resolution
    let record = crate::resolve::resolve(&state, &args.container)?;

    let mut config = store
        .get(&args.network)?
        .ok_or_else(|| format!("network '{}' not found", args.network))?;

    // Enforce network isolation policy before connecting
    if config.policy.isolation == IsolationMode::Strict {
        return Err(format!(
            "network '{}' has strict isolation — no new connections allowed",
            args.network
        )
        .into());
    }

    let endpoint = config
        .connect(&record.id, &record.name)
        .map_err(|e| format!("Failed to connect: {e}"))?;

    store.update(&config)?;

    println!(
        "Connected {} to {} (IP: {})",
        record.name, args.network, endpoint.ip_address
    );
    Ok(())
}

async fn execute_disconnect(args: DisconnectArgs) -> Result<(), Box<dyn std::error::Error>> {
    let store = NetworkStore::default_path()?;
    let state = crate::state::StateFile::load_default()?;

    // Resolve box name/ID using Docker-compatible resolution
    let record = crate::resolve::resolve(&state, &args.container)?;

    let mut config = store
        .get(&args.network)?
        .ok_or_else(|| format!("network '{}' not found", args.network))?;

    config
        .disconnect(&record.id)
        .map_err(|e| format!("Failed to disconnect: {e}"))?;

    store.update(&config)?;

    println!("Disconnected {} from {}", record.name, args.network);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn temp_store() -> (tempfile::TempDir, NetworkStore) {
        let dir = tempfile::tempdir().unwrap();
        let store = NetworkStore::new(dir.path().join("networks.json"));
        (dir, store)
    }

    #[test]
    fn test_create_network_via_store() {
        let (_dir, store) = temp_store();
        let config = NetworkConfig::new("testnet", "10.89.0.0/24").unwrap();
        store.create(config).unwrap();

        let loaded = store.get("testnet").unwrap().unwrap();
        assert_eq!(loaded.name, "testnet");
        assert_eq!(loaded.subnet, "10.89.0.0/24");
        assert_eq!(loaded.driver, "bridge");
    }

    #[test]
    fn test_create_network_with_labels() {
        let (_dir, store) = temp_store();
        let mut config = NetworkConfig::new("testnet", "10.89.0.0/24").unwrap();
        config.labels.insert("env".to_string(), "test".to_string());
        store.create(config).unwrap();

        let loaded = store.get("testnet").unwrap().unwrap();
        assert_eq!(loaded.labels.get("env").unwrap(), "test");
    }

    #[test]
    fn test_create_duplicate_network_fails() {
        let (_dir, store) = temp_store();
        let c1 = NetworkConfig::new("testnet", "10.89.0.0/24").unwrap();
        let c2 = NetworkConfig::new("testnet", "10.90.0.0/24").unwrap();
        store.create(c1).unwrap();
        assert!(store.create(c2).is_err());
    }

    #[test]
    fn test_list_networks_sorted() {
        let (_dir, store) = temp_store();
        store
            .create(NetworkConfig::new("znet", "10.89.0.0/24").unwrap())
            .unwrap();
        store
            .create(NetworkConfig::new("anet", "10.90.0.0/24").unwrap())
            .unwrap();

        let mut list = store.list().unwrap();
        list.sort_by(|a, b| a.name.cmp(&b.name));
        assert_eq!(list[0].name, "anet");
        assert_eq!(list[1].name, "znet");
    }

    #[test]
    fn test_remove_network() {
        let (_dir, store) = temp_store();
        store
            .create(NetworkConfig::new("testnet", "10.89.0.0/24").unwrap())
            .unwrap();
        store.remove("testnet").unwrap();
        assert!(store.get("testnet").unwrap().is_none());
    }

    #[test]
    fn test_remove_network_with_endpoints_fails() {
        let (_dir, store) = temp_store();
        let mut config = NetworkConfig::new("testnet", "10.89.0.0/24").unwrap();
        config.connect("box-1", "web").unwrap();
        store.create(config).unwrap();

        assert!(store.remove("testnet").is_err());
    }

    #[test]
    fn test_force_remove_with_endpoints() {
        let (_dir, store) = temp_store();
        let mut config = NetworkConfig::new("testnet", "10.89.0.0/24").unwrap();
        config.connect("box-1", "web").unwrap();
        store.create(config).unwrap();

        // Simulate force: disconnect all, then update, then remove
        let mut config = store.get("testnet").unwrap().unwrap();
        let box_ids: Vec<String> = config.endpoints.keys().cloned().collect();
        for box_id in box_ids {
            config.disconnect(&box_id).ok();
        }
        store.update(&config).unwrap();
        store.remove("testnet").unwrap();

        assert!(store.get("testnet").unwrap().is_none());
    }

    #[test]
    fn test_inspect_network() {
        let (_dir, store) = temp_store();
        let mut config = NetworkConfig::new("testnet", "10.89.0.0/24").unwrap();
        config.connect("box-1", "web").unwrap();
        store.create(config).unwrap();

        let loaded = store.get("testnet").unwrap().unwrap();
        let json = serde_json::to_string_pretty(&loaded).unwrap();
        assert!(json.contains("testnet"));
        assert!(json.contains("box-1"));
        assert!(json.contains("10.89.0.2"));
    }

    #[test]
    fn test_connect_box_to_network() {
        let (_dir, store) = temp_store();
        store
            .create(NetworkConfig::new("testnet", "10.89.0.0/24").unwrap())
            .unwrap();

        let mut config = store.get("testnet").unwrap().unwrap();
        let ep = config.connect("box-1", "web").unwrap();
        store.update(&config).unwrap();

        assert_eq!(ep.ip_address, std::net::Ipv4Addr::new(10, 89, 0, 2));
        assert_eq!(ep.box_name, "web");

        let reloaded = store.get("testnet").unwrap().unwrap();
        assert_eq!(reloaded.endpoints.len(), 1);
    }

    #[test]
    fn test_disconnect_box_from_network() {
        let (_dir, store) = temp_store();
        let mut config = NetworkConfig::new("testnet", "10.89.0.0/24").unwrap();
        config.connect("box-1", "web").unwrap();
        store.create(config).unwrap();

        let mut config = store.get("testnet").unwrap().unwrap();
        config.disconnect("box-1").unwrap();
        store.update(&config).unwrap();

        let reloaded = store.get("testnet").unwrap().unwrap();
        assert!(reloaded.endpoints.is_empty());
    }

    #[test]
    fn test_disconnect_nonexistent_box_fails() {
        let (_dir, store) = temp_store();
        let mut config = NetworkConfig::new("testnet", "10.89.0.0/24").unwrap();
        store.create(config.clone()).unwrap();

        assert!(config.disconnect("nonexistent").is_err());
    }

    #[test]
    fn test_connect_multiple_boxes() {
        let (_dir, store) = temp_store();
        store
            .create(NetworkConfig::new("testnet", "10.89.0.0/24").unwrap())
            .unwrap();

        let mut config = store.get("testnet").unwrap().unwrap();
        let ep1 = config.connect("box-1", "web").unwrap();
        let ep2 = config.connect("box-2", "api").unwrap();
        store.update(&config).unwrap();

        assert_eq!(ep1.ip_address, std::net::Ipv4Addr::new(10, 89, 0, 2));
        assert_eq!(ep2.ip_address, std::net::Ipv4Addr::new(10, 89, 0, 3));

        let reloaded = store.get("testnet").unwrap().unwrap();
        assert_eq!(reloaded.endpoints.len(), 2);
    }

    #[test]
    fn test_parse_labels() {
        let labels = vec!["env=prod".to_string(), "team=infra".to_string()];
        let mut map = HashMap::new();
        for label in &labels {
            let (key, value) = label.split_once('=').unwrap();
            map.insert(key.to_string(), value.to_string());
        }
        assert_eq!(map.get("env").unwrap(), "prod");
        assert_eq!(map.get("team").unwrap(), "infra");
    }

    #[test]
    fn test_invalid_label_format() {
        let label = "no-equals-sign";
        assert!(label.split_once('=').is_none());
    }
}
