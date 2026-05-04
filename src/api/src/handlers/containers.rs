//! Container API handlers.

use axum::{Json, extract::{Path, Query}, http::StatusCode};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::HashMap;

use crate::error::{ApiResult, ApiError};
use crate::models::ContainerCreateRequest;

/// Query parameters for container list.
#[derive(Debug, Deserialize)]
pub struct ListQuery {
    /// Show all containers (default shows just running)
    #[serde(default)]
    all: bool,

    /// Show n last created containers (includes all states)
    limit: Option<usize>,

    /// Show only containers with given size
    size: Option<bool>,

    /// Filter containers by status, name, etc.
    filters: Option<String>,
}

/// GET /containers/json - List containers.
pub async fn list(Query(query): Query<ListQuery>) -> ApiResult<Json<serde_json::Value>> {
    let state = a3s_box_cli::state::StateFile::load_default()
        .map_err(|e| ApiError::Internal(e.to_string()))?;

    let mut containers: Vec<_> = state.list(query.all)
        .iter()
        .map(|record| {
            json!({
                "Id": record.id,
                "Names": [format!("/{}", record.name)],
                "Image": record.image,
                "ImageID": "",
                "Command": record.cmd.join(" "),
                "Created": record.created_at.timestamp(),
                "State": record.status,
                "Status": format!("{}", record.status),
                "Ports": [],
                "Labels": record.labels,
                "SizeRw": 0,
                "SizeRootFs": 0,
                "HostConfig": {
                    "NetworkMode": record.network_mode
                },
                "NetworkSettings": {
                    "Networks": {}
                },
                "Mounts": []
            })
        })
        .collect();

    // Apply limit if specified
    if let Some(limit) = query.limit {
        containers.truncate(limit);
    }

    Ok(Json(json!(containers)))
}

/// POST /containers/create - Create a container.
pub async fn create(
    Query(query): Query<HashMap<String, String>>,
    Json(req): Json<ContainerCreateRequest>,
) -> ApiResult<Json<serde_json::Value>> {
    // Extract container name from query parameter
    let name = query.get("name").map(|s| s.as_str());

    // Build a3s-box run command arguments
    let run_args = a3s_box_cli::commands::run::RunArgs {
        common: a3s_box_cli::commands::common::CommonBoxArgs {
            image: req.image.clone(),
            name: name.map(|s| s.to_string()),
            env: req.env.unwrap_or_default(),
            volume: vec![], // TODO: Parse from host_config.binds
            publish: vec![], // TODO: Parse from host_config.port_bindings
            network: req.host_config.as_ref()
                .and_then(|hc| hc.network_mode.clone()),
            hostname: req.hostname,
            user: req.user,
            workdir: req.working_dir,
            entrypoint: req.entrypoint,
            label: req.labels.map(|labels| {
                labels.into_iter()
                    .map(|(k, v)| format!("{}={}", k, v))
                    .collect()
            }).unwrap_or_default(),
            restart: req.host_config.as_ref()
                .and_then(|hc| hc.restart_policy.as_ref())
                .map(|rp| rp.name.clone())
                .unwrap_or_else(|| "no".to_string()),
            memory: req.host_config.as_ref()
                .and_then(|hc| hc.memory)
                .map(|m| format!("{}m", m / 1024 / 1024)),
            privileged: req.host_config.as_ref()
                .and_then(|hc| hc.privileged)
                .unwrap_or(false),
            read_only: req.host_config.as_ref()
                .and_then(|hc| hc.readonly_rootfs)
                .unwrap_or(false),
            ..Default::default()
        },
        detach: true,
        rm: req.host_config.as_ref()
            .and_then(|hc| hc.auto_remove)
            .unwrap_or(false),
        interactive: false,
        tty: false,
        cmd: req.cmd.unwrap_or_default(),
        log_driver: "json-file".to_string(),
        log_opts: vec![],
        tee: false,
        tee_workload_id: None,
        tee_simulate: false,
        sidecar: None,
        sidecar_vsock_port: 4092,
    };

    // Execute create (run in detached mode creates without starting)
    let _result = a3s_box_cli::commands::run::execute(run_args).await
        .map_err(|e| ApiError::Internal(e.to_string()))?;

    // Get the created container ID from state
    let state = a3s_box_cli::state::StateFile::load_default()
        .map_err(|e| ApiError::Internal(e.to_string()))?;

    let container = if let Some(name) = name {
        state.find_by_name(name)
    } else {
        state.list(true).last()
    };

    let box_id = container
        .map(|c| c.id.clone())
        .ok_or_else(|| ApiError::Internal("Failed to find created container".to_string()))?;

    Ok(Json(json!({
        "Id": box_id,
        "Warnings": []
    })))
}

