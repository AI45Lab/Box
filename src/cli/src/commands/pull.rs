//! `a3s-box pull` command.

use std::sync::Arc;

use clap::Args;

#[derive(Args)]
pub struct PullArgs {
    /// Image reference (e.g., "alpine:latest", "ghcr.io/org/image:tag")
    pub image: String,

    /// Suppress progress output
    #[arg(short, long)]
    pub quiet: bool,

    /// Set target platform (e.g., "linux/amd64", "linux/arm64")
    #[arg(long)]
    pub platform: Option<String>,

    /// Verify image signature with a cosign public key file
    #[arg(long, value_name = "KEY_FILE")]
    pub verify_key: Option<String>,

    /// Verify image signature with keyless cosign (issuer and identity)
    #[arg(long, value_name = "ISSUER", requires = "verify_identity")]
    pub verify_issuer: Option<String>,

    /// Identity (email/URI) for keyless signature verification
    #[arg(long, value_name = "IDENTITY")]
    pub verify_identity: Option<String>,
}

pub async fn execute(args: PullArgs) -> Result<(), Box<dyn std::error::Error>> {
    let store = Arc::new(super::open_image_store()?);

    // Parse reference to determine registry for credential lookup
    let reference = a3s_box_runtime::ImageReference::parse(&args.image)?;
    let auth = a3s_box_runtime::RegistryAuth::from_credential_store(&reference.registry);

    let mut puller = a3s_box_runtime::ImagePuller::new(store, auth);

    // Configure signature verification policy
    let policy = if let Some(ref key_path) = args.verify_key {
        a3s_box_runtime::SignaturePolicy::CosignKey {
            public_key: key_path.clone(),
        }
    } else if let (Some(ref issuer), Some(ref identity)) =
        (&args.verify_issuer, &args.verify_identity)
    {
        a3s_box_runtime::SignaturePolicy::CosignKeyless {
            issuer: issuer.clone(),
            identity: identity.clone(),
        }
    } else {
        a3s_box_runtime::SignaturePolicy::Skip
    };

    puller = puller.with_signature_policy(policy);

    if !args.quiet {
        println!("Pulling {}...", args.image);
    }
    let image = puller.pull(&args.image).await?;

    if args.quiet {
        println!("{}", image.root_dir().display());
    } else {
        println!("Pulled: {} ({})", args.image, image.root_dir().display());
    }

    Ok(())
}
