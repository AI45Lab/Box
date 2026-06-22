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
    /// Evict old snapshots to bound disk usage
    Prune(SnapshotPruneArgs),
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
    /// Remove even if a restored box still references the snapshot as its
    /// copy-on-write overlay lower (`.snapshot-lower`).
    #[arg(long, short)]
    pub force: bool,
}

/// Arguments for `snapshot inspect`.
#[derive(Parser)]
pub struct SnapshotInspectArgs {
    /// Snapshot ID to inspect
    pub id: String,
}

/// Arguments for `snapshot prune`.
#[derive(Parser)]
pub struct SnapshotPruneArgs {
    /// Keep at most this many newest snapshots (0 = no count limit)
    #[arg(long, default_value = "0")]
    pub keep: usize,
    /// Evict oldest snapshots until the total size is under this many bytes
    /// (0 = no size limit)
    #[arg(long = "max-bytes", default_value = "0")]
    pub max_bytes: u64,
}

/// Execute a snapshot command.
pub async fn execute(args: SnapshotArgs) -> Result<(), Box<dyn std::error::Error>> {
    match args.action {
        SnapshotAction::Create(a) => execute_create(a).await,
        SnapshotAction::Restore(a) => execute_restore(a).await,
        SnapshotAction::Ls(a) => execute_ls(a).await,
        SnapshotAction::Rm(a) => execute_rm(a).await,
        SnapshotAction::Inspect(a) => execute_inspect(a).await,
        SnapshotAction::Prune(a) => execute_prune(a).await,
    }
}

/// Prune snapshots to bound disk usage. Explicit operator action (no surprise
/// auto-deletion): wires the `SnapshotStore::prune` primitive — which was
/// implemented and unit-tested but had no caller, so `max_snapshots`/
/// `max_total_bytes` were inert and scheduled/per-CI snapshots grew the host
/// disk unbounded with only manual `snapshot rm` as recourse.
async fn execute_prune(args: SnapshotPruneArgs) -> Result<(), Box<dyn std::error::Error>> {
    use a3s_box_runtime::SnapshotStore;

    if args.keep == 0 && args.max_bytes == 0 {
        return Err("specify --keep <N> and/or --max-bytes <BYTES> to prune".into());
    }

    let store = SnapshotStore::default_path()?;
    let removed = store.prune(args.keep, args.max_bytes)?;
    if removed.is_empty() {
        println!("Nothing to prune");
    } else {
        for id in &removed {
            println!("{id}");
        }
        println!("Pruned {} snapshot(s)", removed.len());
    }
    Ok(())
}

/// Create a snapshot from a box.
async fn execute_create(args: SnapshotCreateArgs) -> Result<(), Box<dyn std::error::Error>> {
    use crate::state::StateFile;
    use a3s_box_core::snapshot::SnapshotMetadata;
    use a3s_box_runtime::SnapshotStore;

    let state = StateFile::load_default()?;

    // Resolve box by ID, short ID, or name
    let record = resolve_box(&state, &args.box_id)?;

    // Generate snapshot ID and name
    let snap_id = format!(
        "snap-{}",
        chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0)
    );
    let snap_name = args
        .name
        .unwrap_or_else(|| format!("{}-snapshot", record.name));

    // Build metadata from box record
    let mut meta =
        SnapshotMetadata::new(snap_id, snap_name, record.id.clone(), record.image.clone());
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

    // Snapshot the box's current root filesystem (overlay `merged` or the plain
    // provider's `rootfs`), so runtime changes are captured — not an empty dir.
    let rootfs_path = super::resolve_box_rootfs(&record.box_dir).ok_or_else(|| {
        format!(
            "Rootfs not found for box '{}' under {} (looked for merged/ and rootfs/); \
             snapshot a running box",
            record.name,
            record.box_dir.display()
        )
    })?;
    let store = SnapshotStore::default_path()?;
    let saved = store.save(meta, &rootfs_path)?;

    // Opt-in auto-prune: when A3S_BOX_MAX_SNAPSHOTS / A3S_BOX_MAX_SNAPSHOT_BYTES are
    // set, evict the oldest snapshots beyond the cap after each create so a
    // scheduled/per-CI snapshot workflow self-bounds the host disk (each snapshot
    // deep-copies the rootfs — hundreds of MB-GB). Unset = no auto-prune (unchanged
    // behaviour); `snapshot prune` remains the explicit operator tool.
    let max_count = std::env::var("A3S_BOX_MAX_SNAPSHOTS")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(0);
    let max_bytes = std::env::var("A3S_BOX_MAX_SNAPSHOT_BYTES")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(0);
    if max_count > 0 || max_bytes > 0 {
        if let Err(e) = store.prune(max_count, max_bytes) {
            eprintln!("warning: snapshot auto-prune failed: {e}");
        }
    }

    println!("{}", saved.id);
    Ok(())
}

