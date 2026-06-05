//! `a3s-box image-inspect` command — display detailed image metadata as JSON.

use clap::Args;

use crate::image_usage;

#[derive(Args)]
pub struct ImageInspectArgs {
    /// Image reference to inspect
    pub image: String,
}

pub async fn execute(args: ImageInspectArgs) -> Result<(), Box<dyn std::error::Error>> {
    let store = super::open_image_store()?;
    let images = store.list().await;
    let stored = image_usage::resolve_required_stored_image(&images, &args.image)?;
    println!("{}", build_image_inspect_json(&stored)?);
    Ok(())
}

/// Try to inspect `reference` as an image. Returns `Ok(None)` when no image
/// matches (so a polymorphic `inspect` can fall back to other object types).
pub(crate) async fn try_image_inspect_json(
    reference: &str,
) -> Result<Option<String>, Box<dyn std::error::Error>> {
    let store = super::open_image_store()?;
    let images = store.list().await;
    match image_usage::resolve_stored_image(&images, reference)? {
        Some(stored) => Ok(Some(build_image_inspect_json(&stored)?)),
        None => Ok(None),
    }
}

/// Build the JSON inspection document for a stored image.
fn build_image_inspect_json(
    stored: &a3s_box_runtime::StoredImage,
) -> Result<String, Box<dyn std::error::Error>> {
    // Load OCI image to get full config
    let oci = a3s_box_runtime::OciImage::from_path(&stored.path)?;
    let config = oci.config();

    let env_map: serde_json::Map<String, serde_json::Value> = config
        .env
        .iter()
        .map(|(k, v)| (k.clone(), serde_json::Value::String(v.clone())))
        .collect();
    let healthcheck = config.health_check.as_ref().map(|hc| {
        serde_json::json!({
            "Test": hc.test.clone(),
            "Interval": hc.interval,
            "Timeout": hc.timeout,
            "Retries": hc.retries,
            "StartPeriod": hc.start_period,
        })
    });

    let output = serde_json::json!({
        "Reference": stored.reference,
        "Digest": stored.digest,
        "Size": stored.size_bytes,
        "PulledAt": stored.pulled_at.to_rfc3339(),
        "Config": {
            "Entrypoint": config.entrypoint,
            "Cmd": config.cmd,
            "Env": env_map,
            "WorkingDir": config.working_dir,
            "User": config.user,
            "ExposedPorts": config.exposed_ports,
            "Volumes": config.volumes,
            "StopSignal": config.stop_signal,
            "Healthcheck": healthcheck,
            "OnBuild": config.onbuild,
            "Labels": config.labels,
        },
        "LayerCount": oci.layer_paths().len(),
    });

    // Docker's inspect family always returns a top-level JSON array (even for a
    // single image), so tooling can do `inspect X | jq '.[0].Config'`.
    Ok(serde_json::to_string_pretty(&serde_json::json!([output]))?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use a3s_box_runtime::StoredImage;
    use chrono::Utc;
    use std::path::PathBuf;

    #[test]
    fn test_image_inspect_resolution_accepts_normalized_alias() {
        let images = vec![StoredImage {
            reference: "docker.io/library/alpine:latest".to_string(),
            digest: "sha256:abc".to_string(),
            size_bytes: 1024,
            pulled_at: Utc::now(),
            last_used: Utc::now(),
            path: PathBuf::from("/tmp/image"),
        }];

        let stored = image_usage::resolve_required_stored_image(&images, "alpine:latest").unwrap();

        assert_eq!(stored.reference, "docker.io/library/alpine:latest");
    }
}
