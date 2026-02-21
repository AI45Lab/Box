//! `a3s-box push` command — Push a local image to a registry.
//!
//! Optionally signs the image after push using a cosign-compatible
//! ECDSA P-256 private key (`--sign-key`).

use std::sync::Arc;

use clap::Args;

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
    let store = Arc::new(super::open_image_store()?);

    // Parse the target reference
    let reference = a3s_box_runtime::ImageReference::parse(&args.image)?;

    // Look up the image in the local store
    let stored = store.get(&args.image).await.ok_or_else(|| {
        format!(
            "Image '{}' not found locally. Pull or build it first.",
            args.image
        )
    })?;

    if !args.quiet {
        println!("Pushing {}...", args.image);
    }

    // Load auth from credential store (falls back to env vars, then anonymous)
    let auth = a3s_box_runtime::RegistryAuth::from_credential_store(&reference.registry);
    let pusher = a3s_box_runtime::RegistryPusher::with_auth(auth);

    let result = pusher.push(&reference, &stored.path).await?;

    if args.quiet {
        println!("{}", result.manifest_url);
    } else {
        println!("Pushed: {} ({})", args.image, result.manifest_url);
    }

    // Sign the image if --sign-key is provided
    if let Some(ref key_path) = args.sign_key {
        if !args.quiet {
            println!("Signing {}...", args.image);
        }

        let sign_result = a3s_box_runtime::oci::signing::sign_image(
            key_path,
            &reference.registry,
            &reference.repository,
            &result.manifest_digest,
            &args.image,
        )
        .await?;

        if !args.quiet {
            println!("Signed: {} ({})", args.image, sign_result.signature_tag);
        }
    }

    Ok(())
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
}
