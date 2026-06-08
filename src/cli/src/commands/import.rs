//! `a3s-box import` command — create a single-layer image from a rootfs tarball.
//!
//! Mirrors `docker import`: wrap a filesystem tarball (`.tar` or `.tar.gz`) as a
//! single-layer OCI image, optionally applying Dockerfile-style `--change`
//! directives to the image config. This is distinct from `load`, which imports
//! a full OCI/Docker image archive.

use std::io::Read;
use std::sync::Arc;

use clap::Args;
use sha2::{Digest, Sha256};

#[derive(Args)]
pub struct ImportArgs {
    /// Path to the rootfs tarball (.tar or .tar.gz)
    pub file: String,

    /// Image reference to assign (e.g. "myimage:latest")
    pub reference: Option<String>,

    /// Apply a Dockerfile instruction to the image config (repeatable):
    /// CMD, ENTRYPOINT, ENV, WORKDIR, USER, EXPOSE, LABEL, VOLUME.
    #[arg(short = 'c', long = "change")]
    pub change: Vec<String>,

    /// Commit message (stored in the image history)
    #[arg(short = 'm', long = "message")]
    pub message: Option<String>,
}

pub async fn execute(args: ImportArgs) -> Result<(), Box<dyn std::error::Error>> {
    let raw = std::fs::read(&args.file)
        .map_err(|e| format!("Failed to read import source '{}': {}", args.file, e))?;

    // Accept gzip-compressed or plain tar input. The layer's diff_id is the
    // SHA256 of the UNCOMPRESSED tar; the layer blob is gzip-compressed.
    let uncompressed = if raw.starts_with(&[0x1f, 0x8b]) {
        let mut d = flate2::read::GzDecoder::new(&raw[..]);
        let mut out = Vec::new();
        d.read_to_end(&mut out)
            .map_err(|e| format!("Failed to decompress import source: {e}"))?;
        out
    } else {
        raw
    };
    let diff_id = hex::encode(Sha256::digest(&uncompressed));

    // Gzip the uncompressed tar to form the layer blob.
    let layer_blob = gzip(&uncompressed)?;
    let layer_digest = hex::encode(Sha256::digest(&layer_blob));

    // Assemble an OCI layout in a temp dir.
    let staging =
        tempfile::TempDir::new().map_err(|e| format!("Failed to create staging dir: {e}"))?;
    let blobs = staging.path().join("blobs").join("sha256");
    std::fs::create_dir_all(&blobs)?;
    std::fs::write(blobs.join(&layer_digest), &layer_blob)?;

    let reference = args
        .reference
        .clone()
        .unwrap_or_else(|| "a3s-import:latest".to_string());
    let host_platform = a3s_box_core::platform::Platform::host();
    let arch = host_platform.oci_arch();
    let now = chrono::Utc::now().to_rfc3339();

    let mut config_section = serde_json::Map::new();
    apply_changes(&args.change, &mut config_section)?;

    let history_msg = args
        .message
        .clone()
        .unwrap_or_else(|| "Imported from tarball".to_string());
    let config = serde_json::json!({
        "architecture": arch,
        "os": "linux",
        "created": now,
        "config": serde_json::Value::Object(config_section),
        "rootfs": { "type": "layers", "diff_ids": [format!("sha256:{diff_id}")] },
        "history": [{ "created": now, "created_by": history_msg }],
    });
    let config_bytes = serde_json::to_vec_pretty(&config)?;
    let config_digest = hex::encode(Sha256::digest(&config_bytes));
    std::fs::write(blobs.join(&config_digest), &config_bytes)?;

    let manifest = serde_json::json!({
        "schemaVersion": 2,
        "mediaType": "application/vnd.oci.image.manifest.v1+json",
        "config": {
            "mediaType": "application/vnd.oci.image.config.v1+json",
            "digest": format!("sha256:{config_digest}"),
            "size": config_bytes.len(),
        },
        "layers": [{
            "mediaType": "application/vnd.oci.image.layer.v1.tar+gzip",
            "digest": format!("sha256:{layer_digest}"),
            "size": layer_blob.len(),
        }],
    });
    let manifest_bytes = serde_json::to_vec_pretty(&manifest)?;
    let manifest_digest = hex::encode(Sha256::digest(&manifest_bytes));
    std::fs::write(blobs.join(&manifest_digest), &manifest_bytes)?;

    let index = serde_json::json!({
        "schemaVersion": 2,
        "mediaType": "application/vnd.oci.image.index.v1+json",
        "manifests": [{
            "mediaType": "application/vnd.oci.image.manifest.v1+json",
            "digest": format!("sha256:{manifest_digest}"),
            "size": manifest_bytes.len(),
            "platform": { "os": "linux", "architecture": arch },
        }],
    });
    std::fs::write(
        staging.path().join("index.json"),
        serde_json::to_string_pretty(&index)?,
    )?;
    std::fs::write(
        staging.path().join("oci-layout"),
        r#"{"imageLayoutVersion":"1.0.0"}"#,
    )?;

    let store = Arc::new(super::open_image_store()?);
    let digest_str = format!("sha256:{manifest_digest}");
    let stored = store.put(&reference, &digest_str, staging.path()).await?;

    println!(
        "Imported {} ({})",
        stored.reference,
        crate::output::format_bytes(stored.size_bytes)
    );
    println!("{digest_str}");
    Ok(())
}

