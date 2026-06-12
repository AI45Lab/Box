//! `a3s-box pool` — Warm VM pool daemon + client.
//!
//! Pre-boots keepalive MicroVMs of one image so a command can run in an
//! already-ready sandbox instead of paying a full cold boot. `pool start` is the
//! daemon (pre-warms a pool and serves requests over a Unix socket); `pool run`
//! is the client (runs a command in a fresh warm sandbox via the guest exec
//! server, no cold boot). This is the low-risk keepalive+exec MVP from
//! docs/cow-snapshot-fork-design.md — it removes cold boot from the hot path
//! without touching guest-init's lifecycle.
//!
//! Subcommands:
//!   pool start --image IMAGE --size N [--socket P]   Daemon: pre-warm + serve
//!   pool run [--socket P] -- CMD...                  Client: run CMD in a sandbox
//!   pool stop / pool status                          Discoverability helpers

use clap::{Parser, Subcommand};
use serde::{Deserialize, Serialize};

use a3s_box_core::config::{BoxConfig, PoolConfig};
use a3s_box_core::event::EventEmitter;
use a3s_box_runtime::pool::{PoolStats, WarmPool};

/// Default Unix socket the `pool` daemon listens on.
const DEFAULT_SOCKET: &str = "/tmp/a3s-box-pool.sock";

/// Manage the warm VM pool.
#[derive(Parser)]
pub struct PoolArgs {
    #[command(subcommand)]
    pub action: PoolAction,
}

/// Pool subcommands.
#[derive(Subcommand)]
pub enum PoolAction {
    /// Start the warm pool daemon (pre-boot VMs + serve `pool run` over a socket)
    Start(PoolStartArgs),
    /// Run a command in a fresh warm sandbox (client of `pool start`)
    Run(PoolRunArgs),
    /// Drain and stop the warm pool
    Stop(PoolStopArgs),
    /// Show warm pool statistics
    Status(PoolStatusArgs),
}

/// Arguments for `pool start`.
#[derive(Parser)]
pub struct PoolStartArgs {
    /// Image to pre-warm (optional). Sandboxes default to this image; `pool run`
    /// may request any other image, which the daemon warms on first use.
    #[arg(long)]
    pub image: Option<String>,

    /// Number of VMs to keep pre-booted (min_idle)
    #[arg(long, default_value = "2")]
    pub size: usize,

    /// Maximum pool capacity
    #[arg(long, default_value = "8")]
    pub max: usize,

    /// Idle TTL in seconds before evicting a pre-booted VM (0 = unlimited)
    #[arg(long, default_value = "300")]
    pub ttl: u64,

    /// Unix socket to serve `pool run` requests on
    #[arg(long, default_value = DEFAULT_SOCKET)]
    pub socket: String,

    /// Extra images to pre-warm at startup, `image[=count]` (count defaults to
    /// --size). Repeat or comma-separate: `--warm python:3=4,node:20`.
    #[arg(long, value_delimiter = ',')]
    pub warm: Vec<String>,

    /// Boot pooled VMs IDLE and run each `pool run` command as the box's real MAIN
    /// (full box semantics: exit code + json-file console logs), instead of
    /// exec-into-keepalive.
    #[arg(long)]
    pub deferred: bool,

    /// Mark pooled VM memory KSM-mergeable so the host dedups identical pages
    /// across same-image VMs (Linux 6.4+; needs /sys/kernel/mm/ksm/run=1).
    #[arg(long)]
    pub ksm: bool,

    /// Output as JSON
    #[arg(long)]
    pub json: bool,
}

/// Arguments for `pool run`.
#[derive(Parser)]
pub struct PoolRunArgs {
    /// Unix socket of the `pool start` daemon
    #[arg(long, default_value = DEFAULT_SOCKET)]
    pub socket: String,

    /// Image to run in (defaults to the daemon's --image). The daemon warms a
    /// pool for this image on first use.
    #[arg(long)]
    pub image: Option<String>,

    /// User to run as (uid[:gid] or a name resolved in the container).
    #[arg(long, short = 'u')]
    pub user: Option<String>,

    /// Working directory inside the sandbox.
    #[arg(long, short = 'w')]
    pub workdir: Option<String>,

    /// Extra environment variables, KEY=VALUE (repeatable).
    #[arg(long, short = 'e')]
    pub env: Vec<String>,

    /// On a --deferred daemon: run via exec instead of as the box's main —
    /// faster (the VM survives and is returned to use), output via the exec
    /// stream rather than the json-file logs.
    #[arg(long)]
    pub exec: bool,

