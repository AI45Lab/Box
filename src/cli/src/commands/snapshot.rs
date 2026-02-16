//! CLI commands for VM snapshot management.
//!
//! Provides `a3s-box snapshot create/restore/ls/rm/inspect` commands.

use clap::{Parser, Subcommand};

/// Manage VM snapshots.
#[derive(Parser)]
pub struct SnapshotArgs {
    #[command(subcommand)]
    pub action: SnapshotAction,
}

/// Snapshot subcommands.
#[derive(Subcommand)]
pub enum SnapshotAction {
    /// Create a snapshot from a running or stopped box
    Create(SnapshotCreateArgs),
    /// Restore a box from a snapshot
    Restore(SnapshotRestoreArgs),
    /// List all snapshots
    Ls(SnapshotLsArgs),
    /// Remove a snapshot
    Rm(SnapshotRmArgs),
    /// Display detailed snapshot information
    Inspect(SnapshotInspectArgs),
}

/// Arguments for `snapshot create`.
#[derive(Parser)]
pub struct SnapshotCreateArgs {
    /// Box ID or name to snapshot
    pub box_id: String,
    /// Snapshot name
    #[arg(long)]
    pub name: Option<String>,
    /// Description
    #[arg(long)]
    pub description: Option<String>,
}

/// Arguments for `snapshot restore`.
#[derive(Parser)]
pub struct SnapshotRestoreArgs {
    /// Snapshot ID or name to restore from
    pub snapshot: String,
    /// Name for the restored box
    #[arg(long)]
    pub name: Option<String>,
}

/// Arguments for `snapshot ls`.
#[derive(Parser)]
pub struct SnapshotLsArgs {
    /// Output as JSON
    #[arg(long)]
    pub json: bool,
}

/// Arguments for `snapshot rm`.
#[derive(Parser)]
pub struct SnapshotRmArgs {
    /// Snapshot ID(s) to remove
    pub ids: Vec<String>,
}

/// Arguments for `snapshot inspect`.
#[derive(Parser)]
pub struct SnapshotInspectArgs {
    /// Snapshot ID to inspect
    pub id: String,
}

/// Execute a snapshot command.
pub async fn execute(args: SnapshotArgs) -> Result<(), Box<dyn std::error::Error>> {
    match args.action {
        SnapshotAction::Create(a) => execute_create(a).await,
        SnapshotAction::Restore(a) => execute_restore(a).await,
        SnapshotAction::Ls(a) => execute_ls(a).await,
        SnapshotAction::Rm(a) => execute_rm(a).await,
        SnapshotAction::Inspect(a) => execute_inspect(a).await,
    }
}

/// Create a snapshot from a box.
async fn execute_create(args: SnapshotCreateArgs) -> Result<(), Box<dyn std::error::Error>> {
    use a3s_box_core::snapshot::SnapshotMetadata;
    use a3s_box_runtime::SnapshotStore;
    use crate::state::StateFile;

    let state = StateFile::load_default()?;

    // Resolve box by ID, short ID, or name
    let record = resolve_box(&state, &args.box_id)?;

    // Generate snapshot ID and name
    let snap_id = format!(
        "snap-{}",
        chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0)
    );
    let snap_name = args.name.unwrap_or_else(|| {
        format!("{}-snapshot", record.name)
    });

    // Build metadata from box record
    let mut meta = SnapshotMetadata::new(
        snap_id,
        snap_name,
        record.id.clone(),
        record.image.clone(),
    );
    meta.vcpus = record.cpus;
    meta.memory_mb = record.memory_mb;
    meta.volumes = record.volumes.clone();
    meta.env = record.env.clone();
    meta.cmd = record.cmd.clone();
    meta.entrypoint = record.entrypoint.clone();
    meta.workdir = record.workdir.clone();
    meta.port_map = record.port_map.clone();
    meta.labels = record.labels.clone();
    if let Some(ref desc) = args.description {
        meta.description = desc.clone();
    }

    // Snapshot the rootfs
    let rootfs_path = record.box_dir.join("rootfs");
    let store = SnapshotStore::default_path()?;
    let saved = store.save(meta, &rootfs_path)?;

    println!("{}", saved.id);
    Ok(())
}

