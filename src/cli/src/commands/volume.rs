//! `a3s-box volume` subcommands — Manage named volumes.
//!
//! Provides create/ls/rm/inspect/prune for persistent named volumes
//! that can be shared across box instances.

use a3s_box_core::volume::VolumeConfig;
use a3s_box_runtime::VolumeStore;
use clap::{Args, Subcommand};

/// Manage volumes.
#[derive(Args)]
pub struct VolumeArgs {
    #[command(subcommand)]
    pub command: VolumeCommand,
}

/// Volume subcommands.
#[derive(Subcommand)]
pub enum VolumeCommand {
    /// Create a new named volume
    Create(CreateArgs),
    /// List volumes
    Ls(LsArgs),
    /// Remove one or more volumes
    Rm(RmArgs),
    /// Display detailed volume information
    Inspect(InspectArgs),
    /// Remove all unused volumes
    Prune(PruneArgs),
}

#[derive(Args)]
pub struct CreateArgs {
    /// Volume name
    pub name: String,

    /// Volume driver
    #[arg(long, default_value = "local")]
    pub driver: String,

    /// Set metadata labels (KEY=VALUE), can be repeated
    #[arg(short = 'l', long = "label")]
    pub labels: Vec<String>,
}

#[derive(Args)]
pub struct LsArgs {
    /// Only display volume names
    #[arg(short, long)]
    pub quiet: bool,
}

#[derive(Args)]
pub struct RmArgs {
    /// Volume name(s) to remove
    pub names: Vec<String>,

    /// Force removal (even if in use)
    #[arg(short, long)]
    pub force: bool,
}

#[derive(Args)]
pub struct InspectArgs {
    /// Volume name
    pub name: String,
}

#[derive(Args)]
pub struct PruneArgs {
    /// Skip confirmation prompt
    #[arg(short, long)]
    pub force: bool,
}

/// Dispatch volume subcommands.
pub async fn execute(args: VolumeArgs) -> Result<(), Box<dyn std::error::Error>> {
    match args.command {
        VolumeCommand::Create(a) => execute_create(a).await,
        VolumeCommand::Ls(a) => execute_ls(a).await,
        VolumeCommand::Rm(a) => execute_rm(a).await,
        VolumeCommand::Inspect(a) => execute_inspect(a).await,
        VolumeCommand::Prune(a) => execute_prune(a).await,
    }
}

async fn execute_create(args: CreateArgs) -> Result<(), Box<dyn std::error::Error>> {
    let store = VolumeStore::default_path()?;

    let mut config = VolumeConfig::new(&args.name, "");
    config.driver = args.driver;

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
    let store = VolumeStore::default_path()?;
    let mut volumes = store.list()?;
    volumes.sort_by(|a, b| a.name.cmp(&b.name));

    if args.quiet {
        for vol in &volumes {
            println!("{}", vol.name);
        }
        return Ok(());
    }

    let mut table = comfy_table::Table::new();
    table.load_preset(comfy_table::presets::NOTHING);
    table.set_header(vec!["DRIVER", "VOLUME NAME", "MOUNT POINT", "IN USE BY"]);

    for vol in &volumes {
        let in_use = if vol.in_use_by.is_empty() {
            "-".to_string()
        } else {
            format!("{} box(es)", vol.in_use_by.len())
        };
        table.add_row(vec![
            vol.driver.clone(),
            vol.name.clone(),
            vol.mount_point.clone(),
            in_use,
        ]);
    }

    println!("{table}");
    Ok(())
}

async fn execute_rm(args: RmArgs) -> Result<(), Box<dyn std::error::Error>> {
    if args.names.is_empty() {
        return Err("requires at least 1 argument".into());
    }

    let store = VolumeStore::default_path()?;

    let mut errors = Vec::new();
    for name in &args.names {
        match store.remove(name, args.force) {
            Ok(_) => println!("{name}"),
            Err(e) => errors.push(format!("Error removing volume '{name}': {e}")),
        }
    }

    if errors.is_empty() {
        Ok(())
    } else {
        // A failed removal (e.g. volume in use) must be a non-zero exit, like Docker.
        Err(errors.join("\n").into())
    }
}

async fn execute_inspect(args: InspectArgs) -> Result<(), Box<dyn std::error::Error>> {
    let store = VolumeStore::default_path()?;

    let config = store
        .get(&args.name)?
        .ok_or_else(|| format!("volume '{}' not found", args.name))?;

    // Match `docker volume inspect`: a top-level JSON array of PascalCase objects
    // (Mountpoint, Scope, etc.), not the raw snake_case VolumeConfig.
    let output = serde_json::json!([{
        "Name": config.name,
        "Driver": config.driver,
        "Mountpoint": config.mount_point,
        "Scope": "local",
        "Labels": config.labels,
        "Options": serde_json::Map::new(),
        "CreatedAt": config.created_at,
    }]);
    println!("{}", serde_json::to_string_pretty(&output)?);
    Ok(())
}