/// Restore a box from a snapshot.
async fn execute_restore(args: SnapshotRestoreArgs) -> Result<(), Box<dyn std::error::Error>> {
    use crate::state::{generate_name, BoxRecord, StateFile};
    use a3s_box_runtime::SnapshotStore;

    let store = SnapshotStore::default_path()?;

    // Find snapshot by ID or name
    let meta = resolve_snapshot(&store, &args.snapshot)?;

    // Create a new box record from snapshot metadata
    let box_id = uuid::Uuid::new_v4().to_string();
    let box_name = args.name.unwrap_or_else(generate_name);
    let short_id = BoxRecord::make_short_id(&box_id);

    let home = a3s_box_core::dirs_home();
    let box_dir = home.join("boxes").join(&box_id);
    let socket_dir = box_dir.join("sockets");
    let logs_dir = box_dir.join("logs");

    // Arm cleanup: every step below (create dirs, write marker, register) can
    // fail with `?`. Until the box is in the state file it is invisible to
    // `prune`/`rm`, so a half-created box dir would leak on disk forever. The
    // guard removes it on any early return and is disarmed once registered.
    let mut dir_guard = crate::cleanup::BoxDirGuard::new(box_dir.clone());

    std::fs::create_dir_all(&socket_dir)?;
    std::fs::create_dir_all(&logs_dir)?;

    // Point the restored box's overlay at the snapshot's pristine stored rootfs
    // as a read-only lower (copy-on-write): the box writes to its own upper, the
    // snapshot stays shared and untouched across all forks, and nothing is
    // copied. The runtime reads `.snapshot-lower` in `prepare_layout` and mounts
    // the overlay (or, on a non-overlay host, the CopyProvider falls back to a
    // full copy — same result, slower). This replaces a full per-restore rootfs
    // deep-copy, so forking a warmed snapshot is near-instant and space-cheap.
    let snap_rootfs = store.rootfs_path(&meta.id);
    if snap_rootfs.exists() {
        std::fs::write(
            box_dir.join(".snapshot-lower"),
            snap_rootfs.to_string_lossy().as_bytes(),
        )?;
    }

    let record = BoxRecord {
        id: box_id.clone(),
        short_id,
        name: box_name,
        image: meta.image.clone(),
        status: "created".to_string(),
        pid: None,
        pid_start_time: None,
        cpus: meta.vcpus,
        memory_mb: meta.memory_mb,
        volumes: meta.volumes.clone(),
        env: meta.env.clone(),
        cmd: meta.cmd.clone(),
        entrypoint: meta.entrypoint.clone(),
        box_dir: box_dir.clone(),
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
        healthcheck_disabled: false,
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

    // Atomic append under the state lock so a concurrent writer (run/monitor/
    // compose/health) cannot clobber this registration with a stale snapshot.
    StateFile::add_record(record)?;
    dir_guard.disarm(); // registered — the box dir is now owned by the record

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
        "{:<30} {:<20} {:<15} {:<12} {:<10} CREATED",
        "SNAPSHOT ID", "NAME", "SOURCE BOX", "IMAGE", "SIZE"
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
///
/// Refuses to remove a snapshot that a restored box still references as its
/// copy-on-write overlay lower (`.snapshot-lower`): the snapshot's rootfs is
/// shared read-only into every fork, so deleting it would break a live overlay
/// (ESTALE) or stop a restored box from re-starting. Pass `--force` to override.
async fn execute_rm(args: SnapshotRmArgs) -> Result<(), Box<dyn std::error::Error>> {
    use crate::state::StateFile;
    use a3s_box_runtime::SnapshotStore;

    let store = SnapshotStore::default_path()?;
    let state = StateFile::load_default()?;

    let mut refused = false;
    for id in &args.ids {
        if !args.force {
            let users = boxes_referencing_snapshot(&state, &store.rootfs_path(id));
            if !users.is_empty() {
                refused = true;
                eprintln!(
                    "Cannot remove snapshot '{}': still used as a copy-on-write lower by box(es): {}. \
                     Remove the box(es) first, or re-run with --force.",
                    id,
                    users.join(", ")
                );
                continue;
            }
        }
        if store.delete(id)? {
            println!("{}", id);
        } else {
            eprintln!("Snapshot '{}' not found", id);
        }
    }

    if refused {
        return Err("one or more snapshots are still in use (not removed)".into());
    }
    Ok(())
}

/// Names of boxes whose `.snapshot-lower` marker points at `snap_rootfs`.
fn boxes_referencing_snapshot(
    state: &crate::state::StateFile,
    snap_rootfs: &std::path::Path,
) -> Vec<String> {
    state
        .records()
        .iter()
        .filter(|r| box_references_lower(&r.box_dir, snap_rootfs))
        .map(|r| r.name.clone())
        .collect()
}

/// Whether the box at `box_dir` references `snap_rootfs` as its CoW overlay lower.
fn box_references_lower(box_dir: &std::path::Path, snap_rootfs: &std::path::Path) -> bool {
    std::fs::read_to_string(box_dir.join(".snapshot-lower"))
        .map(|s| std::path::Path::new(s.trim()) == snap_rootfs)
        .unwrap_or(false)
}

/// Inspect a snapshot.
async fn execute_inspect(args: SnapshotInspectArgs) -> Result<(), Box<dyn std::error::Error>> {
    use a3s_box_runtime::SnapshotStore;

    let store = SnapshotStore::default_path()?;
    let meta = store
        .get(&args.id)?
        .ok_or_else(|| format!("Snapshot '{}' not found", args.id))?;

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
        1 => {
            // Safe: len() == 1 guarantees next() returns Some
            Ok(by_name.into_iter().next().expect("len checked"))
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_box_references_lower() {
        let tmp = tempfile::TempDir::new().unwrap();
        let box_dir = tmp.path();
        let snap = std::path::PathBuf::from("/root/.a3s/snapshots/snap-1/rootfs");
        // no marker -> not referencing
        assert!(!box_references_lower(box_dir, &snap));
        // marker points elsewhere -> not referencing
        std::fs::write(box_dir.join(".snapshot-lower"), "/other/snap/rootfs").unwrap();
        assert!(!box_references_lower(box_dir, &snap));
        // marker points at the snapshot (trailing whitespace tolerated) -> referencing
        std::fs::write(
            box_dir.join(".snapshot-lower"),
            "/root/.a3s/snapshots/snap-1/rootfs\n",
        )
        .unwrap();
        assert!(box_references_lower(box_dir, &snap));
    }

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