/// Restore a box from a snapshot.
async fn execute_restore(args: SnapshotRestoreArgs) -> Result<(), Box<dyn std::error::Error>> {
    use a3s_box_runtime::SnapshotStore;
    use crate::state::{BoxRecord, StateFile, generate_name};

    let store = SnapshotStore::default_path()?;

    // Find snapshot by ID or name
    let meta = resolve_snapshot(&store, &args.snapshot)?;

    // Create a new box record from snapshot metadata
    let box_id = uuid::Uuid::new_v4().to_string();
    let box_name = args.name.unwrap_or_else(generate_name);
    let short_id = BoxRecord::make_short_id(&box_id);

    let home = dirs::home_dir()
        .map(|h| h.join(".a3s"))
        .unwrap_or_else(|| std::path::PathBuf::from(".a3s"));
    let box_dir = home.join("boxes").join(&box_id);
    let socket_dir = box_dir.join("sockets");
    let logs_dir = box_dir.join("logs");

    std::fs::create_dir_all(&socket_dir)?;
    std::fs::create_dir_all(&logs_dir)?;

    // Copy snapshot rootfs to new box
    let snap_rootfs = store.rootfs_path(&meta.id);
    let box_rootfs = box_dir.join("rootfs");
    if snap_rootfs.exists() {
        copy_dir_recursive_io(&snap_rootfs, &box_rootfs)?;
    }

    let record = BoxRecord {
        id: box_id.clone(),
        short_id,
        name: box_name,
        image: meta.image.clone(),
        status: "created".to_string(),
        pid: None,
        cpus: meta.vcpus,
        memory_mb: meta.memory_mb,
        volumes: meta.volumes.clone(),
        env: meta.env.clone(),
        cmd: meta.cmd.clone(),
        entrypoint: meta.entrypoint.clone(),
        box_dir: box_dir.clone(),
        socket_path: socket_dir.join("grpc.sock"),
        exec_socket_path: socket_dir.join("exec.sock"),
        console_log: logs_dir.join("console.log"),
        created_at: chrono::Utc::now(),
        started_at: None,
        auto_remove: false,
        hostname: None,
        user: None,
        workdir: meta.workdir.clone(),
        restart_policy: "no".to_string(),
        port_map: meta.port_map.clone(),
        labels: meta.labels.clone(),
        stopped_by_user: false,
        restart_count: 0,
        max_restart_count: 0,
        exit_code: None,
        health_check: None,
        health_status: "none".to_string(),
        health_retries: 0,
        health_last_check: None,
        network_mode: a3s_box_core::NetworkMode::default(),
        network_name: None,
        volume_names: vec![],
        tmpfs: vec![],
        anonymous_volumes: vec![],
        resource_limits: a3s_box_core::config::ResourceLimits::default(),
        log_config: a3s_box_core::log::LogConfig::default(),
        add_host: vec![],
        platform: None,
        init: false,
        read_only: false,
        cap_add: vec![],
        cap_drop: vec![],
        security_opt: vec![],
        privileged: false,
        devices: vec![],
        gpus: None,
        shm_size: None,
        stop_signal: None,
        stop_timeout: None,
        oom_kill_disable: false,
        oom_score_adj: None,
    };

    let mut state = StateFile::load_default()?;
    state.add(record)?;

    println!("{}", box_id);
    Ok(())
}

/// List all snapshots.
async fn execute_ls(args: SnapshotLsArgs) -> Result<(), Box<dyn std::error::Error>> {
    use a3s_box_runtime::SnapshotStore;

    let store = SnapshotStore::default_path()?;
    let snapshots = store.list()?;

    if args.json {
        println!("{}", serde_json::to_string_pretty(&snapshots)?);
        return Ok(());
    }

    if snapshots.is_empty() {
        println!("No snapshots found.");
        return Ok(());
    }

    println!(
        "{:<30} {:<20} {:<15} {:<12} {:<10} {}",
        "SNAPSHOT ID", "NAME", "SOURCE BOX", "IMAGE", "SIZE", "CREATED"
    );
    for snap in &snapshots {
        let size = format_size(snap.size_bytes);
        let created = snap.created_at.format("%Y-%m-%d %H:%M").to_string();
        let short_source = if snap.source_box_id.len() > 12 {
            &snap.source_box_id[..12]
        } else {
            &snap.source_box_id
        };
        let short_image = if snap.image.len() > 10 {
            &snap.image[..10]
        } else {
            &snap.image
        };
        println!(
            "{:<30} {:<20} {:<15} {:<12} {:<10} {}",
            snap.id, snap.name, short_source, short_image, size, created
        );
    }

    Ok(())
}