async fn execute_prune(args: PruneArgs) -> Result<(), Box<dyn std::error::Error>> {
    if !args.force {
        println!("WARNING! This will remove all local volumes not used by at least one box.");
        print!("Are you sure you want to continue? [y/N] ");
        use std::io::Write;
        std::io::stdout().flush()?;

        let mut input = String::new();
        std::io::stdin().read_line(&mut input)?;
        if !input.trim().eq_ignore_ascii_case("y") {
            return Ok(());
        }
    }

    let store = VolumeStore::default_path()?;
    let pruned = store.prune()?;

    if pruned.is_empty() {
        println!("Total reclaimed space: 0B");
    } else {
        for name in &pruned {
            println!("{name}");
        }
        println!("Total reclaimed space: {} volume(s)", pruned.len());
    }

    Ok(())
}

/// Resolve a volume spec, returning the host path for a named volume.
///
/// If the host part of a volume spec (before `:`) doesn't start with `/` or `.`,
/// it's treated as a named volume. The volume is auto-created if it doesn't exist.
///
/// Returns the resolved volume spec (with named volume replaced by host path)
/// and optionally the named volume name if it was a named volume.
pub fn resolve_named_volume(
    volume_spec: &str,
) -> Result<(String, Option<String>), Box<dyn std::error::Error>> {
    let parts: Vec<&str> = volume_spec.split(':').collect();
    if parts.len() < 2 {
        return Ok((volume_spec.to_string(), None));
    }

    let host_part = parts[0];

    // If host_part starts with / or . it's a bind mount, not a named volume
    if host_part.starts_with('/') || host_part.starts_with('.') {
        return Ok((volume_spec.to_string(), None));
    }

    // Treat as named volume
    let volume_name = host_part;
    let store = VolumeStore::default_path()?;

    // Auto-create volume if it doesn't exist (Docker behavior)
    let config = match store.get(volume_name)? {
        Some(config) => config,
        None => {
            let config = VolumeConfig::new(volume_name, "");
            store.create(config)?
        }
    };

    // Replace the named volume with the host mount point path
    let mut resolved = config.mount_point.clone();
    for part in &parts[1..] {
        resolved.push(':');
        resolved.push_str(part);
    }

    Ok((resolved, Some(volume_name.to_string())))
}

/// Attach named volumes to a box in the VolumeStore.
pub fn attach_volumes(
    volume_names: &[String],
    box_id: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    if volume_names.is_empty() {
        return Ok(());
    }
    let store = VolumeStore::default_path()?;
    attach_volumes_with_store(&store, volume_names, box_id)
}

/// Attach named volumes to a box in a specific VolumeStore.
pub(crate) fn attach_volumes_with_store(
    store: &VolumeStore,
    volume_names: &[String],
    box_id: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    for name in volume_names {
        if let Some(mut config) = store.get(name)? {
            config.attach(box_id);
            store.update(&config)?;
        }
    }
    Ok(())
}

