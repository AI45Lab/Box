//! `a3s-box tag` command — create a tag that refers to an existing image.

use clap::Args;

use crate::image_usage;

#[derive(Args)]
pub struct ImageTagArgs {
    /// Source image reference
    pub source: String,

    /// Target image reference (new tag)
    pub target: String,
}

pub async fn execute(args: ImageTagArgs) -> Result<(), Box<dyn std::error::Error>> {
    let store = super::open_image_store()?;
    let images = store.list().await;

    let source = image_usage::resolve_stored_image(&images, &args.source)?
        .ok_or_else(|| format!("Image not found: {}", args.source))?;

    // Store with new reference pointing to the same digest directory (no disk copy)
    store
        .put(&args.target, &source.digest, &source.path)
        .await?;

    println!("{}", args.target);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use a3s_box_runtime::StoredImage;
    use chrono::Utc;
    use std::path::PathBuf;

    fn stored(reference: &str) -> StoredImage {
        StoredImage {
            reference: reference.to_string(),
            digest: "sha256:abc".to_string(),
            size_bytes: 1024,
            pulled_at: Utc::now(),
            last_used: Utc::now(),
            path: PathBuf::from("/tmp/image"),
        }
    }

    #[test]
    fn test_tag_source_resolution_accepts_normalized_alias() {
        let images = vec![stored("docker.io/library/alpine:latest")];
        let source = image_usage::resolve_stored_image(&images, "alpine:latest")
            .unwrap()
            .unwrap();

        assert_eq!(source.reference, "docker.io/library/alpine:latest");
    }
}
