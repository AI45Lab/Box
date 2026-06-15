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
    exec_socket_path: PathBuf,
    #[cfg_attr(windows, allow(dead_code))]
    pty_socket_path: PathBuf,
    volume_names: Vec<String>,
    anonymous_volumes: Vec<String>,
    health_checker: Option<tokio::task::JoinHandle<()>>,
    stop_signal: i32,
    stop_timeout_ms: u64,
}

pub async fn execute(args: RunArgs) -> Result<(), Box<dyn std::error::Error>> {
    validate_run_mode(&args, std::io::stdin().is_terminal())
        .map_err(|e| -> Box<dyn std::error::Error> { e.into() })?;

    let ctx = setup_and_boot(&args).await?;
    if args.detach {
        println!("{}", ctx.box_id);
        return Ok(());
    }

    if args.tty {
        return run_tty(ctx, &args).await;
    }

    run_foreground(ctx, &args).await
}

fn validate_run_mode(args: &RunArgs, stdin_is_terminal: bool) -> Result<(), &'static str> {
    if args.detach && args.tty {
        return Err("Cannot use -t (tty) with -d (detach)");
    }
    if args.tty && !stdin_is_terminal {
        return Err("The -t flag requires a terminal (stdin is not a TTY)");
    }
    Ok(())
}

// ============================================================================
// Phase 1: Parse args, build config, boot VM, save state
// ============================================================================