/// Detach named volumes from a box in the VolumeStore.
pub fn detach_volumes(volume_names: &[String], box_id: &str) {
    if volume_names.is_empty() {
        return;
    }
    if let Ok(store) = VolumeStore::default_path() {
        for name in volume_names {
            if let Ok(Some(mut config)) = store.get(name) {
                config.detach(box_id);
                store.update(&config).ok();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_store() -> (tempfile::TempDir, VolumeStore) {
        let dir = tempfile::tempdir().unwrap();
        let store = VolumeStore::new(dir.path().join("volumes.json"), dir.path().join("volumes"));
        (dir, store)
    }

    #[test]
    fn test_create_volume_via_store() {
        let (_dir, store) = temp_store();
        let config = VolumeConfig::new("testdata", "");
        store.create(config).unwrap();

        let loaded = store.get("testdata").unwrap().unwrap();
        assert_eq!(loaded.name, "testdata");
        assert_eq!(loaded.driver, "local");
        assert!(loaded.mount_point.contains("testdata"));
    }

    #[test]
    fn test_create_volume_with_labels() {
        let (_dir, store) = temp_store();
        let mut config = VolumeConfig::new("testdata", "");
        config.labels.insert("env".to_string(), "test".to_string());
        store.create(config).unwrap();

        let loaded = store.get("testdata").unwrap().unwrap();
        assert_eq!(loaded.labels.get("env").unwrap(), "test");
    }

    #[test]
    fn test_create_duplicate_volume_fails() {
        let (_dir, store) = temp_store();
        let c1 = VolumeConfig::new("testdata", "");
        let c2 = VolumeConfig::new("testdata", "");
        store.create(c1).unwrap();
        assert!(store.create(c2).is_err());
    }

    #[test]
    fn test_list_volumes_sorted() {
        let (_dir, store) = temp_store();
        store.create(VolumeConfig::new("zvol", "")).unwrap();
        store.create(VolumeConfig::new("avol", "")).unwrap();

        let mut list = store.list().unwrap();
        list.sort_by(|a, b| a.name.cmp(&b.name));
        assert_eq!(list[0].name, "avol");
        assert_eq!(list[1].name, "zvol");
    }

    #[test]
    fn test_remove_volume() {
        let (_dir, store) = temp_store();
        store.create(VolumeConfig::new("testdata", "")).unwrap();
        store.remove("testdata", false).unwrap();
        assert!(store.get("testdata").unwrap().is_none());
    }

    #[test]
    fn test_remove_volume_in_use_fails() {
        let (_dir, store) = temp_store();
        let created = store.create(VolumeConfig::new("testdata", "")).unwrap();
        let mut updated = created;
        updated.attach("box-1");
        store.update(&updated).unwrap();

        assert!(store.remove("testdata", false).is_err());
    }

    #[test]
    fn test_attach_volumes_with_store_attaches_existing_volumes() {
        let (_dir, store) = temp_store();
        store.create(VolumeConfig::new("testdata", "")).unwrap();

        attach_volumes_with_store(&store, &["testdata".to_string()], "box-1").unwrap();

        let updated = store.get("testdata").unwrap().unwrap();
        assert!(updated.in_use_by.contains(&"box-1".to_string()));
    }

    #[test]
    fn test_force_remove_volume_in_use() {
        let (_dir, store) = temp_store();
        let created = store.create(VolumeConfig::new("testdata", "")).unwrap();
        let mut updated = created;
        updated.attach("box-1");
        store.update(&updated).unwrap();

        store.remove("testdata", true).unwrap();
        assert!(store.get("testdata").unwrap().is_none());
    }

    #[test]
    fn test_inspect_volume() {
        let (_dir, store) = temp_store();
        let mut config = VolumeConfig::new("testdata", "");
        config.labels.insert("env".to_string(), "prod".to_string());
        store.create(config).unwrap();

        let loaded = store.get("testdata").unwrap().unwrap();
        let json = serde_json::to_string_pretty(&loaded).unwrap();
        assert!(json.contains("testdata"));
        assert!(json.contains("prod"));
    }

    #[test]
    fn test_prune_volumes() {
        let (_dir, store) = temp_store();
        store.create(VolumeConfig::new("unused1", "")).unwrap();
        store.create(VolumeConfig::new("unused2", "")).unwrap();

        let created = store.create(VolumeConfig::new("in_use", "")).unwrap();
        let mut updated = created;
        updated.attach("box-1");
        store.update(&updated).unwrap();

        let pruned = store.prune().unwrap();
        assert_eq!(pruned.len(), 2);
        assert!(pruned.contains(&"unused1".to_string()));
        assert!(pruned.contains(&"unused2".to_string()));

        let remaining = store.list().unwrap();
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].name, "in_use");
    }

    #[test]
    fn test_parse_labels() {
        let labels = vec!["env=prod".to_string(), "team=infra".to_string()];
        let mut map = std::collections::HashMap::new();
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

    #[test]
    fn test_resolve_named_volume_bind_mount() {
        // Absolute path should pass through unchanged
        let (resolved, name) = resolve_named_volume("/host/path:/guest/path").unwrap();
        assert_eq!(resolved, "/host/path:/guest/path");
        assert!(name.is_none());
    }

    #[test]
    fn test_resolve_named_volume_relative_bind() {
        // Relative path starting with . should pass through unchanged
        let (resolved, name) = resolve_named_volume("./data:/guest/data").unwrap();
        assert_eq!(resolved, "./data:/guest/data");
        assert!(name.is_none());
    }

    #[test]
    fn test_resolve_named_volume_single_part() {
        // A spec without : is not a valid mount, pass through
        let (resolved, name) = resolve_named_volume("justname").unwrap();
        assert_eq!(resolved, "justname");
        assert!(name.is_none());
    }

    #[test]
    fn test_resolve_named_volume_with_mode() {
        // Absolute path with mode should pass through unchanged
        let (resolved, name) = resolve_named_volume("/host:/guest:ro").unwrap();
        assert_eq!(resolved, "/host:/guest:ro");
        assert!(name.is_none());
    }
}