    /// Command and arguments to run in a fresh warm sandbox
    #[arg(last = true, required = true)]
    pub cmd: Vec<String>,
}

/// Arguments for `pool stop`.
#[derive(Parser)]
pub struct PoolStopArgs {
    /// Output as JSON
    #[arg(long)]
    pub json: bool,
}

/// Arguments for `pool status`.
#[derive(Parser)]
pub struct PoolStatusArgs {
    /// Unix socket of the `pool start` daemon
    #[arg(long, default_value = DEFAULT_SOCKET)]
    pub socket: String,

    /// Output as JSON
    #[arg(long)]
    pub json: bool,
}

/// Wire protocol for the `pool` Unix socket (length-prefixed JSON).
///
/// Client→daemon request: run a command, or query status. Tagged so the daemon
/// can dispatch; the client parses the response type matching what it sent.
#[derive(Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
enum Request {
    Run(RunRequest),
    Status,
}

#[derive(Serialize, Deserialize)]
struct RunRequest {
    /// Image to run in; `None` means use the daemon's default image.
    #[serde(default)]
    image: Option<String>,
    /// User to run as (uid[:gid] or name); `None` runs as the image default.
    #[serde(default)]
    user: Option<String>,
    /// Working directory inside the sandbox.
    #[serde(default)]
    workdir: Option<String>,
    /// Extra KEY=VALUE environment entries.
    #[serde(default)]
    env: Vec<String>,
    /// Force exec mode for this request (valid on a --deferred daemon, whose
    /// IDLE VMs still serve exec; a keepalive daemon is always exec).
    #[serde(default)]
    exec: bool,
    cmd: Vec<String>,
}

#[derive(Serialize, Deserialize)]
struct RunResponse {
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    exit_code: i32,
    error: Option<String>,
}

/// Live stats for one image's warm pool.
#[derive(Serialize, Deserialize)]
struct ImageStat {
    image: String,
    idle: usize,
    total_created: u64,
    total_acquired: u64,
    total_evicted: u64,
}

#[derive(Serialize, Deserialize)]
struct StatusResponse {
    images: Vec<ImageStat>,
}

/// Execute a pool command.
pub async fn execute(args: PoolArgs) -> Result<(), Box<dyn std::error::Error>> {
    match args.action {
        PoolAction::Start(a) => execute_start(a).await,
        PoolAction::Run(a) => execute_run(a).await,
        PoolAction::Stop(a) => execute_stop(a).await,
        PoolAction::Status(a) => execute_status(a).await,
    }
}

/// Keepalive main process so a pooled VM stays up with its exec server available;
/// the real `pool run` command runs via exec, not as this main.
fn keepalive_cmd() -> Vec<String> {
    vec![
        "/bin/sh".to_string(),
        "-c".to_string(),
        "trap 'exit 0' TERM INT; while :; do sleep 3600; done".to_string(),
    ]
}

/// Build the `spawn-main` JSON spec for a deferred-mode pool command (executable +
/// args + a standard PATH so the binary resolves like a normal container main,
/// plus optional user/workdir and extra env from the request).
fn deferred_spec_json(req: &RunRequest) -> Vec<u8> {
    let mut env: Vec<(String, String)> = vec![(
        "PATH".to_string(),
        "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin".to_string(),
    )];
    for entry in &req.env {
        if let Some((k, v)) = entry.split_once('=') {
            env.push((k.to_string(), v.to_string()));
        }
    }
    let spec = serde_json::json!({
        "executable": req.cmd.first().map(String::as_str).unwrap_or("/bin/sh"),
        "args": req.cmd.get(1..).unwrap_or(&[]),
        "env": env,
        "workdir": req.workdir,
        "user": req.user,
    });
    serde_json::to_vec(&spec).unwrap_or_default()
}

/// Parse a `--warm` entry of the form `image[=count]` (count defaults to `default_size`).
fn parse_warm_spec(entry: &str, default_size: usize) -> Result<(String, usize), String> {
    match entry.split_once('=') {
        Some((image, count)) => {
            let image = image.trim();
            if image.is_empty() {
                return Err(format!("missing image in '{entry}'"));
            }
            let count: usize = count
                .trim()
                .parse()
                .map_err(|_| format!("invalid warm count in '{entry}'"))?;
            Ok((image.to_string(), count))
        }
        None => Ok((entry.trim().to_string(), default_size)),
    }
}

/// One image's warm pool plus a semaphore bounding concurrent in-flight sandboxes.
/// `WarmPool::acquire` boots on a pool miss with no `max_size` cap, so without this
/// a burst of `pool run`s would boot unbounded VMs; the permit makes excess
/// requests queue instead.
#[derive(Clone)]
struct PoolEntry {
    pool: std::sync::Arc<WarmPool>,
    sem: std::sync::Arc<tokio::sync::Semaphore>,
}

