//! `a3s-box load` command — Load an image from a tar archive.

use clap::Args;

#[derive(Args)]
pub struct LoadArgs {
    /// Input tar file path
    #[arg(short, long)]
    pub input: String,

    /// Tag to assign to the loaded image
    #[arg(short, long)]
    pub tag: Option<String>,
}

pub async fn execute(args: LoadArgs) -> Result<(), Box<dyn std::error::Error>> {
    let store = super::open_image_store()?;

    // Extract tar to a temporary directory
    let tmp_dir =
        tempfile::tempdir().map_err(|e| format!("Failed to create temp directory: {e}"))?;

    let file = std::fs::File::open(&args.input)
        .map_err(|e| format!("Failed to open {}: {e}", args.input))?;
    let mut archive = tar::Archive::new(file);
    archive
        .unpack(tmp_dir.path())
        .map_err(|e| format!("Failed to extract archive: {e}"))?;

    // Determine reference and digest from the OCI layout
    let index_path = tmp_dir.path().join("index.json");
    let index_content = std::fs::read_to_string(&index_path)
        .map_err(|e| format!("Failed to read index.json from archive: {e}"))?;
    let index: serde_json::Value =
        serde_json::from_str(&index_content).map_err(|e| format!("Invalid index.json: {e}"))?;

    let digest = index["manifests"][0]["digest"]
        .as_str()
        .ok_or("No manifest digest in index.json")?
        .to_string();

    let reference = load_reference(&index, args.tag.as_deref(), &digest)?;

    let stored = store.put(&reference, &digest, tmp_dir.path()).await?;

    println!(
        "Loaded image: {} ({})",
        stored.reference,
        crate::output::format_bytes(stored.size_bytes)
    );
    Ok(())
}

fn load_reference(
    index: &serde_json::Value,
    tag: Option<&str>,
    digest: &str,
) -> Result<String, String> {
    if let Some(tag) = tag.map(str::trim).filter(|tag| !tag.is_empty()) {
        return Ok(tag.to_string());
    }

    if tag.is_some() {
        return Err("Image tag cannot be empty".to_string());
    }

    Ok(
        index["manifests"][0]["annotations"]["org.opencontainers.image.ref.name"]
            .as_str()
            .map(str::trim)
            .filter(|reference| !reference.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| digest.to_string()),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_load_reference_prefers_explicit_tag() {
        let index = serde_json::json!({
            "manifests": [{
                "digest": "sha256:abc",
                "annotations": {
                    "org.opencontainers.image.ref.name": "from-index:latest"
                }
            }]
        });

        assert_eq!(
            load_reference(&index, Some("loaded:latest"), "sha256:abc").unwrap(),
            "loaded:latest"
        );
    }

    #[test]
    fn test_load_reference_uses_annotation_before_digest() {
        let index = serde_json::json!({
            "manifests": [{
                "digest": "sha256:abc",
                "annotations": {
                    "org.opencontainers.image.ref.name": "from-index:latest"
                }
            }]
        });

        assert_eq!(
            load_reference(&index, None, "sha256:abc").unwrap(),
            "from-index:latest"
        );
    }

    #[test]
    fn test_load_reference_falls_back_to_digest() {
        let index = serde_json::json!({
            "manifests": [{
                "digest": "sha256:abc"
            }]
        });

        assert_eq!(
            load_reference(&index, None, "sha256:abc").unwrap(),
            "sha256:abc"
        );
    }
}
