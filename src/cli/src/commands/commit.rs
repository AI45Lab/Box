//! `a3s-box commit` command — Create an image from a box's filesystem.
//!
//! Packages the box's rootfs into an OCI image and stores it in the
//! local image store, similar to `docker commit`.

use std::path::Path;
use std::sync::Arc;

use clap::Args;
use sha2::{Digest, Sha256};

use crate::resolve;
use crate::state::StateFile;

#[derive(Args)]
pub struct CommitArgs {
    /// Box name or ID
    pub name: String,

    /// Repository name and optionally a tag (e.g., "myimage:latest")
    pub repository: Option<String>,

    /// Commit message
    #[arg(short, long)]
    pub message: Option<String>,

    /// Author (e.g., "Name <email>")
    #[arg(short, long)]
    pub author: Option<String>,

    /// Apply Dockerfile instruction (e.g., "CMD /bin/sh")
    #[arg(short, long)]
    pub change: Vec<String>,

    /// Pause the box during commit
    #[arg(short, long, default_value = "true")]
    pub pause: bool,
}

pub async fn execute(args: CommitArgs) -> Result<(), Box<dyn std::error::Error>> {
    let state = StateFile::load_default()?;
    let record = resolve::resolve(&state, &args.name)?;

    let rootfs_dir = super::resolve_box_rootfs(&record.box_dir).ok_or_else(|| {
        format!(
            "Rootfs not found for box '{}' under {} (looked for merged/ and rootfs/). \
             For overlay-backed boxes the filesystem is only available while the box exists; \
             commit a running box.",
            args.name,
            record.box_dir.display()
        )
    })?;

    let reference = args.repository.unwrap_or_else(|| {
        format!(
            "{}:latest",
            record.image.split(':').next().unwrap_or("committed")
        )
    });

    println!("Committing {}...", record.name);

    // Create a temporary directory for the OCI image layout
    let tmp = tempfile::tempdir().map_err(|e| format!("Failed to create temp dir: {e}"))?;
    let image_dir = tmp.path();

    // Build OCI image layout
    build_oci_image(
        image_dir,
        &rootfs_dir,
        &reference,
        &args.message,
        &args.author,
        &args.change,
    )?;

    // Compute image digest from manifest
    let manifest_bytes = std::fs::read(image_dir.join("manifest.json")).or_else(|_| {
        // Read the actual manifest blob
        find_manifest_blob(image_dir)
    })?;
    let digest = format!("sha256:{:x}", Sha256::digest(&manifest_bytes));

    // Store in image store
    let store = Arc::new(super::open_image_store()?);
    let stored = store.put(&reference, &digest, image_dir).await?;

    println!(
        "sha256:{}",
        &stored
            .digest
            .strip_prefix("sha256:")
            .unwrap_or(&stored.digest)
    );

    Ok(())
}

/// Find the manifest blob in the OCI layout.
fn find_manifest_blob(image_dir: &Path) -> Result<Vec<u8>, std::io::Error> {
    let index_path = image_dir.join("index.json");
    let index_data = std::fs::read_to_string(&index_path)?;
    let index: serde_json::Value = serde_json::from_str(&index_data)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;

    if let Some(digest) = index["manifests"][0]["digest"].as_str() {
        let hex = digest.strip_prefix("sha256:").unwrap_or(digest);
        let blob_path = image_dir.join("blobs").join("sha256").join(hex);
        std::fs::read(&blob_path)
    } else {
        Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "No manifest digest in index.json",
        ))
    }
}

