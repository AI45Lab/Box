//! `a3s-box stop` command — Graceful stop of one or more boxes.

use clap::Args;

use crate::cleanup;
use crate::process;
use crate::resolve;
use crate::state::StateFile;

#[derive(Args)]
pub struct StopArgs {
    /// Box name(s) or ID(s)
    #[arg(required = true)]
    pub boxes: Vec<String>,

    /// Seconds to wait before force-killing
    #[arg(short = 't', long, default_value = "10")]
    pub timeout: u64,
}

pub async fn execute(args: StopArgs) -> Result<(), Box<dyn std::error::Error>> {
    let mut state = StateFile::load_default()?;
    let mut errors: Vec<String> = Vec::new();

    for query in &args.boxes {
        if let Err(e) = stop_one(&mut state, query, args.timeout).await {
            errors.push(format!("{query}: {e}"));
        }
    }

    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors.join("\n").into())
    }
}

async fn stop_one(
    state: &mut StateFile,
    query: &str,
    timeout: u64,
) -> Result<(), Box<dyn std::error::Error>> {
    let record = resolve::resolve(state, query)?;

    if record.status != "running" {
        return Err(format!(
            "Box {} is not running (status: {})",
            record.name, record.status
        )
        .into());
    }

    let box_id = record.id.clone();
    let name = record.name.clone();
    let pid = record.pid;
    let auto_remove = record.auto_remove;
    let box_dir = record.box_dir.clone();
    let network_name = record.network_name.clone();
    let volume_names = record.volume_names.clone();

    // Send SIGTERM, then SIGKILL after timeout
    if let Some(pid) = pid {
        process::graceful_stop(pid, timeout).await;
    }

    // Clean up volumes and network
    cleanup::cleanup_box_resources(&box_id, &volume_names, network_name.as_deref());

    // Update state
    let record = resolve::resolve_mut(state, &box_id)?;
    record.status = "stopped".to_string();
    record.pid = None;
    record.stopped_by_user = true;

    if auto_remove {
        let _ = std::fs::remove_dir_all(&box_dir);
        state.remove(&box_id)?;
        println!("{name} (auto-removed)");
    } else {
        state.save()?;
        println!("{name}");
    }

    Ok(())
}
