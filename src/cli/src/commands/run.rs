//! `a3s-box run` command — Pull + Create + Start.

use std::io::IsTerminal;
use std::path::PathBuf;

use a3s_box_core::config::{BoxConfig, ResourceConfig, SidecarConfig, TeeConfig};
use a3s_box_core::event::EventEmitter;
use a3s_box_core::vmm::{parse_signal_name, DEFAULT_SHUTDOWN_TIMEOUT_MS};
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

    /// Sidecar OCI image to run alongside the main container inside the VM.
    /// Intended for security proxies such as SafeClaw.
    /// Example: --sidecar ghcr.io/a3s-lab/safeclaw:latest
    #[arg(long)]
    pub sidecar: Option<String>,

    /// Vsock port for the sidecar process (default: 4092)
    #[arg(long, default_value = "4092")]
    pub sidecar_vsock_port: u32,
}

/// Intermediate state produced by the setup phase, consumed by the run phase.
struct RunContext {
    vm: VmManager,
    box_id: String,
    box_dir: PathBuf,
    name: String,
    volume_names: Vec<String>,
    health_checker: Option<tokio::task::JoinHandle<()>>,
    stop_signal: i32,
    stop_timeout_ms: u64,
}

pub async fn execute(args: RunArgs) -> Result<(), Box<dyn std::error::Error>> {
    let ctx = setup_and_boot(&args).await?;

    if args.detach && args.tty {
        return Err("Cannot use -t (tty) with -d (detach)".into());
    }
    if args.tty && !std::io::stdin().is_terminal() {
        return Err("The -t flag requires a terminal (stdin is not a TTY)".into());
    }

    if args.detach {
        println!("{}", ctx.box_id);
        return Ok(());
    }

    if args.tty {
        return run_tty(ctx, &args).await;
    }

    run_foreground(ctx, &args).await
}

// ============================================================================
// Phase 1: Parse args, build config, boot VM, save state
// ============================================================================