/// Build a minimal OCI image layout from a rootfs directory.
fn build_oci_image(
    output_dir: &Path,
    rootfs_dir: &Path,
    _reference: &str,
    message: &Option<String>,
    author: &Option<String>,
    changes: &[String],
) -> Result<(), Box<dyn std::error::Error>> {
    use flate2::write::GzEncoder;
    use flate2::Compression;

    let blobs_dir = output_dir.join("blobs").join("sha256");
    std::fs::create_dir_all(&blobs_dir)?;

    // 1. Create layer tarball (gzipped)
    let layer_path = blobs_dir.join("layer.tmp");
    {
        let file = std::fs::File::create(&layer_path)?;
        let encoder = GzEncoder::new(file, Compression::default());
        let mut builder = tar::Builder::new(encoder);
        builder.follow_symlinks(false);
        builder
            .append_dir_all(".", rootfs_dir)
            .map_err(|e| format!("Failed to archive rootfs: {e}"))?;
        builder
            .finish()
            .map_err(|e| format!("Failed to finalize layer: {e}"))?;
    }

    // Hash the layer
    let layer_bytes = std::fs::read(&layer_path)?;
    let layer_digest = format!("{:x}", Sha256::digest(&layer_bytes));
    let layer_size = layer_bytes.len() as u64;
    let layer_blob = blobs_dir.join(&layer_digest);
    std::fs::rename(&layer_path, &layer_blob)?;

    // Compute diff_id (sha256 of uncompressed tar)
    let diff_id = compute_diff_id(rootfs_dir)?;

    // 2. Create image config
    let mut config_obj = serde_json::json!({
        "architecture": std::env::consts::ARCH,
        "os": "linux",
        "config": {},
        "rootfs": {
            "type": "layers",
            "diff_ids": [format!("sha256:{diff_id}")]
        },
        "history": [{
            "created": chrono::Utc::now().to_rfc3339(),
            "created_by": "a3s-box commit",
            "comment": message.as_deref().unwrap_or(""),
            "author": author.as_deref().unwrap_or("")
        }]
    });

    // Apply --change directives to config
    apply_changes(&mut config_obj, changes);

    let config_bytes = serde_json::to_vec_pretty(&config_obj)?;
    let config_digest = format!("{:x}", Sha256::digest(&config_bytes));
    let config_size = config_bytes.len() as u64;
    std::fs::write(blobs_dir.join(&config_digest), &config_bytes)?;

    // 3. Create manifest
    let manifest = serde_json::json!({
        "schemaVersion": 2,
        "mediaType": "application/vnd.oci.image.manifest.v1+json",
        "config": {
            "mediaType": "application/vnd.oci.image.config.v1+json",
            "digest": format!("sha256:{config_digest}"),
            "size": config_size
        },
        "layers": [{
            "mediaType": "application/vnd.oci.image.layer.v1.tar+gzip",
            "digest": format!("sha256:{layer_digest}"),
            "size": layer_size
        }]
    });

    let manifest_bytes = serde_json::to_vec_pretty(&manifest)?;
    let manifest_digest = format!("{:x}", Sha256::digest(&manifest_bytes));
    let manifest_size = manifest_bytes.len() as u64;
    std::fs::write(blobs_dir.join(&manifest_digest), &manifest_bytes)?;

    // Also write as manifest.json for digest computation
    std::fs::write(output_dir.join("manifest.json"), &manifest_bytes)?;

    // 4. Create index.json
    let index = serde_json::json!({
        "schemaVersion": 2,
        "mediaType": "application/vnd.oci.image.index.v1+json",
        "manifests": [{
            "mediaType": "application/vnd.oci.image.manifest.v1+json",
            "digest": format!("sha256:{manifest_digest}"),
            "size": manifest_size
        }]
    });
    std::fs::write(
        output_dir.join("index.json"),
        serde_json::to_vec_pretty(&index)?,
    )?;

    // 5. Create oci-layout
    std::fs::write(
        output_dir.join("oci-layout"),
        r#"{"imageLayoutVersion":"1.0.0"}"#,
    )?;

    Ok(())
}

/// Compute the diff_id (sha256 of uncompressed tar) for a directory.
fn compute_diff_id(rootfs_dir: &Path) -> Result<String, Box<dyn std::error::Error>> {
    let mut hasher = Sha256::new();
    let buf = Vec::new();
    let mut builder = tar::Builder::new(buf);
    builder.follow_symlinks(false);
    builder.append_dir_all(".", rootfs_dir)?;
    builder.finish()?;
    let data = builder.into_inner()?;
    hasher.update(&data);
    Ok(format!("{:x}", hasher.finalize()))
}