/// A registry of warm pools keyed by image, created lazily on first use, so one
/// daemon can serve sandboxes of different images.
struct PoolRegistry {
    pools: tokio::sync::Mutex<std::collections::HashMap<String, PoolEntry>>,
    default_image: Option<String>,
    size: usize,
    max: usize,
    ttl: u64,
    /// When true, pooled VMs boot IDLE and `pool run` spawns the command as the
    /// box's real MAIN (full box semantics), instead of exec-into-keepalive.
    deferred: bool,
    /// Mark pooled VM memory KSM-mergeable (host page dedup across same-image VMs).
    ksm: bool,
}

impl PoolRegistry {
    /// The pool entry for `image`, lazily started (and pre-warmed in the background)
    /// on first use, with `min_idle = size`. `WarmPool::start` returns once the
    /// replenisher is spawned, so holding the map lock across it is brief. The
    /// concurrency semaphore is sized to the pool's `max_size`.
    async fn get_or_create_with_size(&self, image: &str, size: usize) -> Result<PoolEntry, String> {
        let mut pools = self.pools.lock().await;
        if let Some(entry) = pools.get(image) {
            return Ok(entry.clone());
        }
        let max_size = self.max.max(size);
        let pool_config = PoolConfig {
            enabled: true,
            min_idle: size,
            max_size,
            idle_ttl_secs: self.ttl,
            ..Default::default()
        };
        let box_config = BoxConfig {
            image: image.to_string(),
            // In deferred mode the VM boots IDLE (keepalive cmd is stashed but
            // unused — the per-request command arrives via spawn-main).
            cmd: keepalive_cmd(),
            pool: pool_config.clone(),
            deferred_main: self.deferred,
            ksm: self.ksm,
            ..Default::default()
        };
        let pool = std::sync::Arc::new(
            WarmPool::start(pool_config, box_config, EventEmitter::new(256))
                .await
                .map_err(|e| e.to_string())?,
        );
        let entry = PoolEntry {
            pool,
            sem: std::sync::Arc::new(tokio::sync::Semaphore::new(max_size)),
        };
        pools.insert(image.to_string(), entry.clone());
        Ok(entry)
    }

    /// Lazy pool for `image` at the daemon's default size.
    async fn get_or_create(&self, image: &str) -> Result<PoolEntry, String> {
        self.get_or_create_with_size(image, self.size).await
    }

    /// Resolve the image for a request: the requested one, else the daemon default.
    fn resolve_image(&self, requested: Option<String>) -> Option<String> {
        requested.or_else(|| self.default_image.clone())
    }

    /// Stop replenishment and destroy idle VMs across all pools (shutdown).
    async fn drain_all(&self) {
        let pools = self.pools.lock().await;
        for entry in pools.values() {
            entry.pool.signal_shutdown();
            let _ = entry.pool.drain_idle().await;
        }
    }

    /// Snapshot live per-image stats, sorted by image name.
    async fn stats(&self) -> Vec<ImageStat> {
        let pools = self.pools.lock().await;
        let mut out = Vec::with_capacity(pools.len());
        for (image, entry) in pools.iter() {
            let s = entry.pool.stats().await;
            out.push(ImageStat {
                image: image.clone(),
                idle: s.idle_count,
                total_created: s.total_created,
                total_acquired: s.total_acquired,
                total_evicted: s.total_evicted,
            });
        }
        out.sort_by(|a, b| a.image.cmp(&b.image));
        out
    }
}