/// GET /containers/:id/json - Inspect a container.
pub async fn inspect(Path(id): Path<String>) -> ApiResult<Json<serde_json::Value>> {
    let state = a3s_box_cli::state::StateFile::load_default()
        .map_err(|e| ApiError::Internal(e.to_string()))?;

    // Find container by ID or name
    let record = state.list(true)
        .into_iter()
        .find(|r| r.id.starts_with(&id) || r.name == id)
        .ok_or_else(|| ApiError::NotFound(format!("Container {} not found", id)))?;

    Ok(Json(json!({
        "Id": record.id,
        "Created": record.created_at.to_rfc3339(),
        "Path": record.cmd.first().unwrap_or(&String::new()),
        "Args": &record.cmd[1..],
        "State": {
            "Status": record.status,
            "Running": record.status == "running",
            "Paused": false,
            "Restarting": false,
            "OOMKilled": false,
            "Dead": record.status == "dead",
            "Pid": record.pid.unwrap_or(0),
            "ExitCode": record.exit_code.unwrap_or(0),
            "Error": "",
            "StartedAt": record.started_at.map(|t| t.to_rfc3339()).unwrap_or_default(),
            "FinishedAt": ""
        },
        "Image": record.image,
        "Name": format!("/{}", record.name),
        "RestartCount": record.restart_count,
        "HostConfig": {
            "NetworkMode": record.network_mode,
            "RestartPolicy": {
                "Name": record.restart_policy,
                "MaximumRetryCount": record.max_restart_count
            },
            "AutoRemove": record.auto_remove,
            "Privileged": record.privileged,
            "ReadonlyRootfs": record.read_only,
            "Memory": (record.memory_mb as i64) * 1024 * 1024,
        },
        "Config": {
            "Hostname": record.hostname.unwrap_or_else(|| record.name.clone()),
            "User": record.user.unwrap_or_default(),
            "Env": record.env.iter().map(|(k, v)| format!("{}={}", k, v)).collect::<Vec<_>>(),
            "Cmd": record.cmd,
            "Image": record.image,
            "WorkingDir": record.workdir.unwrap_or_else(|| "/".to_string()),
            "Entrypoint": record.entrypoint,
            "Labels": record.labels
        },
    })))
}

/// POST /containers/:id/start - Start a container.
pub async fn start(Path(id): Path<String>) -> ApiResult<StatusCode> {
    let mut state = a3s_box_cli::state::StateFile::load_default()
        .map_err(|e| ApiError::Internal(e.to_string()))?;

    // Find container
    let record = state.list(true)
        .into_iter()
        .find(|r| r.id.starts_with(&id) || r.name == id)
        .ok_or_else(|| ApiError::NotFound(format!("Container {} not found", id)))?;

    if record.status == "running" {
        return Ok(StatusCode::NOT_MODIFIED);
    }

    let box_id = record.id.clone();

    // Use start command
    let start_args = a3s_box_cli::commands::start::StartArgs {
        boxes: vec![box_id.clone()],
    };

    a3s_box_cli::commands::start::execute(start_args).await
        .map_err(|e| ApiError::Internal(e.to_string()))?;

    Ok(StatusCode::NO_CONTENT)
}

/// Query parameters for stop.
#[derive(Debug, Deserialize, Default)]
pub struct StopQuery {
    /// Seconds to wait before killing
    #[serde(rename = "t")]
    timeout: Option<u64>,
}

