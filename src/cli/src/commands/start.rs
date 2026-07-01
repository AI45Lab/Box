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

    validate_start_status(&record.name, &record.status)
        .map_err(|error| -> Box<dyn std::error::Error> { error.into() })?;

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

fn validate_start_status(name: &str, status: &str) -> Result<(), String> {
    match status {
        "created" | "stopped" | "dead" => Ok(()),
        "running" => Err(format!("Box {name} is already running")),
        other => Err(format!("Cannot start box in state: {other}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_start_status_accepts_startable_states() {
        assert!(validate_start_status("web", "created").is_ok());
        assert!(validate_start_status("web", "stopped").is_ok());
        assert!(validate_start_status("web", "dead").is_ok());
    }

    #[test]
    fn validate_start_status_rejects_running_box_by_name() {
        assert_eq!(
            validate_start_status("web", "running").unwrap_err(),
            "Box web is already running"
        );
    }

    #[test]
    fn validate_start_status_rejects_other_states() {
        assert_eq!(
            validate_start_status("web", "paused").unwrap_err(),
            "Cannot start box in state: paused"
        );
    }
}
