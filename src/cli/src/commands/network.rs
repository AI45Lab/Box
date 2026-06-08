//! `a3s-box network` subcommands — Manage custom networks.
//!
//! Provides create/ls/rm/inspect/connect/disconnect for user-defined
//! bridge networks that enable container-to-container communication.

use a3s_box_core::network::{IsolationMode, NetworkConfig, NetworkEndpoint, NetworkMode};
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
    /// Remove all unused networks
    Prune(PruneArgs),
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

#[derive(Args)]
pub struct PruneArgs {
    /// Skip confirmation prompt
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
        NetworkCommand::Prune(a) => execute_prune(a).await,
    }
}

/// Networks that mirror Docker's predefined networks and are never pruned.
fn is_predefined_network(name: &str) -> bool {
    matches!(name, "bridge" | "host" | "none")
}

/// Whether a network has no attachments: no live endpoints and no box record
/// (running or stopped) configured for it. Matches `docker network prune`,
/// which removes networks not used by at least one container.
fn network_is_unused(
    config: &NetworkConfig,
    in_use_names: &std::collections::HashSet<String>,
) -> bool {
    config.endpoints.is_empty() && !in_use_names.contains(&config.name)
}

/// Remove every unused, non-predefined network from `store`. Returns the names
/// removed and any per-network errors. Shared by `network prune` and
/// `system prune` (Docker's `system prune` also reaps unused networks).
pub(crate) fn prune_unused_networks(
    store: &NetworkStore,
    state: &crate::state::StateFile,
) -> (Vec<String>, Vec<String>) {
    let in_use: std::collections::HashSet<String> = state
        .records()
        .iter()
        .filter_map(|record| crate::cleanup::record_network_name(record).map(str::to_string))
        .collect();

    let mut networks = store.list().unwrap_or_default();
    networks.sort_by(|a, b| a.name.cmp(&b.name));

    let mut removed = Vec::new();
    let mut errors = Vec::new();
    for net in &networks {
        if is_predefined_network(&net.name) || !network_is_unused(net, &in_use) {
            continue;
        }
        match store.remove(&net.name) {
            Ok(_) => removed.push(net.name.clone()),
            Err(error) => errors.push(format!("{}: {error}", net.name)),
        }
    }
    (removed, errors)
}

async fn execute_prune(args: PruneArgs) -> Result<(), Box<dyn std::error::Error>> {
    if !args.force {
        println!("WARNING! This will remove all networks not used by at least one box.");
        print!("Are you sure you want to continue? [y/N] ");
        use std::io::Write;
        std::io::stdout().flush()?;

        let mut input = String::new();
        std::io::stdin().read_line(&mut input)?;
        if !input.trim().eq_ignore_ascii_case("y") {
            return Ok(());
        }
    }

    let store = NetworkStore::default_path()?;
    let state = crate::state::StateFile::load_default()?;
    let (removed, errors) = prune_unused_networks(&store, &state);

    if removed.is_empty() {
        println!("Total reclaimed space: 0 networks");
    } else {
        println!("Deleted Networks:");
        for name in &removed {
            println!("{name}");
        }
        println!("Total reclaimed space: {} network(s)", removed.len());
    }

    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors.join("\n").into())
    }
}

async fn execute_create(args: CreateArgs) -> Result<(), Box<dyn std::error::Error>> {
    let store = NetworkStore::default_path()?;

    validate_network_driver(&args.driver)?;

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
    validate_attachable_network(&config)?;

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

pub(crate) fn validate_attachable_network(config: &NetworkConfig) -> Result<(), String> {
    validate_network_driver(&config.driver)?;
    config
        .policy
        .validate()
        .map_err(|e| format!("Unsupported network isolation mode: {e}"))?;
    Ok(())
}

pub(crate) fn validate_network_driver(driver: &str) -> Result<(), String> {
    if driver == "bridge" {
        Ok(())
    } else {
        Err(format!(
            "Unsupported network driver '{driver}'. Only 'bridge' is currently supported"
        ))
    }
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
    let mut state = crate::state::StateFile::load_default()?;
    let mut errors = Vec::new();

    for name in &args.names {
        if args.force {
            if let Some(mut config) = store.get(name)? {
                if let Err(error) =
                    force_disconnect_network_endpoints(&store, &mut state, name, &mut config)
                {
                    errors.push(format!("{name}: {error}"));
                    continue;
                }
            }
        }

        match store.remove(name) {
            Ok(_) => println!("{name}"),
            Err(error) => errors.push(format!("{name}: {error}")),
        }
    }

    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors.join("\n").into())
    }
}