/// POST /containers/:id/stop - Stop a container.
pub async fn stop(
    Path(id): Path<String>,
    Query(query): Query<StopQuery>,
) -> ApiResult<StatusCode> {
    let state = a3s_box_cli::state::StateFile::load_default()
        .map_err(|e| ApiError::Internal(e.to_string()))?;

    // Find container
    let record = state.list(true)
        .into_iter()
        .find(|r| r.id.starts_with(&id) || r.name == id)
        .ok_or_else(|| ApiError::NotFound(format!("Container {} not found", id)))?;

    if record.status != "running" {
        return Ok(StatusCode::NOT_MODIFIED);
    }

    let box_id = record.id.clone();

    // Use stop command
    let stop_args = a3s_box_cli::commands::stop::StopArgs {
        boxes: vec![box_id],
        timeout: query.timeout,
    };

    a3s_box_cli::commands::stop::execute(stop_args).await
        .map_err(|e| ApiError::Internal(e.to_string()))?;

    Ok(StatusCode::NO_CONTENT)
}

/// POST /containers/:id/restart - Restart a container.
pub async fn restart(
    Path(id): Path<String>,
    Query(query): Query<StopQuery>,
) -> ApiResult<StatusCode> {
    let state = a3s_box_cli::state::StateFile::load_default()
        .map_err(|e| ApiError::Internal(e.to_string()))?;

    // Find container
    let record = state.list(true)
        .into_iter()
        .find(|r| r.id.starts_with(&id) || r.name == id)
        .ok_or_else(|| ApiError::NotFound(format!("Container {} not found", id)))?;

    let box_id = record.id.clone();

    // Use restart command
    let restart_args = a3s_box_cli::commands::restart::RestartArgs {
        boxes: vec![box_id],
        timeout: query.timeout.unwrap_or(10),
    };

    a3s_box_cli::commands::restart::execute(restart_args).await
        .map_err(|e| ApiError::Internal(e.to_string()))?;

    Ok(StatusCode::NO_CONTENT)
}

/// Query parameters for kill.
#[derive(Debug, Deserialize, Default)]
pub struct KillQuery {
    /// Signal to send
    signal: Option<String>,
}

/// POST /containers/:id/kill - Kill a container.
pub async fn kill(
    Path(id): Path<String>,
    Query(_query): Query<KillQuery>,
) -> ApiResult<StatusCode> {
    let state = a3s_box_cli::state::StateFile::load_default()
        .map_err(|e| ApiError::Internal(e.to_string()))?;

    // Find container
    let record = state.list(true)
        .into_iter()
        .find(|r| r.id.starts_with(&id) || r.name == id)
        .ok_or_else(|| ApiError::NotFound(format!("Container {} not found", id)))?;

    if record.status != "running" {
        return Err(ApiError::Conflict("Container is not running".to_string()));
    }

    let box_id = record.id.clone();

    // Use kill command
    let kill_args = a3s_box_cli::commands::kill::KillArgs {
        boxes: vec![box_id],
        signal: None, // TODO: Parse signal from query
    };

    a3s_box_cli::commands::kill::execute(kill_args).await
        .map_err(|e| ApiError::Internal(e.to_string()))?;

    Ok(StatusCode::NO_CONTENT)
}

/// Query parameters for remove.
#[derive(Debug, Deserialize, Default)]
pub struct RemoveQuery {
    /// Remove volumes
    #[serde(default)]
    v: bool,

    /// Force removal
    #[serde(default)]
    force: bool,

    /// Remove link
    link: Option<String>,
}

/// DELETE /containers/:id - Remove a container.
pub async fn remove(
    Path(id): Path<String>,
    Query(query): Query<RemoveQuery>,
) -> ApiResult<StatusCode> {
    let state = a3s_box_cli::state::StateFile::load_default()
        .map_err(|e| ApiError::Internal(e.to_string()))?;

    // Find container
    let record = state.list(true)
        .into_iter()
        .find(|r| r.id.starts_with(&id) || r.name == id)
        .ok_or_else(|| ApiError::NotFound(format!("Container {} not found", id)))?;

    if record.status == "running" && !query.force {
        return Err(ApiError::Conflict(
            "Cannot remove running container. Stop it first or use force=true".to_string()
        ));
    }

    let box_id = record.id.clone();

    // Use rm command
    let rm_args = a3s_box_cli::commands::rm::RmArgs {
        boxes: vec![box_id],
        force: query.force,
        volumes: query.v,
    };

    a3s_box_cli::commands::rm::execute(rm_args).await
        .map_err(|e| ApiError::Internal(e.to_string()))?;

    Ok(StatusCode::NO_CONTENT)
}