/// Remove snapshots.
async fn execute_rm(args: SnapshotRmArgs) -> Result<(), Box<dyn std::error::Error>> {
    use a3s_box_runtime::SnapshotStore;

    let store = SnapshotStore::default_path()?;

    for id in &args.ids {
        if store.delete(id)? {
            println!("{}", id);
        } else {
            eprintln!("Snapshot '{}' not found", id);
        }
    }

    Ok(())
}

/// Inspect a snapshot.
async fn execute_inspect(args: SnapshotInspectArgs) -> Result<(), Box<dyn std::error::Error>> {
    use a3s_box_runtime::SnapshotStore;

    let store = SnapshotStore::default_path()?;
    let meta = store.get(&args.id)?.ok_or_else(|| {
        format!("Snapshot '{}' not found", args.id)
    })?;

    println!("{}", serde_json::to_string_pretty(&meta)?);
    Ok(())
}

/// Resolve a box by ID, short ID, or name.
fn resolve_box<'a>(
    state: &'a crate::state::StateFile,
    id_or_name: &str,
) -> Result<&'a crate::state::BoxRecord, Box<dyn std::error::Error>> {
    // Try exact ID
    if let Some(record) = state.find_by_id(id_or_name) {
        return Ok(record);
    }
    // Try name
    if let Some(record) = state.find_by_name(id_or_name) {
        return Ok(record);
    }
    // Try prefix
    let matches = state.find_by_id_prefix(id_or_name);
    match matches.len() {
        0 => Err(format!("No box found matching '{}'", id_or_name).into()),
        1 => Ok(matches[0]),
        n => Err(format!(
            "Ambiguous box reference '{}': matches {} boxes",
            id_or_name, n
        )
        .into()),
    }
}

/// Resolve a snapshot by ID or name.
fn resolve_snapshot(
    store: &a3s_box_runtime::SnapshotStore,
    id_or_name: &str,
) -> Result<a3s_box_core::snapshot::SnapshotMetadata, Box<dyn std::error::Error>> {
    // Try exact ID
    if let Some(meta) = store.get(id_or_name)? {
        return Ok(meta);
    }
    // Try by name
    let all = store.list()?;
    let by_name: Vec<_> = all.into_iter().filter(|s| s.name == id_or_name).collect();
    match by_name.len() {
        0 => Err(format!("No snapshot found matching '{}'", id_or_name).into()),
        1 => Ok(by_name.into_iter().next().unwrap()),
        n => Err(format!(
            "Ambiguous snapshot reference '{}': matches {} snapshots",
            id_or_name, n
        )
        .into()),
    }
}

/// Format bytes as human-readable size.
fn format_size(bytes: u64) -> String {
    if bytes >= 1024 * 1024 * 1024 {
        format!("{:.1}GB", bytes as f64 / (1024.0 * 1024.0 * 1024.0))
    } else if bytes >= 1024 * 1024 {
        format!("{:.1}MB", bytes as f64 / (1024.0 * 1024.0))
    } else if bytes >= 1024 {
        format!("{:.1}KB", bytes as f64 / 1024.0)
    } else {
        format!("{}B", bytes)
    }
}

/// Simple recursive directory copy (std::io version for CLI).
fn copy_dir_recursive_io(
    src: &std::path::Path,
    dst: &std::path::Path,
) -> std::io::Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let src_path = entry.path();
        let dst_path = dst.join(entry.file_name());
        if src_path.is_dir() {
            copy_dir_recursive_io(&src_path, &dst_path)?;
        } else {
            std::fs::copy(&src_path, &dst_path)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_format_size_bytes() {
        assert_eq!(format_size(0), "0B");
        assert_eq!(format_size(512), "512B");
    }

    #[test]
    fn test_format_size_kb() {
        assert_eq!(format_size(1024), "1.0KB");
        assert_eq!(format_size(2560), "2.5KB");
    }

    #[test]
    fn test_format_size_mb() {
        assert_eq!(format_size(1024 * 1024), "1.0MB");
        assert_eq!(format_size(5 * 1024 * 1024), "5.0MB");
    }

    #[test]
    fn test_format_size_gb() {
        assert_eq!(format_size(1024 * 1024 * 1024), "1.0GB");
        assert_eq!(format_size(2 * 1024 * 1024 * 1024), "2.0GB");
    }
}