async fn setup_and_boot(args: &RunArgs) -> Result<RunContext, Box<dyn std::error::Error>> {
    common::validate_runtime_options(&args.common)
        .map_err(|e| -> Box<dyn std::error::Error> { e.into() })?;
    let (restart_policy, max_restart_count) =
        crate::state::parse_restart_policy(&args.common.restart)
            .map_err(|e| -> Box<dyn std::error::Error> { e.into() })?;

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
    let env = common::build_env_map(&args.common)?;
    let port_map = common::normalize_port_maps(&args.common.publish)
        .map_err(|e| -> Box<dyn std::error::Error> { e.into() })?;
    let labels = common::parse_env_vars(&args.common.labels)
        .map_err(|e| e.replace("environment variable", "label"))?;
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

    // Default (TSI) networking proxies guest sockets to the host, so a container
    // cannot reach its own services over the guest loopback. A health check that
    // probes localhost would always fail — point the user at bridge networking.
    if matches!(network_mode, a3s_box_core::NetworkMode::Tsi) {
        if let Some(cmd) = &args.common.health_cmd {
            let lc = cmd.to_lowercase();
            if lc.contains("localhost") || lc.contains("127.0.0.1") {
                eprintln!(
                    "warning: the health check probes localhost, but default (TSI) networking \
                     cannot reach a container's own services over loopback, so the check will fail. \
                     For a working localhost, create and attach a bridge network: \
                     `a3s-box network create mynet` then run with `--network mynet`."
                );
            }
        }
    }

    let tee = build_tee_config(args);

    let config = build_box_config(
        args,
        memory_mb,
        resource_limits.clone(),
        entrypoint_override.clone(),
        resolved_volumes.clone(),
        env.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
        port_map.clone(),
        network_mode.clone(),
        tmpfs,
        tee,
    )
    .map_err(|e| -> Box<dyn std::error::Error> { e.into() })?;

    let emitter = EventEmitter::new(256);
    let mut vm = VmManager::new(config, emitter);
    // The shim runs the log processor for the box's lifetime (so detached boxes
    // keep logging after this CLI exits).
    vm.set_log_config(log_config.clone());
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
            let actual_size = -size;
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
    if let Err(error) = vm.boot().await {
        crate::cleanup::cleanup_box_resources(
            &box_id,
            &volume_names,
            args.common.network.as_deref(),
        );
        return Err(error.into());
    }

    let image_health_check = vm
        .image_config()
        .and_then(|config| config.health_check.clone());
    let image_stop_signal = vm
        .image_config()
        .and_then(|config| config.stop_signal.clone());
    let health_check = common::effective_health_check(&args.common, image_health_check.as_ref());
    let effective_stop_signal = common::effective_stop_signal(
        args.common.stop_signal.as_deref(),
        image_stop_signal.as_deref(),
    );

    let pid = vm.pid().await;
    let box_dir = a3s_box_core::dirs_home().join("boxes").join(&box_id);
    let exec_socket_path = vm
        .exec_socket_path()
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| box_dir.join("sockets").join("exec.sock"));
    let pty_socket_path = vm
        .pty_socket_path()
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| box_dir.join("sockets").join("pty.sock"));

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
        exec_socket_path: exec_socket_path.clone(),
        console_log: box_dir.join("logs").join("console.log"),
        created_at: chrono::Utc::now(),
        started_at: Some(chrono::Utc::now()),
        auto_remove: args.rm,
        hostname: args.common.hostname.clone(),
        user: args.common.user.clone(),
        workdir: args.common.workdir.clone(),
        restart_policy,
        port_map: port_map.clone(),
        labels,
        stopped_by_user: false,
        restart_count: 0,
        health_check: health_check.clone(),
        healthcheck_disabled: args.common.no_healthcheck,
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
        stop_signal: effective_stop_signal.clone(),
        stop_timeout: args.common.stop_timeout,
        oom_kill_disable: args.common.oom_kill_disable,
        oom_score_adj: args.common.oom_score_adj,
        max_restart_count,
        exit_code: None,
    };

    let stop_signal = effective_stop_signal
        .as_deref()
        .map(parse_signal_name)
        .unwrap_or(15); // SIGTERM = 15
    let stop_timeout_ms = args
        .common
        .stop_timeout
        .map(|secs| secs * 1000)
        .unwrap_or(DEFAULT_SHUTDOWN_TIMEOUT_MS);
    let anonymous_volumes = vm.anonymous_volumes().to_vec();
    // Register atomically (load-fresh-under-lock → push → write). A stale
    // `load_default()` + `state.add()` (save the in-memory snapshot) is a
    // lost-update race: concurrent fork registrations clobber each other's records,
    // so a burst of N forks would leave only a fraction registered.
    if let Err(error) = StateFile::add_record(record.clone()) {
        rollback_booted_setup(&mut vm, &record, stop_signal, stop_timeout_ms, None).await;
        return Err(error.into());
    }

    if let Err(error) = super::diff::create_box_baseline_snapshot(&box_dir) {
        tracing::warn!(
            box_id = %box_id,
            error = %error,
            "Failed to create rootfs diff baseline snapshot"
        );
    }

    if let Err(error) = super::volume::attach_volumes(&volume_names, &box_id) {
        // The record was registered atomically above; un-register it the same way.
        let _ = StateFile::remove_record(&record.id);
        rollback_booted_setup(&mut vm, &record, stop_signal, stop_timeout_ms, None).await;
        return Err(error);
    }

    let log_dir = box_dir.join("logs");
    if let Err(error) = std::fs::create_dir_all(&log_dir) {
        let _ = StateFile::remove_record(&record.id);
        rollback_booted_setup(&mut vm, &record, stop_signal, stop_timeout_ms, None).await;
        return Err(error.into());
    }
    // Log processing now runs in the shim for the box's lifetime; see
    // VmManager::set_log_config above. (log_dir is still created so the shim's
    // container.json has a home.)
    let _ = &log_dir;

    let health_checker = health_check.as_ref().map(|hc| {
        crate::health::spawn_health_checker(box_id.clone(), exec_socket_path.clone(), hc.clone())
    });

    Ok(RunContext {
        vm,
        box_id,
        box_dir,
        name,
        exec_socket_path,
        pty_socket_path,
        volume_names,
        anonymous_volumes,
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
    port_map: Vec<String>,
    network: a3s_box_core::NetworkMode,
    tmpfs: Vec<String>,
    tee: TeeConfig,
) -> Result<BoxConfig, String> {
    let (cmd, entrypoint_override) = if args.tty {
        (
            vec!["a3s-box-pty-keepalive".to_string()],
            Some(interactive_keepalive_entrypoint()),
        )
    } else {
        (args.cmd.clone(), entrypoint_override)
    };

    Ok(BoxConfig {
        image: args.common.image.clone(),
        resources: ResourceConfig {
            vcpus: args.common.cpus,
            memory_mb,
            ..Default::default()
        },
        cmd,
        entrypoint_override,
        user: common::normalize_user_option(args.common.user.as_deref())?,
        workdir: args.common.workdir.clone(),
        hostname: args.common.hostname.clone(),
        volumes: resolved_volumes,
        extra_env,
        port_map,
        dns: args.common.dns.clone(),
        add_hosts: args.common.add_host.clone(),
        network,
        tmpfs,
        resource_limits,
        tee,
        read_only: args.common.read_only,
        cap_add: args.common.cap_add.clone(),
        cap_drop: args.common.cap_drop.clone(),
        security_opt: args.common.security_opt.clone(),
        privileged: args.common.privileged,
        sidecar: args.sidecar.as_ref().map(|image| SidecarConfig {
            image: image.clone(),
            vsock_port: args.sidecar_vsock_port,
            env: vec![],
        }),
        // A box without `--rm` survives its stop like a Docker stopped
        // container: keep its dir (logs + overlay upper) so `logs`/`start` work
        // afterwards. `--rm` boxes and CRI pods stay non-persistent (removed on
        // teardown). `rm` force-removes either way (cleanup_removed_box).
        persistent: args.common.persistent || !args.rm,
        ..Default::default()
    })
}

/// Initial process used only to keep the guest init alive for `run -it`.
///
/// The actual user command is executed over the PTY after guest control sockets
/// are ready, so short-lived interactive commands do not race the VM shutdown.
fn interactive_keepalive_entrypoint() -> Vec<String> {
    vec![
        "/bin/sh".to_string(),
        "-c".to_string(),
        "trap 'exit 0' TERM INT; while :; do sleep 3600; done".to_string(),
    ]
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
    let endpoint = connect_box_to_network(&net_store, net_name, box_id, name)?;
    println!(
        "Connected to network {} (IP: {})",
        net_name, endpoint.ip_address
    );
    Ok(())
}

/// Allocate a network endpoint for a box atomically under the store's
/// cross-process lock.
///
/// A plain `get → connect → update` reads the network outside the lock, so two
/// concurrent `run --network` could allocate the same IP and drop one endpoint
/// (#70 fixed the `network`/`start` paths but not this one). Split out from
/// [`connect_network`] so the lost-update behavior is unit-testable with an
/// explicit store — `NetworkStore::default_path` reads the process-global
/// `A3S_HOME`, which cannot be raced safely across tests.
fn connect_box_to_network(
    net_store: &a3s_box_runtime::NetworkStore,
    net_name: &str,
    box_id: &str,
    name: &str,
) -> Result<a3s_box_core::network::NetworkEndpoint, Box<dyn std::error::Error>> {
    net_store.with_write_lock(
        |networks| -> Result<a3s_box_core::network::NetworkEndpoint, Box<dyn std::error::Error>> {
            let net_config =
                networks
                    .get_mut(net_name)
                    .ok_or_else(|| -> Box<dyn std::error::Error> {
                        format!("network '{}' not found", net_name).into()
                    })?;
            super::network::validate_attachable_network(net_config)
                .map_err(|e| -> Box<dyn std::error::Error> { e.into() })?;
            net_config
                .connect(box_id, name)
                .map_err(|e| -> Box<dyn std::error::Error> {
                    format!("Failed to connect to network: {e}").into()
                })
        },
    )
}

// ============================================================================
// Phase 2a: Interactive PTY mode
// ============================================================================

#[cfg(not(windows))]
async fn run_tty(mut ctx: RunContext, args: &RunArgs) -> Result<(), Box<dyn std::error::Error>> {
    use crate::terminal;
    use a3s_box_core::pty::PtyRequest;

    let pty_socket_path = ctx.pty_socket_path.clone();

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
    let user = common::normalize_user_option(args.common.user.as_deref())
        .map_err(|e| -> Box<dyn std::error::Error> { e.into() })?;
    let env = common::build_env_map(&args.common)?
        .into_iter()
        .map(|(key, value)| format!("{key}={value}"))
        .collect();
    let mut client =
        super::exec::connect_pty_with_retry(&pty_socket_path, std::time::Duration::from_secs(10))
            .await?;
    client
        .send_request(&PtyRequest {
            cmd: pty_cmd,
            env,
            working_dir: args.common.workdir.clone(),
            rootfs: None,
            user,
            cols,
            rows,
        })
        .await?;

    let (read_half, write_half) = client.into_split();
    let exit_code = {
        let _raw_mode = terminal::raw_mode()?;
        super::exec::run_pty_session(read_half, write_half).await
    };

    // Cleanup
    cleanup_box(
        &mut ctx,
        args.common.network.as_deref(),
        args.rm,
        Some(exit_code),
    )
    .await?;

    if exit_code != 0 {
        std::process::exit(exit_code);
    }
    Ok(())
}

#[cfg(windows)]
async fn run_tty(_ctx: RunContext, _args: &RunArgs) -> Result<(), Box<dyn std::error::Error>> {
    Err(crate::platform::unsupported_command(
        "run -it",
        "interactive PTY support",
    ))
}

// ============================================================================
// Phase 2b: Foreground mode (tail logs, wait for exit or Ctrl-C)
// ============================================================================

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ForegroundStopReason {
    ProcessExited,
    UserInterrupted,
    VmUnhealthy,
}

impl ForegroundStopReason {
    fn stopped_by_user(self) -> bool {
        matches!(self, Self::UserInterrupted)
    }
}

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
    let console_err = ctx.box_dir.join("logs").join("console.err.log");
    let log_handle = tokio::spawn(async move {
        // Stream the container's stdout (console.log) and stderr
        // (console.err.log, split console) to the terminal's stdout/stderr.
        tokio::join!(
            super::tail_file(&console_log),
            super::tail_file_stream(&console_err, true),
        );
    });

    let name = ctx.name.clone();
    let stop_reason = loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {
                println!("\nStopping box {}...", name);
                break ForegroundStopReason::UserInterrupted;
            }
            _ = tokio::time::sleep(std::time::Duration::from_millis(500)) => {
                if ctx.vm.try_wait_exit().await?.is_some() {
                    break ForegroundStopReason::ProcessExited;
                }
                if !ctx.vm.health_check().await.unwrap_or(false) {
                    break ForegroundStopReason::VmUnhealthy;
                }
            }
        }
    };

    log_handle.abort();

    // Destroy VM
    if let Some(ref handle) = ctx.health_checker {
        handle.abort();
    }
    ctx.vm
        .destroy_with_options(ctx.stop_signal, ctx.stop_timeout_ms)
        .await?;
    let exit_code = foreground_exit_code(stop_reason, ctx.vm.exit_code());

    // Detach volumes and disconnect network
    super::volume::detach_volumes(&ctx.volume_names, &ctx.box_id);
    disconnect_network(&ctx.box_id, args.common.network.as_deref())?;
    crate::cleanup::cleanup_external_socket_dir(&ctx.box_dir, &ctx.exec_socket_path);

    // Update state through the atomic primitives so a concurrent writer (the
    // monitor, or another box's stop) is not clobbered by a stale load → mutate
    // → full-vector save. Box registration already uses add_record for the same
    // reason; this symmetric stop path had been missed.
    let stopped_by_user = stop_reason.stopped_by_user();
    if args.rm {
        crate::cleanup::cleanup_anonymous_volumes(&ctx.anonymous_volumes);
        StateFile::remove_record(&ctx.box_id)?;
        let _ = std::fs::remove_dir_all(&ctx.box_dir);
        println!(
            "{}",
            foreground_completion_message(stop_reason, true, &ctx.name)
        );
    } else {
        StateFile::modify(|s| {
            mark_record_stopped(s, &ctx.box_id, exit_code, stopped_by_user);
            Ok::<(), std::io::Error>(())
        })?;
        println!(
            "{}",
            foreground_completion_message(stop_reason, false, &ctx.name)
        );
    }

    if let Some(code) = exit_code {
        if code != 0 {
            std::process::exit(code);
        }
    }

    Ok(())
}

