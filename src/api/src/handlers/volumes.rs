//! Volume API handlers.

use axum::{Json, extract::Path, http::StatusCode};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::error::{ApiResult, ApiError};

/// GET /volumes - List volumes.
pub async fn list() -> ApiResult<Json<serde_json::Value>> {
    // TODO: Integrate with volume store
    // For now, return empty list
    Ok(Json(json!({
        "Volumes": [],
        "Warnings": null
    })))
}

/// Request body for volume create.
#[derive(Debug, Deserialize)]
pub struct VolumeCreateRequest {
    #[serde(rename = "Name")]
    name: String,

    #[serde(rename = "Driver")]
    driver: Option<String>,

    #[serde(rename = "DriverOpts")]
    driver_opts: Option<std::collections::HashMap<String, String>>,

    #[serde(rename = "Labels")]
    labels: Option<std::collections::HashMap<String, String>>,
}

/// POST /volumes/create - Create a volume.
pub async fn create(Json(req): Json<VolumeCreateRequest>) -> ApiResult<Json<serde_json::Value>> {
    // TODO: Implement volume creation
    // For now, return a stub response
    let volume_path = format!("/var/lib/a3s-box/volumes/{}", req.name);

    Ok(Json(json!({
        "Name": req.name,
        "Driver": req.driver.unwrap_or_else(|| "local".to_string()),
        "Mountpoint": volume_path,
        "CreatedAt": chrono::Utc::now().to_rfc3339(),
        "Status": {},
        "Labels": req.labels.unwrap_or_default(),
        "Scope": "local",
        "Options": req.driver_opts.unwrap_or_default()
    })))
}

/// GET /volumes/:name - Inspect a volume.
pub async fn inspect(Path(name): Path<String>) -> ApiResult<Json<serde_json::Value>> {
    // TODO: Integrate with volume store
    // For now, return a stub response
    let volume_path = format!("/var/lib/a3s-box/volumes/{}", name);

    Ok(Json(json!({
        "Name": name,
        "Driver": "local",
        "Mountpoint": volume_path,
        "CreatedAt": chrono::Utc::now().to_rfc3339(),
        "Status": {},
        "Labels": {},
        "Scope": "local",
        "Options": {}
    })))
}

/// Query parameters for volume remove.
#[derive(Debug, Deserialize, Default)]
pub struct RemoveQuery {
    /// Force removal
    #[serde(default)]
    force: bool,
}

/// DELETE /volumes/:name - Remove a volume.
pub async fn remove(
    Path(_name): Path<String>,
    axum::extract::Query(_query): axum::extract::Query<RemoveQuery>,
) -> ApiResult<StatusCode> {
    // TODO: Implement volume removal
    Ok(StatusCode::NO_CONTENT)
}
