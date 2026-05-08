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
    let mut state = StateFile::load_default()?;
    let mut errors: Vec<String> = Vec::new();

    for query in &args.boxes {
        if let Err(e) = start_one(&mut state, query).await {
            errors.push(format!("{query}: {e}"));
        }
    }

    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors.join("\n").into())
    }
}

async fn start_one(state: &mut StateFile, query: &str) -> Result<(), Box<dyn std::error::Error>> {
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

    // Update record
    let record = resolve::resolve_mut(state, &box_id)?;
    boot::apply_boot_result(record, result, boot::RestartCountUpdate::Reset);
    state.save()?;

    println!("{name}");
    Ok(())
}