/// Apply Dockerfile-style change directives to the image config.
fn apply_changes(config: &mut serde_json::Value, changes: &[String]) {
    for change in changes {
        let trimmed = change.trim();
        if let Some(rest) = trimmed.strip_prefix("CMD ") {
            // Honor exec form (CMD ["a","b"]) vs shell form, like Docker/import.
            config["config"]["Cmd"] = super::import::parse_exec_or_shell(rest);
        } else if let Some(rest) = trimmed.strip_prefix("ENTRYPOINT ") {
            config["config"]["Entrypoint"] = super::import::parse_exec_or_shell(rest);
        } else if let Some(rest) = trimmed.strip_prefix("ENV ") {
            // Accept both KEY=VALUE and the legacy space-separated KEY VALUE form.
            if let Ok((k, v)) = super::import::parse_key_value(rest) {
                let mut env = config["config"]["Env"]
                    .as_array()
                    .cloned()
                    .unwrap_or_default();
                env.push(serde_json::json!(format!("{k}={v}")));
                config["config"]["Env"] = serde_json::json!(env);
            }
        } else if let Some(rest) = trimmed.strip_prefix("EXPOSE ") {
            let ports = config["config"]["ExposedPorts"]
                .as_object()
                .cloned()
                .unwrap_or_default();
            let mut ports = ports;
            ports.insert(format!("{rest}/tcp"), serde_json::json!({}));
            config["config"]["ExposedPorts"] = serde_json::json!(ports);
        } else if let Some(rest) = trimmed.strip_prefix("WORKDIR ") {
            config["config"]["WorkingDir"] = serde_json::json!(rest);
        } else if let Some(rest) = trimmed.strip_prefix("USER ") {
            config["config"]["User"] = serde_json::json!(rest);
        } else if let Some(rest) = trimmed.strip_prefix("LABEL ") {
            if let Ok((k, v)) = super::import::parse_key_value(rest) {
                let mut labels = config["config"]["Labels"]
                    .as_object()
                    .cloned()
                    .unwrap_or_default();
                labels.insert(k, serde_json::json!(v));
                config["config"]["Labels"] = serde_json::json!(labels);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_apply_changes_cmd() {
        let mut config = serde_json::json!({"config": {}});
        apply_changes(&mut config, &["CMD /bin/bash".to_string()]);
        assert_eq!(
            config["config"]["Cmd"],
            serde_json::json!(["/bin/sh", "-c", "/bin/bash"])
        );
    }

    #[test]
    fn test_apply_changes_entrypoint() {
        let mut config = serde_json::json!({"config": {}});
        apply_changes(&mut config, &["ENTRYPOINT /app/start".to_string()]);
        assert_eq!(
            config["config"]["Entrypoint"],
            serde_json::json!(["/bin/sh", "-c", "/app/start"])
        );
    }

    #[test]
    fn test_apply_changes_env() {
        let mut config = serde_json::json!({"config": {}});
        apply_changes(&mut config, &["ENV FOO=bar".to_string()]);
        let env = config["config"]["Env"].as_array().unwrap();
        assert_eq!(env.len(), 1);
        assert_eq!(env[0], "FOO=bar");
    }

    #[test]
    fn test_apply_changes_workdir() {
        let mut config = serde_json::json!({"config": {}});
        apply_changes(&mut config, &["WORKDIR /app".to_string()]);
        assert_eq!(config["config"]["WorkingDir"], "/app");
    }

    #[test]
    fn test_apply_changes_user() {
        let mut config = serde_json::json!({"config": {}});
        apply_changes(&mut config, &["USER nobody".to_string()]);
        assert_eq!(config["config"]["User"], "nobody");
    }

    #[test]
    fn test_apply_changes_label() {
        let mut config = serde_json::json!({"config": {}});
        apply_changes(&mut config, &["LABEL version=1.0".to_string()]);
        assert_eq!(config["config"]["Labels"]["version"], "1.0");
    }

    #[test]
    fn test_apply_changes_expose() {
        let mut config = serde_json::json!({"config": {}});
        apply_changes(&mut config, &["EXPOSE 8080".to_string()]);
        assert!(config["config"]["ExposedPorts"]["8080/tcp"].is_object());
    }

    #[test]
    fn test_apply_changes_multiple() {
        let mut config = serde_json::json!({"config": {}});
        apply_changes(
            &mut config,
            &[
                "CMD /start".to_string(),
                "ENV APP=test".to_string(),
                "WORKDIR /opt".to_string(),
            ],
        );
        assert!(config["config"]["Cmd"].is_array());
        assert!(config["config"]["Env"].is_array());
        assert_eq!(config["config"]["WorkingDir"], "/opt");
    }

    #[test]
    fn test_apply_changes_empty() {
        let mut config = serde_json::json!({"config": {}});
        apply_changes(&mut config, &[]);
        assert_eq!(config["config"], serde_json::json!({}));
    }

    #[test]
    fn test_compute_diff_id() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("hello.txt"), "world").unwrap();
        let id = compute_diff_id(dir.path()).unwrap();
        assert!(!id.is_empty());
        assert_eq!(id.len(), 64); // sha256 hex
    }

    #[test]
    fn test_build_oci_image() {
        let rootfs = tempfile::tempdir().unwrap();
        std::fs::write(rootfs.path().join("test.txt"), "data").unwrap();

        let output = tempfile::tempdir().unwrap();
        build_oci_image(
            output.path(),
            rootfs.path(),
            "test:latest",
            &Some("test commit".to_string()),
            &Some("tester".to_string()),
            &[],
        )
        .unwrap();

        // Verify OCI layout
        assert!(output.path().join("oci-layout").exists());
        assert!(output.path().join("index.json").exists());
        assert!(output.path().join("blobs/sha256").exists());

        // Verify index.json is valid
        let index: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(output.path().join("index.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(index["schemaVersion"], 2);
        assert!(index["manifests"][0]["digest"]
            .as_str()
            .unwrap()
            .starts_with("sha256:"));
    }
}