/// Query parameters for logs.
#[derive(Debug, Deserialize, Default)]
pub struct LogsQuery {
    /// Follow log output
    #[serde(default)]
    follow: bool,

    /// Show stdout
    #[serde(default = "default_true")]
    stdout: bool,

    /// Show stderr
    #[serde(default = "default_true")]
    stderr: bool,

    /// Show timestamps
    #[serde(default)]
    timestamps: bool,

    /// Number of lines from end
    tail: Option<String>,

    /// Show logs since timestamp
    since: Option<String>,

    /// Show logs until timestamp
    until: Option<String>,
}

fn default_true() -> bool {
    true
}

/// GET /containers/:id/logs - Get container logs.
pub async fn logs(
    Path(id): Path<String>,
    Query(query): Query<LogsQuery>,
) -> ApiResult<String> {
    let state = a3s_box_cli::state::StateFile::load_default()
        .map_err(|e| ApiError::Internal(e.to_string()))?;

    // Find container
    let record = state.list(true)
        .into_iter()
        .find(|r| r.id.starts_with(&id) || r.name == id)
        .ok_or_else(|| ApiError::NotFound(format!("Container {} not found", id)))?;

    // Check if logging is enabled
    if record.log_config.driver == a3s_box_core::log::LogDriver::None {
        return Err(ApiError::BadRequest(
            "Logging is disabled for this container".to_string()
        ));
    }

    // Get log file path
    let log_dir = record.box_dir.join("logs");
    let json_log = a3s_box_runtime::log::json_log_path(&log_dir);
    let log_path = if json_log.exists() {
        json_log
    } else {
        record.console_log.clone()
    };

    if !log_path.exists() {
        return Err(ApiError::NotFound("No logs found for container".to_string()));
    }

    // Read log file and apply filters
    let content = std::fs::read_to_string(&log_path)
        .map_err(|e| ApiError::Internal(format!("Failed to read logs: {}", e)))?;

    // Apply tail filtering
    let lines: Vec<&str> = content.lines().collect();
    let filtered = if let Some(tail_str) = &query.tail {
        if tail_str == "all" {
            lines.as_slice()
        } else if let Ok(n) = tail_str.parse::<usize>() {
            let start = lines.len().saturating_sub(n);
            &lines[start..]
        } else {
            lines.as_slice()
        }
    } else {
        lines.as_slice()
    };

    // Apply timestamp prefix if requested
    let output = if query.timestamps {
        filtered.iter()
            .map(|line| format!("{} {}", chrono::Utc::now().to_rfc3339(), line))
            .collect::<Vec<_>>()
            .join("\n")
    } else {
        filtered.join("\n")
    };

    Ok(output)
}

/// Query parameters for stats.
#[derive(Debug, Deserialize, Default)]
pub struct StatsQuery {
    /// Stream stats (default: true)
    #[serde(default = "default_true")]
    stream: bool,

    /// Only get a single stat
    #[serde(default)]
    one_shot: bool,
}