async fn execute_start(args: PoolStartArgs) -> Result<(), Box<dyn std::error::Error>> {
    if args.size == 0 {
        return Err("--size must be greater than 0".into());
    }
    if args.size > args.max {
        return Err(format!("--size ({}) cannot exceed --max ({})", args.size, args.max).into());
    }

    let registry = std::sync::Arc::new(PoolRegistry {
        pools: tokio::sync::Mutex::new(std::collections::HashMap::new()),
        default_image: args.image.clone(),
        size: args.size,
        max: args.max,
        ttl: args.ttl,
        deferred: args.deferred,
        ksm: args.ksm,
    });

    // Pre-warm the default image, if one was given.
    let default_stats = if let Some(ref image) = args.image {
        let entry = registry.get_or_create(image).await?;
        Some((image.clone(), entry.pool.stats().await))
    } else {
        None
    };

    // Pre-warm any extra images requested via --warm.
    let mut warmed_extra: Vec<(String, usize)> = Vec::new();
    for entry in &args.warm {
        let (image, count) = parse_warm_spec(entry, args.size)?;
        if count == 0 {
            return Err(format!("--warm count must be > 0 (in '{entry}')").into());
        }
        registry.get_or_create_with_size(&image, count).await?;
        warmed_extra.push((image, count));
    }

    if args.json {
        match &default_stats {
            Some((image, stats)) => println!("{}", format_stats_json(image, stats)),
            None => println!(
                r#"{{"default_image":null,"max":{},"socket":"{}"}}"#,
                args.max, args.socket
            ),
        }
    } else {
        println!("Warm pool started");
        match &args.image {
            Some(i) => println!("  default image: {i} (pre-warming {})", args.size),
            None => println!("  default image: (none — `pool run` must pass --image)"),
        }
        for (image, count) in &warmed_extra {
            println!("  pre-warmed: {image} (size {count})");
        }
        println!("  max:      {}", args.max);
        println!("  ttl:      {}s", args.ttl);
        println!("  socket:   {}", args.socket);
    }

    serve(registry, &args.socket, args.json).await?;

    if !args.json {
        println!("Done.");
    }
    Ok(())
}

/// Accept `pool run` connections until Ctrl-C, serving each request concurrently
/// so independent sandboxes don't queue behind one another. On shutdown, stop the
/// replenisher and destroy idle VMs (in-flight requests keep their own acquired VM).
#[cfg(not(windows))]
async fn serve(
    registry: std::sync::Arc<PoolRegistry>,
    socket: &str,
    json: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    use tokio::net::UnixListener;

    let _ = std::fs::remove_file(socket);
    let listener = UnixListener::bind(socket)?;
    if !json {
        println!("Listening on {} (Ctrl-C to drain and stop)", socket);
    }

    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let (mut stream, _) = accepted?;
                let registry = registry.clone();
                tokio::spawn(async move {
                    if let Err(e) = handle_conn(&registry, &mut stream).await {
                        tracing::warn!(error = %e, "pool connection failed");
                    }
                });
            }
            _ = tokio::signal::ctrl_c() => {
                let _ = std::fs::remove_file(socket);
                if !json {
                    println!("Draining warm pools...");
                }
                registry.drain_all().await;
                break;
            }
        }
    }
    Ok(())
}

#[cfg(not(windows))]
fn err_resp(msg: impl Into<String>) -> RunResponse {
    RunResponse {
        stdout: vec![],
        stderr: vec![],
        exit_code: -1,
        error: Some(msg.into()),
    }
}

#[cfg(not(windows))]
async fn handle_conn(
    registry: &PoolRegistry,
    stream: &mut tokio::net::UnixStream,
) -> std::io::Result<()> {
    // 60s exec cap — generous for a sandbox command.
    const EXEC_TIMEOUT_NS: u64 = 60_000_000_000;

    let req: Request = serde_json::from_slice(&read_frame(stream).await?)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;

    // `status` is a simple query — answer and return.
    let run = match req {
        Request::Status => {
            let resp = StatusResponse {
                images: registry.stats().await,
            };
            let bytes = serde_json::to_vec(&resp)
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
            return write_frame(stream, &bytes).await;
        }
        Request::Run(run) => run,
    };

    // Resolve the image, get-or-create its pool, acquire a warm VM, run the
    // command. Keep the VM so we tear it down AFTER responding (a one-shot sandbox
    // is discarded; the pool replenishes a fresh one) — the client's latency must
    // not include VM teardown.
    // Holds (vm, permit) until after the response: the permit bounds concurrent
    // in-flight sandboxes and is released only once the VM is torn down.
    let mut used = None;
    let resp = match registry.resolve_image(run.image.clone()) {
        None => err_resp("no image: pass --image or start the daemon with --image"),
        Some(image) => match registry.get_or_create(&image).await {
            Err(e) => err_resp(format!("pool for {image}: {e}")),
            Ok(entry) => {
                // Backpressure: wait for a slot so a burst doesn't boot unbounded VMs.
                let permit = entry
                    .sem
                    .clone()
                    .acquire_owned()
                    .await
                    .expect("pool semaphore is never closed");
                match entry.pool.acquire().await {
                    Err(e) => err_resp(format!("acquire failed: {e}")),
                    Ok(mut vm) => {
                        // Deferred-main: run the command as the box's real MAIN
                        // (full box semantics — exit code + json-file console logs).
                        // Otherwise exec it (output via the exec stream); `exec:
                        // true` forces exec mode per request on a deferred daemon
                        // (its IDLE VMs serve exec just as well). Both honor
                        // user/workdir/env from the request.
                        let result = if registry.deferred && !run.exec {
                            vm.run_deferred_main(
                                &deferred_spec_json(&run),
                                std::time::Duration::from_secs(60),
                            )
                            .await
                        } else {
                            vm.exec_request(&a3s_box_core::exec::ExecRequest {
                                cmd: run.cmd,
                                timeout_ns: EXEC_TIMEOUT_NS,
                                env: run.env,
                                working_dir: run.workdir,
                                rootfs: None,
                                stdin: None,
                                stdin_streaming: false,
                                user: run.user,
                                streaming: false,
                            })
                            .await
                        };
                        let resp = match result {
                            Ok(o) => RunResponse {
                                stdout: o.stdout,
                                stderr: o.stderr,
                                exit_code: o.exit_code,
                                error: None,
                            },
                            Err(e) => err_resp(e.to_string()),
                        };
                        used = Some((vm, permit));
                        resp
                    }
                }
            }
        },
    };

    let bytes = serde_json::to_vec(&resp)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    write_frame(stream, &bytes).await?;

    // Tear down the used sandbox in the background so neither the client nor the
    // daemon's accept loop blocks on it; release the concurrency permit afterwards.
    if let Some((mut vm, permit)) = used {
        tokio::spawn(async move {
            let _ = vm.destroy().await;
            drop(permit);
        });
    }
    Ok(())
}

