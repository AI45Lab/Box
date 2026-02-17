//! `a3s-box rm` command — Remove one or more boxes.

use clap::Args;

use crate::cleanup;
use crate::resolve;
use crate::state::StateFile;

#[derive(Args)]
pub struct RmArgs {
    /// Box name(s) or ID(s)
    #[arg(required = true)]
    pub boxes: Vec<String>,

    /// Force removal of running boxes (stops them first)
    #[arg(short, long)]
    pub force: bool,
}

pub async fn execute(args: RmArgs) -> Result<(), Box<dyn std::error::Error>> {
    let mut state = StateFile::load_default()?;
    let mut errors: Vec<String> = Vec::new();

    for query in &args.boxes {
        if let Err(e) = rm_one(&mut state, query, args.force) {
            errors.push(format!("{query}: {e}"));
        }
    }

    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors.join("\n").into())
    }
}

fn rm_one(
    state: &mut StateFile,
    query: &str,
    force: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let record = resolve::resolve(state, query)?;

    if record.status == "running" {
        if !force {
            return Err(format!(
                "Box {} is running. Use --force to remove a running box.",
                record.name
            )
            .into());
        }

        // Force-kill the running box
        if let Some(pid) = record.pid {
            unsafe {
                libc::kill(pid as i32, libc::SIGKILL);
            }
        }
    }

    let box_id = record.id.clone();
    let name = record.name.clone();
    let box_dir = record.box_dir.clone();
    let network_name = record.network_name.clone();
    let volume_names = record.volume_names.clone();
    let anonymous_volumes = record.anonymous_volumes.clone();

    // Clean up volumes and network
    cleanup::cleanup_box_resources(&box_id, &volume_names, network_name.as_deref());

    // Remove anonymous volumes (auto-created from OCI VOLUME directives)
    if !anonymous_volumes.is_empty() {
        if let Ok(vol_store) = a3s_box_runtime::VolumeStore::default_path() {
            for anon_name in &anonymous_volumes {
                if let Err(e) = vol_store.remove(anon_name, true) {
                    tracing::debug!(volume = anon_name, error = %e, "Failed to remove anonymous volume");
                }
            }
        }
    }

    // Remove box directory
    if box_dir.exists() {
        let _ = std::fs::remove_dir_all(&box_dir);
    }

    // Remove from state
    state.remove(&box_id)?;
    println!("{name}");

    Ok(())
}
