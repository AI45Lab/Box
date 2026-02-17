//! `a3s-box run` command — Pull + Create + Start.

use std::io::IsTerminal;
use std::path::PathBuf;

use a3s_box_core::config::{AgentType, BoxConfig, ResourceConfig, TeeConfig};
use a3s_box_core::event::EventEmitter;
use a3s_box_runtime::VmManager;
use clap::Args;

use super::common::{self, CommonBoxArgs};
use crate::output::parse_memory;
use crate::state::{generate_name, BoxRecord, StateFile};

#[derive(Args)]
pub struct RunArgs {
    #[command(flatten)]
    pub common: CommonBoxArgs,

    /// Run in detached mode (background)
    #[arg(short = 'd', long)]
    pub detach: bool,

    /// Keep STDIN open (interactive mode)
    #[arg(short = 'i', long = "interactive")]
    pub interactive: bool,

    /// Allocate a pseudo-TTY
    #[arg(short = 't', long = "tty")]
    pub tty: bool,

    /// Automatically remove the box when it stops
    #[arg(long)]
    pub rm: bool,

    /// Command to run (override entrypoint)
    #[arg(last = true)]
    pub cmd: Vec<String>,

    /// Logging driver (json-file, none) [default: json-file]
    #[arg(long, default_value = "json-file")]
    pub log_driver: String,

    /// Log driver options (KEY=VALUE), can be repeated
    #[arg(long = "log-opt")]
    pub log_opts: Vec<String>,

    /// Enable TEE (Trusted Execution Environment) with AMD SEV-SNP.
    /// Use --tee-simulate for development without hardware support.
    #[arg(long)]
    pub tee: bool,

    /// TEE workload identifier for attestation (default: image name)
    #[arg(long)]
    pub tee_workload_id: Option<String>,

    /// Enable TEE simulation mode (no AMD SEV-SNP hardware required)
    #[arg(long)]
    pub tee_simulate: bool,
}

