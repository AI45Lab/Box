//! `a3s-box system-prune` command — Remove all unused data.
//!
//! Removes stopped boxes and unused images in one operation.

use clap::Args;

use crate::image_usage::{self, ImagePruneMode, ImageReferenceScope};
use crate::output;
use crate::state::StateFile;

#[derive(Args)]
pub struct SystemPruneArgs {
    /// Remove all unused images, not just dangling ones
    #[arg(short, long)]
    pub all: bool,

    /// Skip confirmation prompt
    #[arg(short, long)]
    pub force: bool,
}

pub async fn execute(args: SystemPruneArgs) -> Result<(), Box<dyn std::error::Error>> {
    if !args.force {
        println!("WARNING: This will remove:");
        println!("  - all created, stopped, and dead boxes");
        println!("  - all networks not used by at least one box");
        if args.all {
            println!("  - all images not used by active boxes");
        } else {
            println!("  - all dangling images");
        }
        println!();
        println!("Use --force to skip this prompt.");
        return Ok(());
    }

    let mut boxes_removed: usize = 0;
    let mut images_removed: usize = 0;
    let mut networks_removed: usize = 0;
    let mut space_freed: u64 = 0;

    // Phase 1: Remove stopped/dead boxes
    let mut state = StateFile::load_default()?;
    let all_boxes = state.list(true);

    let to_remove: Vec<(String, String, std::path::PathBuf)> = all_boxes
        .iter()
        .filter(|r| is_prunable_box(r))
        .map(|r| (r.id.clone(), r.name.clone(), r.box_dir.clone()))
        .collect();

    for (box_id, name, box_dir) in &to_remove {
        if box_dir.exists() {
            let _ = std::fs::remove_dir_all(box_dir);
        }
        if state.remove(box_id).is_ok() {
            boxes_removed += 1;
            println!("Removed box: {name}");
        }
    }

    // Phase 2: Remove unused images
    // Reload state to get current active boxes after removal.
    let state = StateFile::load_default()?;
    let protected_images = active_image_references(&state);
    let prune_mode = image_prune_mode(args.all);

    let images_dir = super::images_dir();
    if images_dir.exists() {
        if let Ok(store) = super::open_image_store() {
            let all_images = store.list().await;

            for image in &all_images {
                if image_usage::is_prunable_reference(
                    &image.reference,
                    &protected_images,
                    prune_mode,
                ) && store.remove(&image.reference).await.is_ok()
                {
                    space_freed += image.size_bytes;
                    images_removed += 1;
                    println!("Removed image: {}", image.reference);
                }
            }
        }
    }

    // Phase 3: Remove unused networks (mirrors `docker system prune`).
    // Reload state so freshly-removed boxes no longer count as network users.
    let state = StateFile::load_default()?;
    if let Ok(network_store) = a3s_box_runtime::NetworkStore::default_path() {
        let (removed, _errors) = super::network::prune_unused_networks(&network_store, &state);
        for name in &removed {
            networks_removed += 1;
            println!("Removed network: {name}");
        }
    }

    println!();
    println!(
        "Removed {} box(es), {} image(s), {} network(s), freed {}",
        boxes_removed,
        images_removed,
        networks_removed,
        output::format_bytes(space_freed)
    );

    Ok(())
}

fn is_prunable_box(record: &crate::state::BoxRecord) -> bool {
    matches!(record.status.as_str(), "stopped" | "dead" | "created")
}

fn active_image_references(state: &StateFile) -> std::collections::HashSet<String> {
    image_usage::referenced_images(state, ImageReferenceScope::ActiveBoxes)
}

fn image_prune_mode(all: bool) -> ImagePruneMode {
    if all {
        ImagePruneMode::Unused
    } else {
        ImagePruneMode::Dangling
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_helpers::fixtures::{make_record, setup_state};

    #[test]
    fn test_is_prunable_box_keeps_active_boxes() {
        assert!(!is_prunable_box(&make_record(
            "id-1",
            "running",
            "running",
            Some(1)
        )));
        assert!(!is_prunable_box(&make_record(
            "id-2",
            "paused",
            "paused",
            Some(1)
        )));
        assert!(is_prunable_box(&make_record(
            "id-3", "created", "created", None
        )));
        assert!(is_prunable_box(&make_record(
            "id-4", "stopped", "stopped", None
        )));
        assert!(is_prunable_box(&make_record("id-5", "dead", "dead", None)));
    }

    #[test]
    fn test_active_image_references_include_paused() {
        let mut running = make_record("id-1", "running", "running", Some(1));
        running.image = "alpine:latest".to_string();
        let mut paused = make_record("id-2", "paused", "paused", Some(1));
        paused.image = "redis:latest".to_string();
        let mut stopped = make_record("id-3", "stopped", "stopped", None);
        stopped.image = "nginx:latest".to_string();
        let (_tmp, state) = setup_state(vec![running, paused, stopped]);

        let used_images = active_image_references(&state);

        assert!(used_images.contains("alpine:latest"));
        assert!(used_images.contains("docker.io/library/alpine:latest"));
        assert!(used_images.contains("redis:latest"));
        assert!(!used_images.contains("nginx:latest"));
    }

    #[test]
    fn test_image_prune_mode_defaults_to_dangling() {
        assert_eq!(image_prune_mode(false), ImagePruneMode::Dangling);
        assert_eq!(image_prune_mode(true), ImagePruneMode::Unused);
    }
}
