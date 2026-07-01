//! `a3s-box push` command — Push a local image to a registry.
//!
//! Optionally signs the image after push using a cosign-compatible
//! ECDSA P-256 private key (`--sign-key`).

use clap::Args;

use crate::image_usage;

#[derive(Args)]
pub struct PushArgs {
    /// Image reference (e.g., "ghcr.io/org/image:tag")
    pub image: String,

    /// Suppress progress output
    #[arg(short, long)]
    pub quiet: bool,

    /// Sign the image after push with a cosign-compatible ECDSA P-256 private key
    #[arg(long)]
    pub sign_key: Option<String>,
}

pub async fn execute(args: PushArgs) -> Result<(), Box<dyn std::error::Error>> {
    let store = super::open_image_store()?;
    let images = store.list().await;

    // Look up the image in the local store
    let stored = image_usage::resolve_stored_image(&images, &args.image)?.ok_or_else(|| {
        format!(
            "Image '{}' not found locally. Pull or build it first.",
            args.image
        )
    })?;
    let push_reference = push_reference_for_query(&args.image, &stored.reference)?;
    let reference = a3s_box_runtime::ImageReference::parse(&push_reference)?;

    if !args.quiet {
        println!("Pushing {push_reference}...");
    }

    // Load auth from credential store (falls back to env vars, then anonymous)
    let auth = a3s_box_runtime::RegistryAuth::from_credential_store(&reference.registry);
    let pusher = a3s_box_runtime::RegistryPusher::with_auth(auth);

    let result = pusher.push(&reference, &stored.path).await?;

    if args.quiet {
        println!("{}", result.manifest_url);
    } else {
        println!("Pushed: {} ({})", push_reference, result.manifest_url);
    }

    // Sign the image if --sign-key is provided
    if let Some(ref key_path) = args.sign_key {
        if !args.quiet {
            println!("Signing {push_reference}...");
        }

        let sign_result = a3s_box_runtime::oci::signing::sign_image(
            key_path,
            &reference.registry,
            &reference.repository,
            &result.manifest_digest,
            &push_reference,
        )
        .await?;

        if !args.quiet {
            println!("Signed: {} ({})", push_reference, sign_result.signature_tag);
        }
    }

    Ok(())
}

fn push_reference_for_query(query: &str, resolved_reference: &str) -> Result<String, String> {
    let query = query.trim();
    if image_usage::is_dangling_reference(query) {
        if image_usage::is_dangling_reference(resolved_reference) {
            return Err(
                "Cannot push a digest-only image reference. Tag it first with `a3s-box tag`."
                    .to_string(),
            );
        }
        return Ok(resolved_reference.to_string());
    }

    Ok(query.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_push_args_defaults() {
        let args = PushArgs {
            image: "ghcr.io/org/app:latest".to_string(),
            quiet: false,
            sign_key: None,
        };
        assert!(!args.quiet);
        assert!(args.sign_key.is_none());
    }

    #[test]
    fn test_push_args_with_sign_key() {
        let args = PushArgs {
            image: "ghcr.io/org/app:latest".to_string(),
            quiet: false,
            sign_key: Some("/path/to/cosign.key".to_string()),
        };
        assert_eq!(args.sign_key.as_deref(), Some("/path/to/cosign.key"));
    }

    #[test]
    fn test_push_reference_for_named_query_uses_query() {
        assert_eq!(
            push_reference_for_query("alpine:latest", "docker.io/library/alpine:latest").unwrap(),
            "alpine:latest"
        );
    }

    #[test]
    fn test_push_reference_for_digest_query_uses_resolved_reference() {
        assert_eq!(
            push_reference_for_query("sha256:abc", "example.com/app:latest").unwrap(),
            "example.com/app:latest"
        );
    }

    #[test]
    fn test_push_reference_rejects_digest_only_resolved_reference() {
        let error = push_reference_for_query("sha256:abc", "sha256:abc").unwrap_err();

        assert!(error.contains("Tag it first"));
    }
}