pub async fn execute(args: RunArgs) -> Result<(), Box<dyn std::error::Error>> {
    let memory_mb = parse_memory(&args.common.memory).map_err(|e| format!("Invalid --memory: {e}"))?;

    // Build resource limits before any partial moves of args
    let resource_limits = common::build_resource_limits(&args.common)?;

    // Parse logging config
    let log_driver: a3s_box_core::log::LogDriver = args
        .log_driver
        .parse()
        .map_err(|e: String| format!("Invalid --log-driver: {e}"))?;
    let log_opts = common::parse_env_vars(&args.log_opts)
        .map_err(|e| e.replace("environment variable", "log option"))?;
    let log_config = a3s_box_core::log::LogConfig {
        driver: log_driver,
        options: log_opts,
    };

    let name = args.common.name.unwrap_or_else(generate_name);
    let mut env = common::parse_env_vars(&args.common.env)?;

    // Load --env-file entries (merged into env, CLI --env takes precedence)
    for env_file in &args.common.env_file {
        let file_env = common::parse_env_file(env_file)?;
        for (k, v) in file_env {
            env.entry(k).or_insert(v);
        }
    }

    let labels =
        common::parse_env_vars(&args.common.labels).map_err(|e| e.replace("environment variable", "label"))?;

    // Parse health check config (--no-healthcheck disables)
    let health_check = if args.common.no_healthcheck {
        None
    } else {
        args.common.health_cmd
            .as_ref()
            .map(|cmd| crate::state::HealthCheck {
                cmd: vec!["sh".to_string(), "-c".to_string(), cmd.clone()],
                interval_secs: args.common.health_interval,
                timeout_secs: args.common.health_timeout,
                retries: args.common.health_retries,
                start_period_secs: args.common.health_start_period,
            })
    };
    let health_status = if health_check.is_some() {
        "starting".to_string()
    } else {
        "none".to_string()
    };

    // Parse entrypoint override: split string into argv
    let entrypoint_override = args
        .common
        .entrypoint
        .as_ref()
        .map(|ep| ep.split_whitespace().map(String::from).collect::<Vec<_>>());

    // Resolve named volumes (e.g., "mydata:/app/data" → "/home/user/.a3s/volumes/mydata:/app/data")
    let mut resolved_volumes = Vec::new();
    let mut volume_names = Vec::new();
    for vol_spec in &args.common.volumes {
        let (resolved, vol_name) = super::volume::resolve_named_volume(vol_spec)?;
        if let Some(name) = vol_name {
            volume_names.push(name);
        }
        resolved_volumes.push(resolved);
    }

    // Parse --shm-size
    let shm_size = match &args.common.shm_size {
        Some(s) => Some(common::parse_memory_bytes(s).map_err(|e| format!("Invalid --shm-size: {e}"))?),
        None => None,
    };

    // Determine network mode
    let network_mode = match &args.common.network {
        Some(name) => a3s_box_core::NetworkMode::Bridge {
            network: name.clone(),
        },
        None => a3s_box_core::NetworkMode::Tsi,
    };

    // Build TEE config
    let tee = if args.tee || args.tee_simulate {
        TeeConfig::SevSnp {
            workload_id: args
                .tee_workload_id
                .clone()
                .unwrap_or_else(|| args.common.image.clone()),
            generation: Default::default(),
            simulate: args.tee_simulate,
        }
    } else {
        TeeConfig::None
    };

    // Build BoxConfig
    let config = BoxConfig {
        agent: AgentType::OciRegistry {
            reference: args.common.image.clone(),
        },
        resources: ResourceConfig {
            vcpus: args.common.cpus,
            memory_mb,
            ..Default::default()
        },
        cmd: args.cmd.clone(),
        entrypoint_override: entrypoint_override.clone(),
        volumes: resolved_volumes.clone(),
        extra_env: env.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
        port_map: args.common.publish.clone(),
        dns: args.common.dns.clone(),
        network: network_mode.clone(),
        tmpfs: args.common.tmpfs.clone(),
        resource_limits: resource_limits.clone(),
        tee,
        ..Default::default()
    };

    // Create VmManager and boot
    let emitter = EventEmitter::new(256);
    let mut vm = VmManager::new(config, emitter);
    let box_id = vm.box_id().to_string();

    println!(
        "Creating box {} ({})...",
        name,
        &BoxRecord::make_short_id(&box_id)
    );

    // Register endpoint in network store BEFORE boot so the VM can find its IP
    if let Some(ref net_name) = args.common.network {
        let net_store = a3s_box_runtime::NetworkStore::default_path()?;
        let mut net_config = net_store
            .get(net_name)?
            .ok_or_else(|| format!("network '{}' not found", net_name))?;
        let endpoint = net_config
            .connect(&box_id, &name)
            .map_err(|e| format!("Failed to connect to network: {e}"))?;
        net_store.update(&net_config)?;
        println!(
            "Connected to network {} (IP: {})",
            net_name, endpoint.ip_address
        );
    }

    vm.boot().await?;

    // Get PID from the running VM
    let pid = vm.pid().await;

    // Determine PID from handler metrics (handler holds PID internally)
    // We use the box directory structure to find PID
    let home = dirs::home_dir()
        .map(|h| h.join(".a3s"))
        .unwrap_or_else(|| PathBuf::from(".a3s"));
    let box_dir = home.join("boxes").join(&box_id);

    // Save box record
    let record = BoxRecord {
        id: box_id.clone(),
        short_id: BoxRecord::make_short_id(&box_id),
        name: name.clone(),
        image: args.common.image.clone(),
        status: "running".to_string(),
        pid,
        cpus: args.common.cpus,
        memory_mb,
        volumes: resolved_volumes.clone(),
        env,
        cmd: args.cmd.clone(),
        entrypoint: entrypoint_override.clone(),
        box_dir: box_dir.clone(),
        exec_socket_path: box_dir.join("sockets").join("exec.sock"),
        console_log: box_dir.join("logs").join("console.log"),
        created_at: chrono::Utc::now(),
        started_at: Some(chrono::Utc::now()),
        auto_remove: args.rm,
        hostname: args.common.hostname.clone(),
        user: args.common.user.clone(),
        workdir: args.common.workdir.clone(),
        restart_policy: args.common.restart.clone(),
        port_map: args.common.publish.clone(),
        labels,
        stopped_by_user: false,
        restart_count: 0,
        health_check,
        health_status,
        health_retries: 0,
        health_last_check: None,
        network_mode: network_mode.clone(),
        network_name: args.common.network.clone(),
        volume_names: volume_names.clone(),
        tmpfs: args.common.tmpfs.clone(),
        anonymous_volumes: vm.anonymous_volumes().to_vec(),
        resource_limits,
        log_config: log_config.clone(),
        add_host: args.common.add_host.clone(),
        platform: args.common.platform.clone(),
        init: args.common.init,
        read_only: args.common.read_only,
        cap_add: args.common.cap_add.clone(),
        cap_drop: args.common.cap_drop.clone(),
        security_opt: args.common.security_opt.clone(),
        privileged: args.common.privileged,
        devices: args.common.device.clone(),
        gpus: args.common.gpus.clone(),
        shm_size,
        stop_signal: args.common.stop_signal.clone(),
        stop_timeout: args.common.stop_timeout,
        oom_kill_disable: args.common.oom_kill_disable,
        oom_score_adj: args.common.oom_score_adj,
        max_restart_count: 0,
        exit_code: None,
    };

    let mut state = StateFile::load_default()?;
    state.add(record)?;

    // Spawn structured log processor (json-file driver writes container.json)
    let log_dir = box_dir.join("logs");
    let _ = std::fs::create_dir_all(&log_dir);
    let _log_handle = a3s_box_runtime::log::spawn_log_processor(
        box_dir.join("logs").join("console.log"),
        log_dir,
        log_config,
    );

    // Attach named volumes to this box
    super::volume::attach_volumes(&volume_names, &box_id)?;

    if args.detach && args.tty {
        return Err("Cannot use -t (tty) with -d (detach)".into());
    }

    if args.tty && !std::io::stdin().is_terminal() {
        return Err("The -t flag requires a terminal (stdin is not a TTY)".into());
    }

    if args.detach {
        println!("{box_id}");
        return Ok(());
    }

    // Interactive PTY mode: connect to the guest PTY server
    if args.tty {
        use a3s_box_core::pty::PtyRequest;
        use a3s_box_runtime::PtyClient;
        use crossterm::terminal;

        let pty_socket_path = box_dir.join("sockets").join("pty.sock");

        // Wait for PTY socket to appear (guest init may still be starting)
        for _ in 0..50 {
            if pty_socket_path.exists() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        if !pty_socket_path.exists() {
            return Err(format!(
                "PTY socket not found at {} (guest may not support interactive mode)",
                pty_socket_path.display()
            )
            .into());
        }

        // Build the command for the PTY session: use cmd override, entrypoint, or /bin/sh
        let pty_cmd = if !args.cmd.is_empty() {
            args.cmd.clone()
        } else if let Some(ref ep) = entrypoint_override {
            ep.clone()
        } else {
            vec!["/bin/sh".to_string()]
        };

        let (cols, rows) = terminal::size().unwrap_or((80, 24));
        let mut client = PtyClient::connect(&pty_socket_path).await?;
        client
            .send_request(&PtyRequest {
                cmd: pty_cmd,
                env: args.common.env.clone(),
                working_dir: args.common.workdir.clone(),
                user: args.common.user.clone(),
                cols,
                rows,
            })
            .await?;

        terminal::enable_raw_mode()?;
        let (read_half, write_half) = client.into_split();
        let exit_code = super::exec::run_pty_session(read_half, write_half).await;
        terminal::disable_raw_mode()?;

        // Clean up: destroy VM
        vm.destroy().await?;
        super::volume::detach_volumes(&volume_names, &box_id);
        if let Some(ref net_name) = args.common.network {
            let net_store = a3s_box_runtime::NetworkStore::default_path()?;
            if let Some(mut net_config) = net_store.get(net_name)? {
                net_config.disconnect(&box_id).ok();
                net_store.update(&net_config)?;
            }
        }

        let mut state = StateFile::load_default()?;
        if let Some(rec) = state.find_by_id_mut(&box_id) {
            rec.status = "stopped".to_string();
            rec.pid = None;
        }
        if args.rm {
            state.remove(&box_id)?;
            let _ = std::fs::remove_dir_all(&box_dir);
        } else {
            state.save()?;
        }

        if exit_code != 0 {
            std::process::exit(exit_code);
        }
        return Ok(());
    }

    // Foreground mode: tail console log and wait for Ctrl-C
    println!(
        "Box {} ({}) started. Press Ctrl-C to stop.",
        name,
        BoxRecord::make_short_id(&box_id)
    );

    let console_log = box_dir.join("logs").join("console.log");
    let shutdown = tokio::signal::ctrl_c();

    // Tail console log in background
    let log_handle = tokio::spawn(async move {
        super::tail_file(&console_log).await;
    });

    // Wait for Ctrl-C
    let _ = shutdown.await;
    println!("\nStopping box {}...", name);

    log_handle.abort();

    // Destroy VM
    vm.destroy().await?;

    // Detach named volumes
    super::volume::detach_volumes(&volume_names, &box_id);

    // Disconnect from network if connected
    if let Some(ref net_name) = args.common.network {
        let net_store = a3s_box_runtime::NetworkStore::default_path()?;
        if let Some(mut net_config) = net_store.get(net_name)? {
            net_config.disconnect(&box_id).ok();
            net_store.update(&net_config)?;
        }
    }

    // Update state
    let mut state = StateFile::load_default()?;
    if let Some(rec) = state.find_by_id_mut(&box_id) {
        rec.status = "stopped".to_string();
        rec.pid = None;
    }

    if args.rm {
        state.remove(&box_id)?;
        let _ = std::fs::remove_dir_all(&box_dir);
        println!("Box {} removed.", name);
    } else {
        state.save()?;
        println!("Box {} stopped.", name);
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- build_resource_limits tests (using new struct layout) ---

    fn default_run_args() -> RunArgs {
        RunArgs {
            common: common::CommonBoxArgs {
                image: "test".to_string(),
                name: None,
                cpus: 2,
                memory: "512m".to_string(),
                volumes: vec![],
                env: vec![],
                publish: vec![],
                dns: vec![],
                entrypoint: None,
                hostname: None,
                user: None,
                workdir: None,
                restart: "no".to_string(),
                labels: vec![],
                tmpfs: vec![],
                network: None,
                health_cmd: None,
                health_interval: 30,
                health_timeout: 5,
                health_retries: 3,
                health_start_period: 0,
                pids_limit: None,
                cpuset_cpus: None,
                ulimits: vec![],
                cpu_shares: None,
                cpu_quota: None,
                cpu_period: None,
                memory_reservation: None,
                memory_swap: None,
                env_file: vec![],
                add_host: vec![],
                platform: None,
                init: false,
                read_only: false,
                cap_add: vec![],
                cap_drop: vec![],
                security_opt: vec![],
                privileged: false,
                device: vec![],
                gpus: None,
                shm_size: None,
                stop_signal: None,
                stop_timeout: None,
                no_healthcheck: false,
                oom_kill_disable: false,
                oom_score_adj: None,
            },
            detach: false,
            interactive: false,
            tty: false,
            rm: false,
            cmd: vec![],
            log_driver: "json-file".to_string(),
            log_opts: vec![],
            tee: false,
            tee_workload_id: None,
            tee_simulate: false,
        }
    }

    #[test]
    fn test_build_resource_limits_defaults() {
        let args = default_run_args();
        let limits = common::build_resource_limits(&args.common).unwrap();
        assert!(limits.pids_limit.is_none());
        assert!(limits.cpuset_cpus.is_none());
        assert!(limits.cpu_shares.is_none());
        assert!(limits.memory_reservation.is_none());
        assert!(limits.memory_swap.is_none());
    }

    #[test]
    fn test_build_resource_limits_with_values() {
        let mut args = default_run_args();
        args.common.pids_limit = Some(100);
        args.common.cpuset_cpus = Some("0-3".to_string());
        args.common.ulimits = vec!["nofile=1024:4096".to_string()];
        args.common.cpu_shares = Some(512);
        args.common.cpu_quota = Some(50000);
        args.common.cpu_period = Some(100000);
        args.common.memory_reservation = Some("256m".to_string());
        args.common.memory_swap = Some("-1".to_string());

        let limits = common::build_resource_limits(&args.common).unwrap();
        assert_eq!(limits.pids_limit, Some(100));
        assert_eq!(limits.cpuset_cpus, Some("0-3".to_string()));
        assert_eq!(limits.cpu_shares, Some(512));
        assert_eq!(limits.cpu_quota, Some(50000));
        assert_eq!(limits.cpu_period, Some(100000));
        assert_eq!(limits.memory_reservation, Some(256 * 1024 * 1024));
        assert_eq!(limits.memory_swap, Some(-1));
    }

    #[test]
    fn test_build_resource_limits_memory_swap_value() {
        let mut args = default_run_args();
        args.common.memory_swap = Some("1g".to_string());

        let limits = common::build_resource_limits(&args.common).unwrap();
        assert_eq!(limits.memory_swap, Some(1024 * 1024 * 1024));
    }
}
