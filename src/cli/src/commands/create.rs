//! `a3s-box create` command — Create without starting.

use clap::Args;

use super::common::{self, CommonBoxArgs};
use crate::output::parse_memory;
use crate::state::{generate_name, BoxRecord, StateFile};

#[derive(Args)]
pub struct CreateArgs {
    #[command(flatten)]
    pub common: CommonBoxArgs,

    /// Command to run when the box starts (override image CMD)
    #[arg(last = true)]
    pub cmd: Vec<String>,
}

pub async fn execute(args: CreateArgs) -> Result<(), Box<dyn std::error::Error>> {
    common::validate_runtime_options(&args.common)
        .map_err(|e| -> Box<dyn std::error::Error> { e.into() })?;

    // Validate restart policy
    let (restart_policy, max_restart_count) =
        crate::state::parse_restart_policy(&args.common.restart)
            .map_err(|e| -> Box<dyn std::error::Error> { e.into() })?;

    let memory_mb =
        parse_memory(&args.common.memory).map_err(|e| format!("Invalid --memory: {e}"))?;

    // Build resource limits before any partial moves of args
    let resource_limits = common::build_resource_limits(&args.common)?;

    let port_map = common::normalize_port_maps(&args.common.publish)
        .map_err(|e| -> Box<dyn std::error::Error> { e.into() })?;
    let env = common::build_env_map(&args.common)?;
    let labels = common::parse_env_vars(&args.common.labels)
        .map_err(|e| e.replace("environment variable", "label"))?;
    if let Some(network) = args.common.network.as_deref() {
        ensure_network_exists(network)?;
    }

    let image_config = common::cached_image_config(&args.common.image).await?;
    let health_check = common::effective_health_check(
        &args.common,
        image_config
            .as_ref()
            .and_then(|config| config.health_check.as_ref()),
    );
    let effective_stop_signal = common::effective_stop_signal(
        args.common.stop_signal.as_deref(),
        image_config
            .as_ref()
            .and_then(|config| config.stop_signal.as_deref()),
    );
    let name = args.common.name.unwrap_or_else(generate_name);

    // Parse --shm-size
    let shm_size = match &args.common.shm_size {
        Some(s) => {
            Some(common::parse_memory_bytes(s).map_err(|e| format!("Invalid --shm-size: {e}"))?)
        }
        None => None,
    };

    let box_id = uuid::Uuid::new_v4().to_string();
    let short_id = BoxRecord::make_short_id(&box_id);

    let home = a3s_box_core::dirs_home();
    let box_dir = home.join("boxes").join(&box_id);

    // Arm cleanup for the pre-registration section: create_dir_all and volume
    // resolution below can fail with `?` after the box dir already exists, and
    // until the box is registered it's invisible to `prune`/`rm`. The guard
    // removes the orphaned dir on any early return; it is disarmed right before
    // the add_record block, after which cleanup_partial_box_record takes over.
    let mut dir_guard = crate::cleanup::BoxDirGuard::new(box_dir.clone());

    // Create box directory structure
    std::fs::create_dir_all(box_dir.join("sockets"))?;
    std::fs::create_dir_all(box_dir.join("logs"))?;

    // Resolve named volumes
    let mut resolved_volumes = Vec::new();
    let mut volume_names = Vec::new();
    for vol_spec in &args.common.volumes {
        let (resolved, vol_name) = super::volume::resolve_named_volume(vol_spec)?;
        if let Some(name) = vol_name {
            volume_names.push(name);
        }
        resolved_volumes.push(resolved);
    }

    let entrypoint = args
        .common
        .entrypoint
        .as_ref()
        .map(|ep| ep.split_whitespace().map(String::from).collect::<Vec<_>>());

    // Determine network mode
    let network_mode = match &args.common.network {
        Some(name) => a3s_box_core::NetworkMode::Bridge {
            network: name.clone(),
        },
        None => a3s_box_core::NetworkMode::Tsi,
    };

    let record = BoxRecord {
        id: box_id.clone(),
        short_id: short_id.clone(),
        name: name.clone(),
        image: args.common.image.clone(),
        status: "created".to_string(),
        pid: None,
        cpus: args.common.cpus,
        memory_mb,
        volumes: resolved_volumes,
        env,
        cmd: args.cmd.clone(),
        entrypoint,
        box_dir: box_dir.clone(),
        exec_socket_path: box_dir.join("sockets").join("exec.sock"),
        console_log: box_dir.join("logs").join("console.log"),
        created_at: chrono::Utc::now(),
        started_at: None,
        auto_remove: false,
        hostname: args.common.hostname,
        user: args.common.user,
        workdir: args.common.workdir,
        restart_policy,
        port_map,
        labels,
        stopped_by_user: false,
        restart_count: 0,
        max_restart_count,
        exit_code: None,
        health_check,
        healthcheck_disabled: args.common.no_healthcheck,
        health_status: "none".to_string(),
        health_retries: 0,
        health_last_check: None,
        network_mode,
        network_name: args.common.network,
        volume_names: volume_names.clone(),
        tmpfs: args.common.tmpfs,
        anonymous_volumes: vec![],
        resource_limits,
        log_config: a3s_box_core::log::LogConfig::default(),
        add_host: args.common.add_host,
        platform: args.common.platform,
        init: args.common.init,
        read_only: args.common.read_only,
        cap_add: args.common.cap_add,
        cap_drop: args.common.cap_drop,
        security_opt: args.common.security_opt,
        privileged: args.common.privileged,
        devices: args.common.device,
        gpus: args.common.gpus,
        shm_size,
        stop_signal: effective_stop_signal,
        stop_timeout: args.common.stop_timeout,
        oom_kill_disable: args.common.oom_kill_disable,
        oom_score_adj: args.common.oom_score_adj,
    };

    let record_for_cleanup = record.clone();
    // Past the fallible pre-registration section: hand box-dir ownership to the
    // record-level cleanup below (cleanup_partial_box_record), which also clears
    // state + attached resources.
    dir_guard.disarm();

    // Atomic append under the state lock so concurrent `create`/`run` cannot
    // lose records (load_default()+add() is a lost-update race).
    if let Err(error) = StateFile::add_record(record) {
        let mut state = StateFile::load_default()?;
        crate::cleanup::cleanup_partial_box_record(&record_for_cleanup, Some(&mut state));
        return Err(error.into());
    }

    // Attach named volumes to this box
    if let Err(error) = super::volume::attach_volumes(&volume_names, &box_id) {
        let mut state = StateFile::load_default()?;
        crate::cleanup::cleanup_partial_box_record(&record_for_cleanup, Some(&mut state));
        return Err(error);
    }

    println!("{box_id}");
    Ok(())
}

fn ensure_network_exists(network: &str) -> Result<(), Box<dyn std::error::Error>> {
    let store = a3s_box_runtime::NetworkStore::default_path()?;
    let config = store
        .get(network)?
        .ok_or_else(|| format!("network '{}' not found", network))?;
    super::network::validate_attachable_network(&config)
        .map_err(|e| -> Box<dyn std::error::Error> { e.into() })?;
    Ok(())
}