fn gzip(data: &[u8]) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    use flate2::write::GzEncoder;
    use flate2::Compression;
    use std::io::Write;
    let mut enc = GzEncoder::new(Vec::new(), Compression::default());
    enc.write_all(data)?;
    Ok(enc.finish()?)
}

/// Apply Dockerfile-style `--change` directives to the image config section.
fn apply_changes(
    changes: &[String],
    config: &mut serde_json::Map<String, serde_json::Value>,
) -> Result<(), String> {
    for change in changes {
        let trimmed = change.trim();
        let (instr, rest) = match trimmed.split_once(char::is_whitespace) {
            Some((i, r)) => (i.to_ascii_uppercase(), r.trim()),
            None => (trimmed.to_ascii_uppercase(), ""),
        };
        match instr.as_str() {
            "CMD" => config.insert("Cmd".into(), parse_exec_or_shell(rest)),
            "ENTRYPOINT" => config.insert("Entrypoint".into(), parse_exec_or_shell(rest)),
            "WORKDIR" => config.insert("WorkingDir".into(), serde_json::json!(rest)),
            "USER" => config.insert("User".into(), serde_json::json!(rest)),
            "ENV" => {
                let (k, v) = parse_key_value(rest)?;
                let entry = format!("{k}={v}");
                let env = config
                    .entry("Env")
                    .or_insert_with(|| serde_json::json!([]));
                if let Some(arr) = env.as_array_mut() {
                    arr.push(serde_json::json!(entry));
                }
                None
            }
            "LABEL" => {
                let (k, v) = parse_key_value(rest)?;
                let labels = config
                    .entry("Labels")
                    .or_insert_with(|| serde_json::json!({}));
                if let Some(map) = labels.as_object_mut() {
                    map.insert(k, serde_json::json!(v));
                }
                None
            }
            "EXPOSE" => {
                let ports = config
                    .entry("ExposedPorts")
                    .or_insert_with(|| serde_json::json!({}));
                if let Some(map) = ports.as_object_mut() {
                    map.insert(format!("{rest}/tcp"), serde_json::json!({}));
                }
                None
            }
            "VOLUME" => {
                let vols = config
                    .entry("Volumes")
                    .or_insert_with(|| serde_json::json!({}));
                if let Some(map) = vols.as_object_mut() {
                    map.insert(rest.to_string(), serde_json::json!({}));
                }
                None
            }
            other => {
                return Err(format!(
                    "Unsupported --change instruction '{other}' (supported: CMD, ENTRYPOINT, ENV, WORKDIR, USER, EXPOSE, LABEL, VOLUME)"
                ))
            }
        };
    }
    Ok(())
}

/// Parse a CMD/ENTRYPOINT value as a JSON array (exec form) or a shell string.
pub(crate) fn parse_exec_or_shell(rest: &str) -> serde_json::Value {
    let t = rest.trim();
    if t.starts_with('[') {
        if let Ok(v) = serde_json::from_str::<Vec<String>>(t) {
            return serde_json::json!(v);
        }
    }
    // Shell form: wrap in /bin/sh -c
    serde_json::json!(["/bin/sh", "-c", t])
}

pub(crate) fn parse_key_value(rest: &str) -> Result<(String, String), String> {
    if let Some((k, v)) = rest.split_once('=') {
        Ok((k.trim().to_string(), v.trim().to_string()))
    } else if let Some((k, v)) = rest.split_once(char::is_whitespace) {
        Ok((k.trim().to_string(), v.trim().to_string()))
    } else {
        Err(format!("Invalid key/value in --change: '{rest}'"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_apply_changes_cmd_entrypoint_env() {
        let mut cfg = serde_json::Map::new();
        apply_changes(
            &[
                "CMD [\"/bin/sh\"]".into(),
                "ENV FOO=bar".into(),
                "WORKDIR /app".into(),
                "USER 1000".into(),
            ],
            &mut cfg,
        )
        .unwrap();
        assert_eq!(cfg["Cmd"], serde_json::json!(["/bin/sh"]));
        assert_eq!(cfg["Env"], serde_json::json!(["FOO=bar"]));
        assert_eq!(cfg["WorkingDir"], serde_json::json!("/app"));
        assert_eq!(cfg["User"], serde_json::json!("1000"));
    }

    #[test]
    fn test_apply_changes_shell_form_cmd() {
        let mut cfg = serde_json::Map::new();
        apply_changes(&["CMD echo hi".into()], &mut cfg).unwrap();
        assert_eq!(cfg["Cmd"], serde_json::json!(["/bin/sh", "-c", "echo hi"]));
    }

    #[test]
    fn test_apply_changes_rejects_unknown() {
        let mut cfg = serde_json::Map::new();
        let err = apply_changes(&["FROM scratch".into()], &mut cfg).unwrap_err();
        assert!(err.contains("Unsupported --change instruction 'FROM'"));
    }

    #[test]
    fn test_parse_exec_or_shell() {
        assert_eq!(
            parse_exec_or_shell("[\"a\",\"b\"]"),
            serde_json::json!(["a", "b"])
        );
        assert_eq!(
            parse_exec_or_shell("run me"),
            serde_json::json!(["/bin/sh", "-c", "run me"])
        );
    }
}