#[cfg(not(windows))]
async fn execute_run(args: PoolRunArgs) -> Result<(), Box<dyn std::error::Error>> {
    use std::io::Write;
    use tokio::net::UnixStream;

    let mut stream = UnixStream::connect(&args.socket).await.map_err(|e| {
        format!(
            "Failed to connect to pool daemon at {} ({}). Is `a3s-box pool start` running?",
            args.socket, e
        )
    })?;

    write_frame(
        &mut stream,
        &serde_json::to_vec(&Request::Run(RunRequest {
            image: args.image,
            user: args.user,
            workdir: args.workdir,
            env: args.env,
            exec: args.exec,
            cmd: args.cmd,
        }))?,
    )
    .await?;
    let resp: RunResponse = serde_json::from_slice(&read_frame(&mut stream).await?)?;

    if let Some(err) = resp.error {
        eprintln!("pool error: {err}");
        std::process::exit(1);
    }
    std::io::stdout().write_all(&resp.stdout)?;
    std::io::stderr().write_all(&resp.stderr)?;
    std::process::exit(resp.exit_code);
}

#[cfg(windows)]
async fn serve(
    _registry: std::sync::Arc<PoolRegistry>,
    _socket: &str,
    _json: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    eprintln!(
        "pool socket serving is not supported on Windows; pool stays pre-warmed until Ctrl-C."
    );
    tokio::signal::ctrl_c().await?;
    Ok(())
}

#[cfg(windows)]
async fn execute_run(_args: PoolRunArgs) -> Result<(), Box<dyn std::error::Error>> {
    Err("`pool run` is not supported on Windows".into())
}

/// Length-prefixed (u32 LE) framing for the Unix-socket protocol.
#[cfg(not(windows))]
async fn write_frame<W: tokio::io::AsyncWriteExt + Unpin>(
    w: &mut W,
    data: &[u8],
) -> std::io::Result<()> {
    w.write_all(&(data.len() as u32).to_le_bytes()).await?;
    w.write_all(data).await?;
    w.flush().await
}

#[cfg(not(windows))]
async fn read_frame<R: tokio::io::AsyncReadExt + Unpin>(r: &mut R) -> std::io::Result<Vec<u8>> {
    let mut len = [0u8; 4];
    r.read_exact(&mut len).await?;
    let mut buf = vec![0u8; u32::from_le_bytes(len) as usize];
    r.read_exact(&mut buf).await?;
    Ok(buf)
}

async fn execute_stop(_args: PoolStopArgs) -> Result<(), Box<dyn std::error::Error>> {
    // Pool stop is handled by sending SIGINT to the `pool start` process.
    eprintln!("Send SIGINT (Ctrl-C) to the running `a3s-box pool start` process to drain and stop the pool.");
    Ok(())
}

