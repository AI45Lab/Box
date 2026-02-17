//! `a3s-box restart` command — Restart one or more boxes.
//!
//! Equivalent to `a3s-box stop` followed by `a3s-box start`.

use clap::Args;

use crate::boot;
use crate::process;
use crate::resolve;
use crate::state::StateFile;

#[derive(Args)]
pub struct RestartArgs {
    /// Box name(s) or ID(s)
    #[arg(required = true)]
    pub boxes: Vec<String>,

    /// Seconds to wait for stop before force-killing
    #[arg(short = 't', long, default_value = "10")]
    pub timeout: u64,
}

pub async fn execute(args: RestartArgs) -> Result<(), Box<dyn std::error::Error>> {
    let mut state = StateFile::load_default()?;
    let mut errors: Vec<String> = Vec::new();

    for query in &args.boxes {
        if let Err(e) = restart_one(&mut state, query, args.timeout).await {
            errors.push(format!("{query}: {e}"));
        }
    }

    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors.join("\n").into())
    }
}

async fn restart_one(
    state: &mut StateFile,
    query: &str,
    timeout: u64,
) -> Result<(), Box<dyn std::error::Error>> {
    let record = resolve::resolve(state, query)?;

    let box_id = record.id.clone();
    let name = record.name.clone();
    let was_running = record.status == "running";
    let pid = record.pid;

    // Phase 1: Stop the box if it's running
    if was_running {
        if let Some(pid) = pid {
            process::graceful_stop(pid, timeout).await;
        }

        // Update state to stopped
        let record = resolve::resolve_mut(state, &box_id)?;
        record.status = "stopped".to_string();
        record.pid = None;
        state.save()?;
    } else {
        // Verify the box is in a startable state
        match record.status.as_str() {
            "created" | "stopped" | "dead" => {}
            other => {
                return Err(format!("Cannot restart box in state: {other}").into());
            }
        }
    }

    // Phase 2: Start the box using shared boot logic
    let record = resolve::resolve(state, &box_id)?;
    let result = boot::boot_from_record(record).await?;

    // Update record to running
    let record = resolve::resolve_mut(state, &box_id)?;
    record.status = "running".to_string();
    record.pid = result.pid;
    record.started_at = Some(chrono::Utc::now());
    state.save()?;

    println!("{name}");
    Ok(())
}
