//! `a3s-box rmi` command — remove one or more cached images.

use clap::Args;

use crate::image_usage::{self, ImageReferenceScope};
use crate::state::StateFile;

#[derive(Args)]
pub struct RmiArgs {
    /// Image references to remove
    #[arg(required = true)]
    pub images: Vec<String>,

    /// Ignore missing images; images referenced by boxes are still protected
    #[arg(short, long)]
    pub force: bool,
}

pub async fn execute(args: RmiArgs) -> Result<(), Box<dyn std::error::Error>> {
    let store = super::open_image_store()?;
    let protected_images = match StateFile::load_default() {
        Ok(state) => image_usage::referenced_images(&state, ImageReferenceScope::AllBoxes),
        Err(_) => Default::default(),
    };

    let mut errors: Vec<String> = Vec::new();

    for query in &args.images {
        let images = store.list().await;
        let target = match image_usage::resolve_stored_image(&images, query) {
            Ok(Some(target)) => target,
            Ok(None) if args.force => continue,
            Ok(None) => {
                errors.push(format!("{query}: Image not found"));
                continue;
            }
            Err(error) => {
                errors.push(format!("{query}: {error}"));
                continue;
            }
        };

        if image_usage::is_protected_reference(&target.reference, &protected_images) {
            errors.push(format!(
                "{}: image is referenced by an existing box; remove the box before removing this image",
                target.reference
            ));
            continue;
        }

        match store.remove(&target.reference).await {
            Ok(()) => {
                println!("Removed: {}", target.reference);
            }
            Err(e) => {
                if args.force {
                    // Silently skip not-found errors in force mode
                    continue;
                }
                errors.push(format!("{}: {e}", target.reference));
            }
        }
    }

    if errors.is_empty() {
        Ok(())
    } else {
        let msg = errors.join("\n");
        Err(format!("Failed to remove image(s):\n{msg}").into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn test_protected_alias_blocks_rmi_target() {
        let mut protected = HashSet::new();
        protected.insert("docker.io/library/alpine:latest".to_string());

        assert!(image_usage::is_protected_reference(
            "alpine:latest",
            &protected
        ));
    }
}