/// GET /containers/:id/stats - Get container stats.
pub async fn stats(
    Path(id): Path<String>,
    Query(query): Query<StatsQuery>,
) -> ApiResult<Json<serde_json::Value>> {
    let state = a3s_box_cli::state::StateFile::load_default()
        .map_err(|e| ApiError::Internal(e.to_string()))?;

    // Find container
    let record = state.list(true)
        .into_iter()
        .find(|r| r.id.starts_with(&id) || r.name == id)
        .ok_or_else(|| ApiError::NotFound(format!("Container {} not found", id)))?;

    if record.status != "running" {
        return Err(ApiError::Conflict("Container is not running".to_string()));
    }

    let pid = record.pid.ok_or_else(|| ApiError::Internal("No PID found".to_string()))?;

    // Collect stats using sysinfo
    use sysinfo::{Pid, System};
    let mut sys = System::new();
    let spid = Pid::from_u32(pid);

    // First refresh
    sys.refresh_process(spid);
    tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;
    // Second refresh for CPU delta
    sys.refresh_process(spid);

    let proc_info = sys.process(spid)
        .ok_or_else(|| ApiError::Internal("Process not found".to_string()))?;

    let cpu_percent = proc_info.cpu_usage();
    let memory_bytes = proc_info.memory();
    let memory_limit = (record.memory_mb as u64) * 1024 * 1024;

    // Build Docker-compatible stats response
    let stats = json!({
        "read": chrono::Utc::now().to_rfc3339(),
        "preread": chrono::Utc::now().to_rfc3339(),
        "pids_stats": {
            "current": 1
        },
        "blkio_stats": {},
        "num_procs": 0,
        "storage_stats": {},
        "cpu_stats": {
            "cpu_usage": {
                "total_usage": 0,
                "usage_in_kernelmode": 0,
                "usage_in_usermode": 0
            },
            "system_cpu_usage": 0,
            "online_cpus": num_cpus::get(),
            "throttling_data": {
                "periods": 0,
                "throttled_periods": 0,
                "throttled_time": 0
            }
        },
        "precpu_stats": {
            "cpu_usage": {
                "total_usage": 0,
                "usage_in_kernelmode": 0,
                "usage_in_usermode": 0
            },
            "system_cpu_usage": 0,
            "online_cpus": num_cpus::get(),
            "throttling_data": {
                "periods": 0,
                "throttled_periods": 0,
                "throttled_time": 0
            }
        },
        "memory_stats": {
            "usage": memory_bytes,
            "max_usage": memory_bytes,
            "stats": {},
            "limit": memory_limit
        },
        "name": format!("/{}", record.name),
        "id": record.id,
        "networks": {},
        "cpu_percent": cpu_percent,
        "memory_percent": if memory_limit > 0 {
            (memory_bytes as f64 / memory_limit as f64) * 100.0
        } else {
            0.0
        }
    });

    // TODO: Implement streaming mode
    Ok(Json(stats))
}

/// Query parameters for top.
#[derive(Debug, Deserialize, Default)]
pub struct TopQuery {
    /// ps arguments (e.g., "aux")
    ps_args: Option<String>,
}

/// GET /containers/:id/top - List processes.
pub async fn top(
    Path(id): Path<String>,
    Query(_query): Query<TopQuery>,
) -> ApiResult<Json<serde_json::Value>> {
    let state = a3s_box_cli::state::StateFile::load_default()
        .map_err(|e| ApiError::Internal(e.to_string()))?;

    // Find container
    let record = state.list(true)
        .into_iter()
        .find(|r| r.id.starts_with(&id) || r.name == id)
        .ok_or_else(|| ApiError::NotFound(format!("Container {} not found", id)))?;

    if record.status != "running" {
        return Err(ApiError::Conflict("Container is not running".to_string()));
    }

    let pid = record.pid.ok_or_else(|| ApiError::Internal("No PID found".to_string()))?;

    // Return basic process info
    Ok(Json(json!({
        "Titles": ["UID", "PID", "PPID", "C", "STIME", "TTY", "TIME", "CMD"],
        "Processes": [
            ["root", pid.to_string(), "0", "0", "00:00", "?", "00:00:00", "init"]
        ]
    })))
}

/// POST /containers/:id/pause - Pause a container.
pub async fn pause(Path(id): Path<String>) -> ApiResult<StatusCode> {
    let state = a3s_box_cli::state::StateFile::load_default()
        .map_err(|e| ApiError::Internal(e.to_string()))?;

    // Find container
    let record = state.list(true)
        .into_iter()
        .find(|r| r.id.starts_with(&id) || r.name == id)
        .ok_or_else(|| ApiError::NotFound(format!("Container {} not found", id)))?;

    if record.status != "running" {
        return Err(ApiError::Conflict("Container is not running".to_string()));
    }

    // TODO: Implement actual pause using cgroups freezer
    Ok(StatusCode::NO_CONTENT)
}

