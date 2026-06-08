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

/// Validate a tag target's reference grammar. Docker requires the repository
/// name (everything before the `:tag`/`@digest`) to be lowercase.
fn validate_tag_target(target: &str) -> Result<(), String> {
    let without_digest = target.split('@').next().unwrap_or(target);
    let last_slash = without_digest.rfind('/');
    // Strip a trailing `:tag` only when the colon is part of the tag, not a
    // `registry:port`.
    let repo = match without_digest.rfind(':') {
        Some(colon) if last_slash.is_none_or(|slash| colon > slash) => &without_digest[..colon],
        _ => without_digest,
    };
    if repo.bytes().any(|b| b.is_ascii_uppercase()) {
        return Err(format!(
            "invalid reference format: repository name must be lowercase: '{target}'"
        ));
    }
    Ok(())
}

pub async fn execute(args: ImageTagArgs) -> Result<(), Box<dyn std::error::Error>> {
    validate_tag_target(&args.target)?;

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
    fn test_validate_tag_target_rejects_uppercase_repo() {
        assert!(validate_tag_target("BadRepo:Tag").is_err());
        assert!(validate_tag_target("myrepo:V1").is_ok()); // uppercase tag is fine
        assert!(validate_tag_target("localhost:5000/myrepo:tag").is_ok());
        assert!(validate_tag_target("alpine:latest").is_ok());
        assert!(validate_tag_target("ns/Sub:tag").is_err());
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