fn force_disconnect_network_endpoints(
    store: &NetworkStore,
    state: &mut crate::state::StateFile,
    network_name: &str,
    config: &mut NetworkConfig,
) -> Result<(), Box<dyn std::error::Error>> {
    let configured_box_ids: Vec<String> = state
        .records()
        .iter()
        .filter(|record| crate::cleanup::record_network_name(record) == Some(network_name))
        .map(|record| record.id.clone())
        .collect();

    for endpoint in config.endpoints.values() {
        let Some(record) = state.find_by_id(&endpoint.box_id) else {
            continue;
        };
        if crate::status::is_active(record) {
            return Err(format!(
                "network '{network_name}' has active box {}. Stop it before force-removing the network because network hot-plug is not supported yet.",
                record.name
            )
            .into());
        }
    }
    for box_id in &configured_box_ids {
        let Some(record) = state.find_by_id(box_id) else {
            continue;
        };
        if crate::status::is_active(record) {
            return Err(format!(
                "network '{network_name}' is configured on active box {}. Stop it before force-removing the network because network hot-plug is not supported yet.",
                record.name
            )
            .into());
        }
    }

    let box_ids: Vec<String> = config.endpoints.keys().cloned().collect();
    let mut changed_state = false;
    for box_id in box_ids {
        let _ = config.disconnect(&box_id);
    }
    for box_id in configured_box_ids {
        if let Some(record) = state.find_by_id_mut(&box_id) {
            clear_record_network(record);
            changed_state = true;
        }
    }
    store.update(config)?;
    if changed_state {
        state.save()?;
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
    let mut state = crate::state::StateFile::load_default()?;

    // Resolve box name/ID using Docker-compatible resolution
    let record = crate::resolve::resolve(&state, &args.container)?.clone();
    require_inactive_for_network_change(&record, "connect to a network")?;

    if let Some(existing) = crate::cleanup::record_network_name(&record) {
        if existing != args.network {
            return Err(format!(
                "Box {} is already configured for network '{}'. Disconnect it before connecting to '{}'.",
                record.name, existing, args.network
            )
            .into());
        }
    }

    let mut config = store
        .get(&args.network)?
        .ok_or_else(|| format!("network '{}' not found", args.network))?;

    validate_attachable_network(&config)?;

    let endpoint = ensure_endpoint(&mut config, &record.id, &record.name)
        .map_err(|e| format!("Failed to connect: {e}"))?;
    store.update(&config)?;

    {
        let state_record = crate::resolve::resolve_mut(&mut state, &record.id)?;
        set_record_network(state_record, &args.network);
    }
    state.save()?;

    println!(
        "Connected {} to {} (IP: {})",
        record.name, args.network, endpoint.ip_address
    );
    Ok(())
}

async fn execute_disconnect(args: DisconnectArgs) -> Result<(), Box<dyn std::error::Error>> {
    let store = NetworkStore::default_path()?;
    let mut state = crate::state::StateFile::load_default()?;

    // Resolve box name/ID using Docker-compatible resolution
    let record = crate::resolve::resolve(&state, &args.container)?.clone();
    require_inactive_for_network_change(&record, "disconnect from a network")?;

    let configured_network = crate::cleanup::record_network_name(&record).map(str::to_string);
    if configured_network.as_deref() != Some(args.network.as_str()) && !args.force {
        return Err(format!(
            "Box {} is not configured for network '{}'. Use --force to remove a stale endpoint only.",
            record.name, args.network
        )
        .into());
    }

    let is_configured_network = configured_network.as_deref() == Some(args.network.as_str());

    if let Some(mut config) = store.get(&args.network)? {
        match config.disconnect(&record.id) {
            Ok(_) => store.update(&config)?,
            Err(error) if args.force || is_configured_network => {
                tracing::debug!(
                    box_id = %record.id,
                    network = %args.network,
                    error = %error,
                    "Ignoring missing network endpoint during forced disconnect"
                );
            }
            Err(error) => return Err(format!("Failed to disconnect: {error}").into()),
        }
    } else if !args.force {
        return Err(format!("network '{}' not found", args.network).into());
    }

    if is_configured_network {
        let state_record = crate::resolve::resolve_mut(&mut state, &record.id)?;
        clear_record_network(state_record);
        state.save()?;
    }

    println!("Disconnected {} from {}", record.name, args.network);
    Ok(())
}

fn ensure_endpoint(
    config: &mut NetworkConfig,
    box_id: &str,
    box_name: &str,
) -> Result<NetworkEndpoint, String> {
    if let Some(endpoint) = config.endpoints.get_mut(box_id) {
        endpoint.box_name = box_name.to_string();
        return Ok(endpoint.clone());
    }
    config.connect(box_id, box_name)
}

fn require_inactive_for_network_change(
    record: &crate::state::BoxRecord,
    action: &str,
) -> Result<(), String> {
    if !crate::status::is_active(record) {
        return Ok(());
    }

    Err(format!(
        "Cannot {action} box {} because network hot-plug is not supported yet. Stop it first, run the network command, then start it again.",
        record.name
    ))
}

fn set_record_network(record: &mut crate::state::BoxRecord, network: &str) {
    record.network_mode = NetworkMode::Bridge {
        network: network.to_string(),
    };
    record.network_name = Some(network.to_string());
}

fn clear_record_network(record: &mut crate::state::BoxRecord) {
    record.network_mode = NetworkMode::Tsi;
    record.network_name = None;
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
    fn test_validate_network_driver_accepts_bridge_only() {
        assert!(validate_network_driver("bridge").is_ok());

        let error = validate_network_driver("overlay").unwrap_err();
        assert!(error.contains("Unsupported network driver"));
    }

    #[test]
    fn test_validate_attachable_network_rejects_unsupported_policy() {
        let mut config = NetworkConfig::new("testnet", "10.89.0.0/24").unwrap();
        config.policy.isolation = IsolationMode::Custom;

        let error = validate_attachable_network(&config).unwrap_err();

        assert!(error.contains("Unsupported network isolation mode"));
    }

    #[test]
    fn test_ensure_endpoint_reuses_existing_endpoint_and_updates_name() {
        let mut config = NetworkConfig::new("testnet", "10.89.0.0/24").unwrap();
        let first = ensure_endpoint(&mut config, "box-1", "old-name").unwrap();
        let second = ensure_endpoint(&mut config, "box-1", "new-name").unwrap();

        assert_eq!(first.ip_address, second.ip_address);
        assert_eq!(second.box_name, "new-name");
        assert_eq!(config.endpoints.get("box-1").unwrap().box_name, "new-name");
    }

    #[test]
    fn test_require_inactive_for_network_change_rejects_active_boxes() {
        let running =
            crate::test_helpers::fixtures::make_record("box-1", "web", "running", Some(123));
        let paused =
            crate::test_helpers::fixtures::make_record("box-2", "api", "paused", Some(124));
        let stopped = crate::test_helpers::fixtures::make_record("box-3", "db", "stopped", None);

        assert!(require_inactive_for_network_change(&running, "connect").is_err());
        assert!(require_inactive_for_network_change(&paused, "connect").is_err());
        assert!(require_inactive_for_network_change(&stopped, "connect").is_ok());
    }

    #[test]
    fn test_set_and_clear_record_network() {
        let mut record =
            crate::test_helpers::fixtures::make_record("box-1", "web", "created", None);

        set_record_network(&mut record, "backend");
        assert_eq!(record.network_name.as_deref(), Some("backend"));
        assert!(matches!(
            record.network_mode,
            NetworkMode::Bridge { ref network } if network == "backend"
        ));

        clear_record_network(&mut record);
        assert_eq!(record.network_name, None);
        assert!(matches!(record.network_mode, NetworkMode::Tsi));
    }

    #[test]
    fn test_force_disconnect_network_endpoints_clears_inactive_record_network() {
        let (_store_dir, store) = temp_store();
        let mut config = NetworkConfig::new("testnet", "10.89.0.0/24").unwrap();
        config.connect("box-1", "web").unwrap();
        store.create(config.clone()).unwrap();

        let mut record =
            crate::test_helpers::fixtures::make_record("box-1", "web", "stopped", None);
        set_record_network(&mut record, "testnet");
        let (_state_dir, mut state) = crate::test_helpers::fixtures::setup_state(vec![record]);
        let mut config = store.get("testnet").unwrap().unwrap();

        force_disconnect_network_endpoints(&store, &mut state, "testnet", &mut config).unwrap();

        let config = store.get("testnet").unwrap().unwrap();
        assert!(config.endpoints.is_empty());
        let record = state.find_by_id("box-1").unwrap();
        assert_eq!(record.network_name, None);
        assert!(matches!(record.network_mode, NetworkMode::Tsi));
    }

    #[test]
    fn test_force_disconnect_network_endpoints_clears_stale_record_without_endpoint() {
        let (_store_dir, store) = temp_store();
        let config = NetworkConfig::new("testnet", "10.89.0.0/24").unwrap();
        store.create(config).unwrap();

        let mut record =
            crate::test_helpers::fixtures::make_record("box-1", "web", "stopped", None);
        set_record_network(&mut record, "testnet");
        let (_state_dir, mut state) = crate::test_helpers::fixtures::setup_state(vec![record]);
        let mut config = store.get("testnet").unwrap().unwrap();

        force_disconnect_network_endpoints(&store, &mut state, "testnet", &mut config).unwrap();

        let record = state.find_by_id("box-1").unwrap();
        assert_eq!(record.network_name, None);
        assert!(matches!(record.network_mode, NetworkMode::Tsi));
    }

    #[test]
    fn test_force_disconnect_network_endpoints_rejects_active_record() {
        let (_store_dir, store) = temp_store();
        let mut config = NetworkConfig::new("testnet", "10.89.0.0/24").unwrap();
        config.connect("box-1", "web").unwrap();
        store.create(config.clone()).unwrap();

        let mut record =
            crate::test_helpers::fixtures::make_record("box-1", "web", "running", Some(123));
        set_record_network(&mut record, "testnet");
        let (_state_dir, mut state) = crate::test_helpers::fixtures::setup_state(vec![record]);
        let mut config = store.get("testnet").unwrap().unwrap();

        let error = force_disconnect_network_endpoints(&store, &mut state, "testnet", &mut config)
            .unwrap_err();

        assert!(error.to_string().contains("active box web"));
        assert_eq!(store.get("testnet").unwrap().unwrap().endpoints.len(), 1);
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
    fn test_is_predefined_network() {
        assert!(is_predefined_network("bridge"));
        assert!(is_predefined_network("host"));
        assert!(is_predefined_network("none"));
        assert!(!is_predefined_network("mynet"));
    }

    #[test]
    fn test_prune_unused_networks_keeps_attached_and_referenced() {
        let (_dir, store) = temp_store();
        // Truly unused: no endpoints and no box record references it.
        store
            .create(NetworkConfig::new("orphan", "10.89.0.0/24").unwrap())
            .unwrap();
        // Has a live endpoint → kept.
        let mut with_ep = NetworkConfig::new("withep", "10.90.0.0/24").unwrap();
        with_ep.connect("box-1", "web").unwrap();
        store.create(with_ep).unwrap();
        // Referenced by a stopped box record → kept (Docker keeps these too).
        store
            .create(NetworkConfig::new("recnet", "10.91.0.0/24").unwrap())
            .unwrap();

        let mut record = crate::test_helpers::fixtures::make_record("box-2", "db", "stopped", None);
        set_record_network(&mut record, "recnet");
        let (_state_dir, state) = crate::test_helpers::fixtures::setup_state(vec![record]);

        let (removed, errors) = prune_unused_networks(&store, &state);

        assert!(errors.is_empty(), "unexpected errors: {errors:?}");
        assert_eq!(removed, vec!["orphan".to_string()]);
        assert!(store.get("orphan").unwrap().is_none());
        assert!(store.get("withep").unwrap().is_some());
        assert!(store.get("recnet").unwrap().is_some());
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