fn foreground_exit_code(reason: ForegroundStopReason, vm_exit_code: Option<i32>) -> Option<i32> {
    vm_exit_code.or(match reason {
        ForegroundStopReason::ProcessExited => None,
        ForegroundStopReason::UserInterrupted => Some(130),
        ForegroundStopReason::VmUnhealthy => Some(1),
    })
}

fn foreground_completion_message(
    reason: ForegroundStopReason,
    auto_remove: bool,
    name: &str,
) -> String {
    match (reason, auto_remove) {
        (ForegroundStopReason::ProcessExited, true) => {
            format!("Box {name} exited and was removed.")
        }
        (ForegroundStopReason::ProcessExited, false) => format!("Box {name} exited."),
        (ForegroundStopReason::UserInterrupted, true) => format!("Box {name} removed."),
        (ForegroundStopReason::UserInterrupted, false) => format!("Box {name} stopped."),
        (ForegroundStopReason::VmUnhealthy, true) => {
            format!("Box {name} stopped after VM health check failed and was removed.")
        }
        (ForegroundStopReason::VmUnhealthy, false) => {
            format!("Box {name} stopped after VM health check failed.")
        }
    }
}

// ============================================================================
// Shared helpers
// ============================================================================

async fn rollback_booted_setup(
    vm: &mut VmManager,
    record: &BoxRecord,
    stop_signal: i32,
    stop_timeout_ms: u64,
    state: Option<&mut StateFile>,
) {
    if let Err(error) = vm.destroy_with_options(stop_signal, stop_timeout_ms).await {
        tracing::debug!(
            box_id = %record.id,
            error = %error,
            "Failed to destroy VM while rolling back run setup"
        );
    }

    crate::cleanup::cleanup_partial_box_record(record, state);
}

