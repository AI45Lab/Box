//! `a3s-box create` command — Create without starting.

use std::path::PathBuf;

use clap::Args;

use super::common::{self, CommonBoxArgs};
use crate::output::parse_memory;
use crate::state::{generate_name, BoxRecord, StateFile};

#[derive(Args)]
pub struct CreateArgs {
    #[command(flatten)]
    pub common: CommonBoxArgs,
}

pub async fn execute(args: CreateArgs) -> Result<(), Box<dyn std::error::Error>> {
    // Validate restart policy
    let (restart_policy, max_restart_count) =
        crate::state::parse_restart_policy(&args.common.restart)
            .map_err(|e| -> Box<dyn std::error::Error> { e.into() })?;

    let memory_mb =
        parse_memory(&args.common.memory).map_err(|e| format!("Invalid --memory: {e}"))?;

    // Build resource limits before any partial moves of args
    let resource_limits = common::build_resource_limits(&args.common)?;

    let name = args.common.name.unwrap_or_else(generate_name);
    let mut env = common::parse_env_vars(&args.common.env)?;

    // Load --env-file entries (merged into env, CLI --env takes precedence)
    for env_file in &args.common.env_file {
        let file_env = common::parse_env_file(env_file)?;
        for (k, v) in file_env {
            env.entry(k).or_insert(v);
        }
    }

    let labels = common::parse_env_vars(&args.common.labels)
        .map_err(|e| e.replace("environment variable", "label"))?;

    // Parse health check config (--no-healthcheck disables)
    let health_check = if args.common.no_healthcheck {
        None
    } else {
        args.common
            .health_cmd
            .as_ref()
            .map(|cmd| crate::state::HealthCheck {
                cmd: vec!["sh".to_string(), "-c".to_string(), cmd.clone()],
                interval_secs: args.common.health_interval,
                timeout_secs: args.common.health_timeout,
                retries: args.common.health_retries,
                start_period_secs: args.common.health_start_period,
            })
    };

    // Parse --shm-size
    let shm_size = match &args.common.shm_size {
        Some(s) => {
            Some(common::parse_memory_bytes(s).map_err(|e| format!("Invalid --shm-size: {e}"))?)
        }
        None => None,
    };

    let box_id = uuid::Uuid::new_v4().to_string();
    let short_id = BoxRecord::make_short_id(&box_id);

    let home = dirs::home_dir()
        .map(|h| h.join(".a3s"))
        .unwrap_or_else(|| PathBuf::from(".a3s"));
    let box_dir = home.join("boxes").join(&box_id);

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
        cmd: vec![],
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
        port_map: args.common.publish,
        labels,
        stopped_by_user: false,
        restart_count: 0,
        max_restart_count,
        exit_code: None,
        health_check,
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
        stop_signal: args.common.stop_signal,
        stop_timeout: args.common.stop_timeout,
        oom_kill_disable: args.common.oom_kill_disable,
        oom_score_adj: args.common.oom_score_adj,
    };

    let mut state = StateFile::load_default()?;
    state.add(record)?;

    // Attach named volumes to this box
    super::volume::attach_volumes(&volume_names, &box_id)?;

    println!("{box_id}");
    Ok(())
}