#[cfg(not(windows))]
async fn execute_status(args: PoolStatusArgs) -> Result<(), Box<dyn std::error::Error>> {
    use tokio::net::UnixStream;

    let mut stream = UnixStream::connect(&args.socket).await.map_err(|e| {
        format!(
            "Failed to connect to pool daemon at {} ({}). Is `a3s-box pool start` running?",
            args.socket, e
        )
    })?;

    write_frame(&mut stream, &serde_json::to_vec(&Request::Status)?).await?;
    let resp: StatusResponse = serde_json::from_slice(&read_frame(&mut stream).await?)?;

    if args.json {
        println!("{}", serde_json::to_string(&resp.images)?);
    } else if resp.images.is_empty() {
        println!("No warm pools yet (no images warmed).");
    } else {
        println!(
            "{:<40} {:>5} {:>8} {:>9} {:>8}",
            "IMAGE", "IDLE", "CREATED", "ACQUIRED", "EVICTED"
        );
        for s in &resp.images {
            println!(
                "{:<40} {:>5} {:>8} {:>9} {:>8}",
                s.image, s.idle, s.total_created, s.total_acquired, s.total_evicted
            );
        }
    }
    Ok(())
}

#[cfg(windows)]
async fn execute_status(_args: PoolStatusArgs) -> Result<(), Box<dyn std::error::Error>> {
    Err("`pool status` is not supported on Windows".into())
}