async fn setup_and_boot(args: &RunArgs) -> Result<RunContext, Box<dyn std::error::Error>> {
    let memory_mb =
        parse_memory(&args.common.memory).map_err(|e| format!("Invalid --memory: {e}"))?;
    let resource_limits = common::build_resource_limits(&args.common)?;

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

    let name = args.common.name.clone().unwrap_or_else(generate_name);
    let mut env = common::parse_env_vars(&args.common.env)?;
    for env_file in &args.common.env_file {
        for (k, v) in common::parse_env_file(env_file)? {
            env.entry(k).or_insert(v);
        }
    }
    let labels = common::parse_env_vars(&args.common.labels)
        .map_err(|e| e.replace("environment variable", "label"))?;
    let health_check = parse_health_check(&args.common);
    let entrypoint_override = args
        .common
        .entrypoint
        .as_ref()
        .map(|ep| ep.split_whitespace().map(String::from).collect::<Vec<_>>());
    let (resolved_volumes, volume_names) = resolve_volumes(&args.common.volumes)?;

    // Parse --shm-size once; reuse for both tmpfs entry and the box record.
    let shm_size = match &args.common.shm_size {
        Some(s) => {
            Some(common::parse_memory_bytes(s).map_err(|e| format!("Invalid --shm-size: {e}"))?)
        }
        None => None,
    };
    let mut tmpfs = args.common.tmpfs.clone();
    if let Some(size_bytes) = shm_size {
        tmpfs.push(format!("/dev/shm:size={}", size_bytes));
    }

    let network_mode = match &args.common.network {
        Some(name) => a3s_box_core::NetworkMode::Bridge {
            network: name.clone(),
        },
        None => a3s_box_core::NetworkMode::Tsi,
    };
    let tee = build_tee_config(args);

    if !args.common.device.is_empty() {
        eprintln!("Warning: --device is not supported by the libkrun backend (devices are stored but not passed through)");
    }
    if args.common.gpus.is_some() {
        eprintln!("Warning: --gpus is not yet implemented (stored but not enforced)");
    }

    let config = build_box_config(
        args,
        memory_mb,
        resource_limits.clone(),
        entrypoint_override.clone(),
        resolved_volumes.clone(),
        env.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
        network_mode.clone(),
        tmpfs,
        tee,
    );

    let emitter = EventEmitter::new(256);
    let mut vm = VmManager::new(config, emitter);
    let box_id = vm.box_id().to_string();
    println!(
        "Creating box {} ({})...",
        name,
        &BoxRecord::make_short_id(&box_id)
    );

    let image_name = args.common.image.clone();
    vm.set_pull_progress_fn(std::sync::Arc::new(move |current, total, digest, size| {
        if current == 1 && size > 0 {
            println!("Pulling {}...", image_name);
        }
        let short = &digest[digest.len().saturating_sub(12)..];
        if size < 0 {
            // Negative size signals completion
            let actual_size = (-size) as i64;
            let size_str = if actual_size >= 1_048_576 {
                format!("{:.1} MB", actual_size as f64 / 1_048_576.0)
            } else if actual_size >= 1024 {
                format!("{:.1} KB", actual_size as f64 / 1024.0)
            } else {
                format!("{} B", actual_size)
            };
            println!("  [{current}/{total}] {short}: {size_str} ✓");
        } else {
            // Positive size means downloading - just show once
            let size_str = if size >= 1_048_576 {
                format!("{:.1} MB", size as f64 / 1_048_576.0)
            } else if size >= 1024 {
                format!("{:.1} KB", size as f64 / 1024.0)
            } else {
                format!("{} B", size)
            };
            println!("  [{current}/{total}] {short}: Pulling {size_str}...");
        }
    }));

    connect_network(args.common.network.as_deref(), &box_id, &name)?;
    vm.boot().await?;

    let pid = vm.pid().await;
    let box_dir = a3s_box_core::dirs_home().join("boxes").join(&box_id);

    let health_status = if health_check.is_some() {
        "starting"
    } else {
        "none"
    };
    let record = BoxRecord {
        id: box_id.clone(),
        short_id: BoxRecord::make_short_id(&box_id),
        name: name.clone(),
        image: args.common.image.clone(),
        status: "running".to_string(),
        pid,
        cpus: args.common.cpus,
        memory_mb,
        volumes: resolved_volumes,
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
        health_check: health_check.clone(),
        health_status: health_status.to_string(),
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
    StateFile::load_default()?.add(record)?;

    let log_dir = box_dir.join("logs");
    let _ = std::fs::create_dir_all(&log_dir);
    let _log_handle = a3s_box_runtime::log::spawn_log_processor(
        box_dir.join("logs").join("console.log"),
        log_dir,
        log_config,
    );

    let health_checker = health_check.as_ref().map(|hc| {
        crate::health::spawn_health_checker(
            box_id.clone(),
            box_dir.join("sockets").join("exec.sock"),
            hc.clone(),
        )
    });

    super::volume::attach_volumes(&volume_names, &box_id)?;

    let stop_signal = args
        .common
        .stop_signal
        .as_deref()
        .map(parse_signal_name)
        .unwrap_or(libc::SIGTERM);
    let stop_timeout_ms = args
        .common
        .stop_timeout
        .map(|secs| secs * 1000)
        .unwrap_or(DEFAULT_SHUTDOWN_TIMEOUT_MS);

    Ok(RunContext {
        vm,
        box_id,
        box_dir,
        name,
        volume_names,
        health_checker,
        stop_signal,
        stop_timeout_ms,
    })
}

/// Build TeeConfig from run args.
fn build_tee_config(args: &RunArgs) -> TeeConfig {
    if args.tee || args.tee_simulate {
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
    }
}

/// Build BoxConfig from parsed run arguments.
#[allow(clippy::too_many_arguments)]
fn build_box_config(
    args: &RunArgs,
    memory_mb: u32,
    resource_limits: a3s_box_core::config::ResourceLimits,
    entrypoint_override: Option<Vec<String>>,
    resolved_volumes: Vec<String>,
    extra_env: Vec<(String, String)>,
    network: a3s_box_core::NetworkMode,
    tmpfs: Vec<String>,
    tee: TeeConfig,
) -> BoxConfig {
    BoxConfig {
        image: args.common.image.clone(),
        resources: ResourceConfig {
            vcpus: args.common.cpus,
            memory_mb,
            ..Default::default()
        },
        cmd: args.cmd.clone(),
        entrypoint_override,
        volumes: resolved_volumes,
        extra_env,
        port_map: args.common.publish.clone(),
        dns: args.common.dns.clone(),
        network,
        tmpfs,
        resource_limits,
        tee,
        read_only: args.common.read_only,
        sidecar: args.sidecar.as_ref().map(|image| SidecarConfig {
            image: image.clone(),
            vsock_port: args.sidecar_vsock_port,
            env: vec![],
        }),
        persistent: args.common.persistent,
        ..Default::default()
    }
}

/// Register a network endpoint for the box before booting.
fn connect_network(
    net_name: Option<&str>,
    box_id: &str,
    name: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let Some(net_name) = net_name else {
        return Ok(());
    };
    let net_store = a3s_box_runtime::NetworkStore::default_path()?;
    let mut net_config = net_store
        .get(net_name)?
        .ok_or_else(|| format!("network '{}' not found", net_name))?;
    let endpoint = net_config
        .connect(box_id, name)
        .map_err(|e| format!("Failed to connect to network: {e}"))?;
    net_store.update(&net_config)?;
    println!(
        "Connected to network {} (IP: {})",
        net_name, endpoint.ip_address
    );
    Ok(())
}

// ============================================================================
// Phase 2a: Interactive PTY mode
// ============================================================================

async fn run_tty(mut ctx: RunContext, args: &RunArgs) -> Result<(), Box<dyn std::error::Error>> {
    use a3s_box_core::pty::PtyRequest;
    use a3s_box_runtime::PtyClient;
    use crossterm::terminal;

    let pty_socket_path = ctx.box_dir.join("sockets").join("pty.sock");

    // Wait for PTY socket to appear
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

    let entrypoint_override = args
        .common
        .entrypoint
        .as_ref()
        .map(|ep| ep.split_whitespace().map(String::from).collect::<Vec<_>>());

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

    // Cleanup
    cleanup_box(&mut ctx, args.common.network.as_deref(), args.rm).await?;

    if exit_code != 0 {
        std::process::exit(exit_code);
    }
    Ok(())
}

// ============================================================================
// Phase 2b: Foreground mode (tail logs, wait for exit or Ctrl-C)
// ============================================================================

async fn run_foreground(
    mut ctx: RunContext,
    args: &RunArgs,
) -> Result<(), Box<dyn std::error::Error>> {
    println!(
        "Box {} ({}) started. Press Ctrl-C to stop.",
        ctx.name,
        BoxRecord::make_short_id(&ctx.box_id)
    );

    let console_log = ctx.box_dir.join("logs").join("console.log");
    let log_handle = tokio::spawn(async move {
        super::tail_file(&console_log).await;
    });

    let name = ctx.name.clone();
    let user_interrupted = tokio::select! {
        _ = tokio::signal::ctrl_c() => {
            println!("\nStopping box {}...", name);
            true
        }
        _ = async {
            loop {
                tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                if !ctx.vm.health_check().await.unwrap_or(false) {
                    break;
                }
            }
        } => false,
    };

    log_handle.abort();

    // Destroy VM
    if let Some(ref handle) = ctx.health_checker {
        handle.abort();
    }
    ctx.vm
        .destroy_with_options(ctx.stop_signal, ctx.stop_timeout_ms)
        .await?;
    let exit_code = ctx.vm.exit_code();

    // Detach volumes and disconnect network
    super::volume::detach_volumes(&ctx.volume_names, &ctx.box_id);
    disconnect_network(&ctx.box_id, args.common.network.as_deref())?;

    // Update state
    let mut state = StateFile::load_default()?;
    if let Some(rec) = state.find_by_id_mut(&ctx.box_id) {
        rec.status = "stopped".to_string();
        rec.pid = None;
    }

    if args.rm {
        state.remove(&ctx.box_id)?;
        let _ = std::fs::remove_dir_all(&ctx.box_dir);
        if !user_interrupted {
            println!("Box {} exited and was removed.", ctx.name);
        } else {
            println!("Box {} removed.", ctx.name);
        }
    } else {
        state.save()?;
        if !user_interrupted {
            println!("Box {} exited.", ctx.name);
        } else {
            println!("Box {} stopped.", ctx.name);
        }
    }

    if let Some(code) = exit_code {
        if code != 0 {
            std::process::exit(code);
        }
    }

    Ok(())
}

// ============================================================================
// Shared helpers
// ============================================================================

/// Parse health check config from common args.
fn parse_health_check(common: &CommonBoxArgs) -> Option<crate::state::HealthCheck> {
    if common.no_healthcheck {
        return None;
    }
    common
        .health_cmd
        .as_ref()
        .map(|cmd| crate::state::HealthCheck {
            cmd: vec!["sh".to_string(), "-c".to_string(), cmd.clone()],
            interval_secs: common.health_interval,
            timeout_secs: common.health_timeout,
            retries: common.health_retries,
            start_period_secs: common.health_start_period,
        })
}

/// Resolve named volumes, returning (resolved_specs, volume_names).
fn resolve_volumes(
    volume_specs: &[String],
) -> Result<(Vec<String>, Vec<String>), Box<dyn std::error::Error>> {
    let mut resolved = Vec::new();
    let mut names = Vec::new();
    for spec in volume_specs {
        let (r, vol_name) = super::volume::resolve_named_volume(spec)?;
        if let Some(name) = vol_name {
            names.push(name);
        }
        resolved.push(r);
    }
    Ok((resolved, names))
}

/// Disconnect from network if connected.
fn disconnect_network(
    box_id: &str,
    net_name: Option<&str>,
) -> Result<(), Box<dyn std::error::Error>> {
    if let Some(net_name) = net_name {
        let net_store = a3s_box_runtime::NetworkStore::default_path()?;
        if let Some(mut net_config) = net_store.get(net_name)? {
            net_config.disconnect(box_id).ok();
            net_store.update(&net_config)?;
        }
    }
    Ok(())
}

/// Shared cleanup: abort health checker, destroy VM, detach volumes, disconnect network, update state.
async fn cleanup_box(
    ctx: &mut RunContext,
    net_name: Option<&str>,
    auto_remove: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    if let Some(ref handle) = ctx.health_checker {
        handle.abort();
    }
    ctx.vm
        .destroy_with_options(ctx.stop_signal, ctx.stop_timeout_ms)
        .await?;
    super::volume::detach_volumes(&ctx.volume_names, &ctx.box_id);
    disconnect_network(&ctx.box_id, net_name)?;

    let mut state = StateFile::load_default()?;
    if let Some(rec) = state.find_by_id_mut(&ctx.box_id) {
        rec.status = "stopped".to_string();
        rec.pid = None;
    }
    if auto_remove {
        state.remove(&ctx.box_id)?;
        let _ = std::fs::remove_dir_all(&ctx.box_dir);
    } else {
        state.save()?;
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
                persistent: false,
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
            sidecar: None,
            sidecar_vsock_port: 4092,
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

    #[test]
    fn test_parse_health_check_none() {
        let args = default_run_args();
        assert!(parse_health_check(&args.common).is_none());
    }

    #[test]
    fn test_parse_health_check_disabled() {
        let mut args = default_run_args();
        args.common.health_cmd = Some("curl localhost".to_string());
        args.common.no_healthcheck = true;
        assert!(parse_health_check(&args.common).is_none());
    }

    #[test]
    fn test_parse_health_check_configured() {
        let mut args = default_run_args();
        args.common.health_cmd = Some("curl localhost".to_string());
        args.common.health_interval = 10;
        args.common.health_retries = 5;
        let hc = parse_health_check(&args.common).unwrap();
        assert_eq!(hc.cmd, vec!["sh", "-c", "curl localhost"]);
        assert_eq!(hc.interval_secs, 10);
        assert_eq!(hc.retries, 5);
    }

    #[test]
    fn test_resolve_volumes_empty() {
        let (resolved, names) = resolve_volumes(&[]).unwrap();
        assert!(resolved.is_empty());
        assert!(names.is_empty());
    }
}
