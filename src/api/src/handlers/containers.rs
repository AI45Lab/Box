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

/// GET /containers/:id/logs - Get container logs.
pub async fn logs(Path(_id): Path<String>) -> ApiResult<String> {
    Err(ApiError::NotImplemented("Container logs not yet implemented".to_string()))
}

/// GET /containers/:id/stats - Get container stats.
pub async fn stats(Path(_id): Path<String>) -> ApiResult<Json<serde_json::Value>> {
    Err(ApiError::NotImplemented("Container stats not yet implemented".to_string()))
}

/// GET /containers/:id/top - List processes.
pub async fn top(Path(_id): Path<String>) -> ApiResult<Json<serde_json::Value>> {
    Err(ApiError::NotImplemented("Container top not yet implemented".to_string()))
}

/// POST /containers/:id/pause - Pause a container.
pub async fn pause(Path(_id): Path<String>) -> ApiResult<StatusCode> {
    Err(ApiError::NotImplemented("Container pause not yet implemented".to_string()))
}

/// POST /containers/:id/unpause - Unpause a container.
pub async fn unpause(Path(_id): Path<String>) -> ApiResult<StatusCode> {
    Err(ApiError::NotImplemented("Container unpause not yet implemented".to_string()))
}

/// POST /containers/:id/wait - Wait for container to stop.
pub async fn wait(Path(_id): Path<String>) -> ApiResult<Json<serde_json::Value>> {
    Err(ApiError::NotImplemented("Container wait not yet implemented".to_string()))
}

/// POST /containers/:id/exec - Create exec instance.
pub async fn exec_create(Path(_id): Path<String>) -> ApiResult<Json<serde_json::Value>> {
    Err(ApiError::NotImplemented("Exec create not yet implemented".to_string()))
}

/// POST /exec/:id/start - Start exec instance.
pub async fn exec_start(Path(_id): Path<String>) -> ApiResult<StatusCode> {
    Err(ApiError::NotImplemented("Exec start not yet implemented".to_string()))
}