/// Format pool stats as a JSON string.
fn format_stats_json(image: &str, stats: &PoolStats) -> String {
    let hit_rate = if stats.total_acquired > 0 {
        stats.total_acquired.saturating_sub(stats.total_evicted) as f64
            / stats.total_acquired as f64
    } else {
        0.0
    };
    format!(
        r#"{{"image":"{image}","idle":{idle},"total_created":{created},"total_acquired":{acquired},"total_released":{released},"total_evicted":{evicted},"hit_rate":{hit_rate:.2}}}"#,
        image = image,
        idle = stats.idle_count,
        created = stats.total_created,
        acquired = stats.total_acquired,
        released = stats.total_released,
        evicted = stats.total_evicted,
        hit_rate = hit_rate,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use a3s_box_runtime::pool::PoolStats;

    fn sample_stats() -> PoolStats {
        PoolStats {
            idle_count: 2,
            total_created: 5,
            total_acquired: 4,
            total_released: 3,
            total_evicted: 1,
        }
    }

    #[test]
    fn test_format_stats_json_fields() {
        let stats = sample_stats();
        let json = format_stats_json("alpine:latest", &stats);
        assert!(json.contains(r#""image":"alpine:latest""#));
        assert!(json.contains(r#""idle":2"#));
        assert!(json.contains(r#""total_created":5"#));
        assert!(json.contains(r#""total_acquired":4"#));
        assert!(json.contains(r#""total_released":3"#));
        assert!(json.contains(r#""total_evicted":1"#));
        assert!(json.contains("hit_rate"));
    }

    #[test]
    fn test_format_stats_json_zero_acquired() {
        let stats = PoolStats {
            idle_count: 0,
            total_created: 0,
            total_acquired: 0,
            total_released: 0,
            total_evicted: 0,
        };
        let json = format_stats_json("nginx:alpine", &stats);
        assert!(json.contains(r#""hit_rate":0.00"#));
    }

    #[test]
    fn test_format_stats_json_is_valid_structure() {
        let stats = sample_stats();
        let json = format_stats_json("alpine:latest", &stats);
        assert!(json.starts_with('{'));
        assert!(json.ends_with('}'));
    }

    #[test]
    fn test_keepalive_cmd_is_a_sleep_loop() {
        let c = keepalive_cmd();
        assert_eq!(c[0], "/bin/sh");
        assert!(c.last().unwrap().contains("sleep"));
    }

    #[test]
    fn test_parse_warm_spec() {
        // image=count
        assert_eq!(
            parse_warm_spec("python:3=4", 2).unwrap(),
            ("python:3".to_string(), 4)
        );
        // bare image → default size
        assert_eq!(
            parse_warm_spec("node:20", 7).unwrap(),
            ("node:20".to_string(), 7)
        );
        // whitespace tolerated
        assert_eq!(
            parse_warm_spec("  alpine = 3 ", 2).unwrap(),
            ("alpine".to_string(), 3)
        );
        // bad count / empty image error out
        assert!(parse_warm_spec("alpine=notanum", 2).is_err());
        assert!(parse_warm_spec("=4", 2).is_err());
    }

    #[test]
    fn test_deferred_spec_json() {
        // The spawn-main spec for a deferred pool run: executable + args + a PATH
        // so the binary resolves like a normal container main, plus per-request
        // user/workdir and extra env.
        let req = RunRequest {
            image: None,
            user: Some("1000".into()),
            workdir: Some("/work".into()),
            env: vec!["FOO=bar".into(), "not-a-pair".into()],
            exec: false,
            cmd: vec!["sh".into(), "-c".into(), "echo hi".into()],
        };
        let json = deferred_spec_json(&req);
        let v: serde_json::Value = serde_json::from_slice(&json).unwrap();
        assert_eq!(v["executable"], "sh");
        assert_eq!(v["args"][0], "-c");
        assert_eq!(v["args"][1], "echo hi");
        assert_eq!(v["env"][0][0], "PATH");
        assert!(v["env"][0][1].as_str().unwrap().contains("/bin"));
        assert_eq!(v["env"][1][0], "FOO");
        assert_eq!(v["env"][1][1], "bar");
        assert_eq!(v["env"].as_array().unwrap().len(), 2); // malformed entry dropped
        assert_eq!(v["user"], "1000");
        assert_eq!(v["workdir"], "/work");
        // Empty cmd falls back to a shell rather than panicking.
        let req2 = RunRequest {
            image: None,
            user: None,
            workdir: None,
            env: vec![],
            exec: false,
            cmd: vec![],
        };
        let v2: serde_json::Value = serde_json::from_slice(&deferred_spec_json(&req2)).unwrap();
        assert_eq!(v2["executable"], "/bin/sh");
        assert!(v2["user"].is_null());
    }

    #[tokio::test]
    async fn test_backpressure_bounds_concurrency() {
        // The contract PoolEntry relies on: a permit (held until teardown) caps
        // concurrent in-flight sandboxes to the semaphore size, so a burst queues
        // instead of all running at once.
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;

        let sem = Arc::new(tokio::sync::Semaphore::new(2));
        let live = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));

        let mut handles = Vec::new();
        for _ in 0..6 {
            let (sem, live, peak) = (sem.clone(), live.clone(), peak.clone());
            handles.push(tokio::spawn(async move {
                let _permit = sem.acquire_owned().await.unwrap();
                let now = live.fetch_add(1, Ordering::SeqCst) + 1;
                peak.fetch_max(now, Ordering::SeqCst);
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                live.fetch_sub(1, Ordering::SeqCst);
            }));
        }
        for h in handles {
            h.await.unwrap();
        }
        assert!(
            peak.load(Ordering::SeqCst) <= 2,
            "concurrency exceeded the permit limit"
        );
    }

    #[test]
    fn test_run_request_response_roundtrip() {
        let req = RunRequest {
            image: Some("alpine:latest".into()),
            user: Some("1000".into()),
            workdir: Some("/tmp".into()),
            env: vec!["FOO=bar".into()],
            exec: false,
            cmd: vec!["echo".into(), "hi".into()],
        };
        let bytes = serde_json::to_vec(&req).unwrap();
        let parsed: RunRequest = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(parsed.cmd, vec!["echo", "hi"]);
        assert_eq!(parsed.image.as_deref(), Some("alpine:latest"));
        assert_eq!(parsed.user.as_deref(), Some("1000"));
        assert_eq!(parsed.workdir.as_deref(), Some("/tmp"));
        assert_eq!(parsed.env, vec!["FOO=bar"]);

        // image/user/workdir/env are optional on the wire (older clients).
        let no_img: RunRequest = serde_json::from_slice(br#"{"cmd":["ls"]}"#).unwrap();
        assert!(no_img.image.is_none());
        assert!(no_img.user.is_none() && no_img.workdir.is_none() && no_img.env.is_empty());

        let resp = RunResponse {
            stdout: b"hi\n".to_vec(),
            stderr: vec![],
            exit_code: 0,
            error: None,
        };
        let rb = serde_json::to_vec(&resp).unwrap();
        let rp: RunResponse = serde_json::from_slice(&rb).unwrap();
        assert_eq!(rp.stdout, b"hi\n");
        assert_eq!(rp.exit_code, 0);
        assert!(rp.error.is_none());
    }

    #[tokio::test]
    async fn test_execute_start_size_zero_fails() {
        let args = PoolStartArgs {
            image: Some("alpine:latest".to_string()),
            size: 0,
            max: 5,
            ttl: 300,
            socket: DEFAULT_SOCKET.to_string(),
            warm: vec![],
            deferred: false,
            ksm: false,
            json: false,
        };
        let result = execute_start(args).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("greater than 0"));
    }

    #[tokio::test]
    async fn test_execute_start_size_exceeds_max_fails() {
        let args = PoolStartArgs {
            image: Some("alpine:latest".to_string()),
            size: 10,
            max: 5,
            ttl: 300,
            socket: DEFAULT_SOCKET.to_string(),
            warm: vec![],
            deferred: false,
            ksm: false,
            json: false,
        };
        let result = execute_start(args).await;
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("cannot exceed --max"));
    }

    #[tokio::test]
    async fn test_execute_stop_is_ok() {
        let result = execute_stop(PoolStopArgs { json: false }).await;
        assert!(result.is_ok());
    }

    #[cfg(not(windows))]
    #[tokio::test]
    async fn test_execute_status_no_daemon_errors() {
        // With no daemon listening, status fails with a connect hint (not a panic).
        let result = execute_status(PoolStatusArgs {
            socket: "/tmp/a3s-box-pool-does-not-exist.sock".to_string(),
            json: false,
        })
        .await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("pool daemon"));
    }

    #[test]
    fn test_request_envelope_tagging() {
        // Run carries an op tag + the flattened RunRequest; Status is a bare tag.
        let run = serde_json::to_string(&Request::Run(RunRequest {
            image: Some("alpine".into()),
            user: None,
            workdir: None,
            env: vec![],
            exec: false,
            cmd: vec!["echo".into(), "hi".into()],
        }))
        .unwrap();
        assert!(run.contains(r#""op":"run""#));
        assert!(run.contains(r#""cmd":["echo","hi"]"#));

        let status = serde_json::to_string(&Request::Status).unwrap();
        assert_eq!(status, r#"{"op":"status"}"#);

        // StatusResponse round-trips.
        let sr = StatusResponse {
            images: vec![ImageStat {
                image: "alpine".into(),
                idle: 2,
                total_created: 5,
                total_acquired: 3,
                total_evicted: 1,
            }],
        };
        let parsed: StatusResponse =
            serde_json::from_slice(&serde_json::to_vec(&sr).unwrap()).unwrap();
        assert_eq!(parsed.images[0].image, "alpine");
        assert_eq!(parsed.images[0].idle, 2);
    }

    #[cfg(not(windows))]
    #[tokio::test]
    async fn test_frame_roundtrip() {
        // write_frame then read_frame must return the exact bytes.
        let (mut a, mut b) = tokio::io::duplex(4096);
        let payload = serde_json::to_vec(&RunRequest {
            image: None,
            user: None,
            workdir: None,
            env: vec![],
            exec: false,
            cmd: vec!["echo".into(), "hi there".into()],
        })
        .unwrap();
        write_frame(&mut a, &payload).await.unwrap();
        let got = read_frame(&mut b).await.unwrap();
        let parsed: RunRequest = serde_json::from_slice(&got).unwrap();
        assert_eq!(parsed.cmd, vec!["echo", "hi there"]);
    }

    #[cfg(not(windows))]
    #[tokio::test]
    async fn test_socket_request_response_protocol() {
        // Exercise the full client/server wire protocol over a real Unix socket
        // (the exact framing `serve` and `pool run` use), with a stub server
        // standing in for the VM pool's acquire+exec.
        use tokio::net::{UnixListener, UnixStream};

        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("pool.sock");
        let listener = UnixListener::bind(&sock).unwrap();

        let server = tokio::spawn(async move {
            let (mut s, _) = listener.accept().await.unwrap();
            let req: RunRequest =
                serde_json::from_slice(&read_frame(&mut s).await.unwrap()).unwrap();
            let resp = RunResponse {
                stdout: format!("ran {:?}", req.cmd).into_bytes(),
                stderr: vec![],
                exit_code: 0,
                error: None,
            };
            write_frame(&mut s, &serde_json::to_vec(&resp).unwrap())
                .await
                .unwrap();
        });

        let mut client = UnixStream::connect(&sock).await.unwrap();
        let req = RunRequest {
            image: Some("alpine:latest".into()),
            user: None,
            workdir: None,
            env: vec![],
            exec: false,
            cmd: vec!["ls".into(), "-la".into()],
        };
        write_frame(&mut client, &serde_json::to_vec(&req).unwrap())
            .await
            .unwrap();
        let resp: RunResponse =
            serde_json::from_slice(&read_frame(&mut client).await.unwrap()).unwrap();

        assert_eq!(resp.exit_code, 0);
        assert!(resp.error.is_none());
        assert!(String::from_utf8_lossy(&resp.stdout).contains("ls"));
        server.await.unwrap();
    }

    #[cfg(not(windows))]
    #[tokio::test]
    async fn test_read_frame_truncated_errors() {
        // A truncated stream must error, not hang or panic.
        use tokio::io::AsyncWriteExt;
        let (mut a, mut b) = tokio::io::duplex(64);
        a.write_all(&[1u8, 0]).await.unwrap(); // partial 4-byte length prefix
        drop(a);
        assert!(read_frame(&mut b).await.is_err());
    }
}
