//! `a3s-box image-prune` command — remove dangling or unused images.

use clap::Args;

use crate::image_usage::{self, ImagePruneMode, ImageReferenceScope};
use crate::output;
use crate::state::StateFile;

#[derive(Args)]
pub struct ImagePruneArgs {
    /// Remove all unused images, not just dangling ones
    #[arg(short, long)]
    pub all: bool,

    /// Skip confirmation prompt
    #[arg(short, long)]
    pub force: bool,
}

pub async fn execute(args: ImagePruneArgs) -> Result<(), Box<dyn std::error::Error>> {
    let store = super::open_image_store()?;

    // `image-prune` never removes images referenced by any existing box.
    let protected_images = match StateFile::load_default() {
        Ok(state) => image_usage::referenced_images(&state, ImageReferenceScope::AllBoxes),
        Err(_) => Default::default(),
    };
    let prune_mode = prune_mode(args.all);

    let all_images = store.list().await;

    // Determine which images to remove
    let to_remove: Vec<_> = all_images
        .iter()
        .filter(|img| {
            image_usage::is_prunable_reference(&img.reference, &protected_images, prune_mode)
        })
        .collect();

    if to_remove.is_empty() {
        println!("{}", empty_message(prune_mode));
        return Ok(());
    }

    // Show what will be removed
    if !args.force {
        println!("WARNING: This will remove {} image(s):", to_remove.len());
        for img in &to_remove {
            println!(
                "  {} ({})",
                img.reference,
                output::format_bytes(img.size_bytes)
            );
        }
        println!();
        println!("Use --force to skip this prompt.");
        return Ok(());
    }

    let mut freed: u64 = 0;
    let mut count: usize = 0;
    let mut errors: Vec<String> = Vec::new();

    for img in &to_remove {
        match store.remove(&img.reference).await {
            Ok(()) => {
                freed += img.size_bytes;
                count += 1;
            }
            Err(e) => {
                errors.push(format!("{}: {e}", img.reference));
            }
        }
    }

    println!(
        "Removed {} image(s), freed {}",
        count,
        output::format_bytes(freed)
    );

    if !errors.is_empty() {
        eprintln!("\nErrors:");
        for err in &errors {
            eprintln!("  {err}");
        }
    }

    Ok(())
}

fn prune_mode(all: bool) -> ImagePruneMode {
    if all {
        ImagePruneMode::Unused
    } else {
        ImagePruneMode::Dangling
    }
}

fn empty_message(mode: ImagePruneMode) -> &'static str {
    match mode {
        ImagePruneMode::Dangling => "No dangling images to remove.",
        ImagePruneMode::Unused => "No unused images to remove.",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_prune_mode_defaults_to_dangling() {
        assert_eq!(prune_mode(false), ImagePruneMode::Dangling);
        assert_eq!(prune_mode(true), ImagePruneMode::Unused);
    }

    #[test]
    fn test_empty_message_matches_mode() {
        assert_eq!(
            empty_message(ImagePruneMode::Dangling),
            "No dangling images to remove."
        );
        assert_eq!(
            empty_message(ImagePruneMode::Unused),
            "No unused images to remove."
        );
    }
}