/// Parse health check config from common args.
#[cfg(test)]
fn parse_health_check(common: &common::CommonBoxArgs) -> Option<crate::state::HealthCheck> {
    common::effective_health_check(common, None)
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
        // Release the endpoint under the lock with a fresh read, so a
        // concurrent connect to the same network is not clobbered.
        net_store.with_write_lock(|networks| -> Result<(), Box<dyn std::error::Error>> {
            if let Some(net_config) = networks.get_mut(net_name) {
                net_config.disconnect(box_id).ok();
            }
            Ok(())
        })?;
    }
    Ok(())
}

/// Shared cleanup: abort health checker, destroy VM, detach volumes, disconnect network, update state.
#[cfg(not(windows))]
async fn cleanup_box(
    ctx: &mut RunContext,
    net_name: Option<&str>,
    auto_remove: bool,
    exit_code: Option<i32>,
) -> Result<(), Box<dyn std::error::Error>> {
    if let Some(ref handle) = ctx.health_checker {
        handle.abort();
    }
    ctx.vm
        .destroy_with_options(ctx.stop_signal, ctx.stop_timeout_ms)
        .await?;
    super::volume::detach_volumes(&ctx.volume_names, &ctx.box_id);
    disconnect_network(&ctx.box_id, net_name)?;
    crate::cleanup::cleanup_external_socket_dir(&ctx.box_dir, &ctx.exec_socket_path);

    // Atomic write under the state lock so a concurrent monitor/CLI write to
    // another box is not clobbered by a stale full-vector save.
    if auto_remove {
        crate::cleanup::cleanup_anonymous_volumes(&ctx.anonymous_volumes);
        StateFile::remove_record(&ctx.box_id)?;
        let _ = std::fs::remove_dir_all(&ctx.box_dir);
    } else {
        StateFile::modify(|s| {
            mark_record_stopped(s, &ctx.box_id, exit_code, false);
            Ok::<(), std::io::Error>(())
        })?;
    }
    Ok(())
}