/// POST /containers/:id/unpause - Unpause a container.
pub async fn unpause(Path(id): Path<String>) -> ApiResult<StatusCode> {
    let state = a3s_box_cli::state::StateFile::load_default()
        .map_err(|e| ApiError::Internal(e.to_string()))?;

    // Find container
    let record = state.list(true)
        .into_iter()
        .find(|r| r.id.starts_with(&id) || r.name == id)
        .ok_or_else(|| ApiError::NotFound(format!("Container {} not found", id)))?;

    if record.status != "paused" {
        return Err(ApiError::Conflict("Container is not paused".to_string()));
    }

    // TODO: Implement actual unpause using cgroups freezer
    Ok(StatusCode::NO_CONTENT)
}

/// POST /containers/:id/wait - Wait for container to stop.
pub async fn wait(Path(id): Path<String>) -> ApiResult<Json<serde_json::Value>> {
    let state = a3s_box_cli::state::StateFile::load_default()
        .map_err(|e| ApiError::Internal(e.to_string()))?;

    // Find container
    let record = state.list(true)
        .into_iter()
        .find(|r| r.id.starts_with(&id) || r.name == id)
        .ok_or_else(|| ApiError::NotFound(format!("Container {} not found", id)))?;

    // If container is already stopped, return immediately
    if record.status != "running" {
        return Ok(Json(json!({
            "StatusCode": record.exit_code.unwrap_or(0)
        })));
    }

    // TODO: Implement actual wait by monitoring container state
    Ok(Json(json!({
        "StatusCode": 0
    })))
}

/// Request body for exec create.
#[derive(Debug, Deserialize)]
pub struct ExecCreateRequest {
    /// Attach to stdin
    #[serde(rename = "AttachStdin")]
    attach_stdin: Option<bool>,

    /// Attach to stdout
    #[serde(rename = "AttachStdout")]
    attach_stdout: Option<bool>,

    /// Attach to stderr
    #[serde(rename = "AttachStderr")]
    attach_stderr: Option<bool>,

    /// Allocate a pseudo-TTY
    #[serde(rename = "Tty")]
    tty: Option<bool>,

    /// Environment variables
    #[serde(rename = "Env")]
    env: Option<Vec<String>>,

    /// Command to run
    #[serde(rename = "Cmd")]
    cmd: Vec<String>,

    /// Working directory
    #[serde(rename = "WorkingDir")]
    working_dir: Option<String>,

    /// User (format: "user:group")
    #[serde(rename = "User")]
    user: Option<String>,

    /// Detach mode
    #[serde(rename = "Detach")]
    detach: Option<bool>,
}

/// POST /containers/:id/exec - Create exec instance.
pub async fn exec_create(
    Path(id): Path<String>,
    Json(req): Json<ExecCreateRequest>,
) -> ApiResult<Json<serde_json::Value>> {
    let state = a3s_box_cli::state::StateFile::load_default()
        .map_err(|e| ApiError::Internal(e.to_string()))?;

    // Find container
    let record = state.list(true)
        .into_iter()
        .find(|r| r.id.starts_with(&id) || r.name == id)
        .ok_or_else(|| ApiError::NotFound(format!("Container {} not found", id)))?;

    if record.status != "running" {
        return Err(ApiError::Conflict("Container is not running".to_string()));
    }

    // Generate exec ID
    let exec_id = uuid::Uuid::new_v4().to_string();

    // Store exec configuration (in-memory for now)
    // TODO: Implement proper exec instance storage

    Ok(Json(json!({
        "Id": exec_id
    })))
}

/// Request body for exec start.
#[derive(Debug, Deserialize, Default)]
pub struct ExecStartRequest {
    /// Detach from exec
    #[serde(rename = "Detach")]
    detach: Option<bool>,

    /// Allocate a pseudo-TTY
    #[serde(rename = "Tty")]
    tty: Option<bool>,
}

/// POST /exec/:id/start - Start exec instance.
pub async fn exec_start(
    Path(exec_id): Path<String>,
    Json(req): Json<ExecStartRequest>,
) -> ApiResult<StatusCode> {
    // For detached mode, just return success
    if req.detach.unwrap_or(false) {
        return Ok(StatusCode::OK);
    }

    // For non-detached mode, we need to find the container and execute the command
    // Since we don't have persistent exec instance storage yet, return OK
    // TODO: Implement exec instance storage and actual command execution
    Ok(StatusCode::OK)
}
