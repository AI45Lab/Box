//! `a3s-box start` command — Start one or more created/stopped boxes.

use clap::Args;

use crate::boot;
use crate::resolve;
use crate::state::StateFile;

#[derive(Args)]
pub struct StartArgs {
    /// Box name(s) or ID(s)
    #[arg(required = true)]
    pub boxes: Vec<String>,
}

pub async fn execute(args: StartArgs) -> Result<(), Box<dyn std::error::Error>> {
    let state = StateFile::load_default()?;
    let mut errors: Vec<String> = Vec::new();

    for query in &args.boxes {
        if let Err(e) = start_one(&state, query).await {
            errors.push(format!("{query}: {e}"));
        }
    }

    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors.join("\n").into())
    }
}

async fn start_one(state: &StateFile, query: &str) -> Result<(), Box<dyn std::error::Error>> {
    let record = resolve::resolve(state, query)?;

    match record.status.as_str() {
        "created" | "stopped" | "dead" => {}
        "running" => return Err(format!("Box {} is already running", record.name).into()),
        other => return Err(format!("Cannot start box in state: {other}").into()),
    }

    let box_id = record.id.clone();
    let name = record.name.clone();

    println!("Starting box {name}...");
    let result = boot::boot_from_record(record).await?;

    // Persist the boot result atomically (load-fresh + mutate + save under the
    // state lock) so it cannot clobber a concurrent writer with our pre-boot
    // snapshot.
    StateFile::modify(move |s| {
        if let Some(record) = s.find_by_id_mut(&box_id) {
            boot::apply_boot_result(record, result, boot::RestartCountUpdate::Reset);
        }
        Ok::<(), std::io::Error>(())
    })?;

    println!("{name}");
    Ok(())
}