fn mark_record_stopped(
    state: &mut StateFile,
    box_id: &str,
    exit_code: Option<i32>,
    stopped_by_user: bool,
) {
    if let Some(rec) = state.find_by_id_mut(box_id) {
        rec.status = "stopped".to_string();
        rec.pid = None;
        rec.exit_code = exit_code;
        rec.stopped_by_user = stopped_by_user;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Regression for the `run --network` lost-update race: connect_box_to_network
    // must allocate a distinct IP per concurrent caller. A get → connect → update
    // (the pre-fix code) would dup IPs and drop endpoints. The advisory lock is
    // per-open-file-description, so separate FileLock::acquire calls serialize
    // even across threads in one process — which exercises the fix in-process.
    #[test]
    fn concurrent_connect_box_to_network_allocates_distinct_ips() {
        use a3s_box_core::network::NetworkConfig;
        use a3s_box_runtime::NetworkStore;
        use std::collections::HashSet;
        use std::sync::Arc;

        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(NetworkStore::new(dir.path().join("networks.json")));
        store
            .create(NetworkConfig::new("dev", "10.88.0.0/24").unwrap())
            .unwrap();

        let handles: Vec<_> = (0..16)
            .map(|i| {
                let store = Arc::clone(&store);
                std::thread::spawn(move || {
                    connect_box_to_network(&store, "dev", &format!("box-{i}"), &format!("name-{i}"))
                        .unwrap()
                        .ip_address
                })
            })
            .collect();
        let ips: HashSet<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        assert_eq!(
            ips.len(),
            16,
            "every concurrent connect must get a distinct IP (no lost update)"
        );
    }

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
    fn test_validate_run_mode_rejects_detached_tty_before_boot() {
        let mut args = default_run_args();
        args.detach = true;
        args.tty = true;

        let err = validate_run_mode(&args, true).unwrap_err();
        assert!(err.contains("Cannot use -t"));
    }

    #[test]
    fn test_validate_run_mode_rejects_tty_without_terminal_before_boot() {
        let mut args = default_run_args();
        args.tty = true;

        let err = validate_run_mode(&args, false).unwrap_err();
        assert!(err.contains("requires a terminal"));
    }

    #[test]
    fn test_validate_run_mode_allows_detached_without_tty() {
        let mut args = default_run_args();
        args.detach = true;

        assert!(validate_run_mode(&args, false).is_ok());
    }

    #[test]
    fn test_build_box_config_uses_keepalive_for_interactive_tty_boot() {
        let mut args = default_run_args();
        args.tty = true;
        args.cmd = vec!["/bin/echo".to_string(), "hello".to_string()];

        let config = build_box_config(
            &args,
            512,
            Default::default(),
            None,
            vec![],
            vec![],
            vec![],
            a3s_box_core::NetworkMode::Tsi,
            vec![],
            TeeConfig::None,
        )
        .unwrap();

        assert_eq!(config.cmd, vec!["a3s-box-pty-keepalive"]);
        assert_eq!(
            config.entrypoint_override,
            Some(interactive_keepalive_entrypoint())
        );
    }

    #[test]
    fn test_build_box_config_preserves_non_tty_command() {
        let mut args = default_run_args();
        args.cmd = vec!["/bin/echo".to_string(), "hello".to_string()];
        let entrypoint = Some(vec!["/custom-entrypoint".to_string()]);

        let config = build_box_config(
            &args,
            512,
            Default::default(),
            entrypoint.clone(),
            vec![],
            vec![],
            vec![],
            a3s_box_core::NetworkMode::Tsi,
            vec![],
            TeeConfig::None,
        )
        .unwrap();

        assert_eq!(config.cmd, args.cmd);
        assert_eq!(config.entrypoint_override, entrypoint);
    }

    #[test]
    fn test_mark_record_stopped_persists_exit_context() {
        let record = crate::test_helpers::fixtures::make_record(
            "550e8400-e29b-41d4-a716-446655440000",
            "run-exit",
            "running",
            Some(1234),
        );
        let (_tmp, mut state) = crate::test_helpers::fixtures::setup_state(vec![record]);

        mark_record_stopped(
            &mut state,
            "550e8400-e29b-41d4-a716-446655440000",
            Some(42),
            true,
        );

        let record = state
            .find_by_id("550e8400-e29b-41d4-a716-446655440000")
            .unwrap();
        assert_eq!(record.status, "stopped");
        assert_eq!(record.pid, None);
        assert_eq!(record.exit_code, Some(42));
        assert!(record.stopped_by_user);
    }

    #[test]
    fn test_foreground_exit_code_preserves_vm_code() {
        assert_eq!(
            foreground_exit_code(ForegroundStopReason::UserInterrupted, Some(143)),
            Some(143)
        );
        assert_eq!(
            foreground_exit_code(ForegroundStopReason::VmUnhealthy, Some(2)),
            Some(2)
        );
    }

    #[test]
    fn test_foreground_exit_code_has_deterministic_fallbacks() {
        assert_eq!(
            foreground_exit_code(ForegroundStopReason::ProcessExited, None),
            None
        );
        assert_eq!(
            foreground_exit_code(ForegroundStopReason::UserInterrupted, None),
            Some(130)
        );
        assert_eq!(
            foreground_exit_code(ForegroundStopReason::VmUnhealthy, None),
            Some(1)
        );
    }

    #[test]
    fn test_foreground_stop_reason_user_flag() {
        assert!(ForegroundStopReason::UserInterrupted.stopped_by_user());
        assert!(!ForegroundStopReason::ProcessExited.stopped_by_user());
        assert!(!ForegroundStopReason::VmUnhealthy.stopped_by_user());
    }

    #[test]
    fn test_foreground_completion_messages() {
        assert_eq!(
            foreground_completion_message(ForegroundStopReason::ProcessExited, true, "box"),
            "Box box exited and was removed."
        );
        assert_eq!(
            foreground_completion_message(ForegroundStopReason::UserInterrupted, false, "box"),
            "Box box stopped."
        );
        assert_eq!(
            foreground_completion_message(ForegroundStopReason::VmUnhealthy, true, "box"),
            "Box box stopped after VM health check failed and was removed."
        );
    }

    #[test]
    fn test_build_box_config_passes_security_options() {
        let mut args = default_run_args();
        args.common.cap_add = vec!["NET_ADMIN".to_string()];
        args.common.cap_drop = vec!["NET_RAW".to_string()];
        args.common.security_opt = vec!["seccomp=unconfined".to_string()];
        args.common.privileged = true;

        let config = build_box_config(
            &args,
            512,
            a3s_box_core::config::ResourceLimits::default(),
            None,
            vec![],
            vec![],
            vec![],
            a3s_box_core::NetworkMode::Tsi,
            vec![],
            TeeConfig::None,
        )
        .unwrap();

        assert_eq!(config.cap_add, vec!["NET_ADMIN"]);
        assert_eq!(config.cap_drop, vec!["NET_RAW"]);
        assert_eq!(config.security_opt, vec!["seccomp=unconfined"]);
        assert!(config.privileged);
    }

    #[test]
    fn test_build_box_config_passes_user_and_workdir() {
        let mut args = default_run_args();
        args.common.user = Some("root:root".to_string());
        args.common.workdir = Some("/app".to_string());

        let config = build_box_config(
            &args,
            512,
            a3s_box_core::config::ResourceLimits::default(),
            None,
            vec![],
            vec![],
            vec![],
            a3s_box_core::NetworkMode::Tsi,
            vec![],
            TeeConfig::None,
        )
        .unwrap();

        assert_eq!(config.user.as_deref(), Some("0:0"));
        assert_eq!(config.workdir.as_deref(), Some("/app"));
    }

    #[test]
    fn test_build_box_config_passes_hostname_and_add_hosts() {
        let mut args = default_run_args();
        args.common.hostname = Some("web".to_string());
        args.common.add_host = vec!["db.local:10.88.0.10".to_string()];

        let config = build_box_config(
            &args,
            512,
            a3s_box_core::config::ResourceLimits::default(),
            None,
            vec![],
            vec![],
            vec![],
            a3s_box_core::NetworkMode::Tsi,
            vec![],
            TeeConfig::None,
        )
        .unwrap();

        assert_eq!(config.hostname.as_deref(), Some("web"));
        assert_eq!(config.add_hosts, vec!["db.local:10.88.0.10"]);
    }

    #[test]
    fn test_resolve_volumes_empty() {
        let (resolved, names) = resolve_volumes(&[]).unwrap();
        assert!(resolved.is_empty());
        assert!(names.is_empty());
    }
}
