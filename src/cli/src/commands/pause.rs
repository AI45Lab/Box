//! `a3s-box pause` command — Pause one or more running boxes.
//!
//! Sends SIGSTOP to the box process and updates the status to "paused".

use clap::Args;

use crate::resolve;
use crate::state::StateFile;

#[derive(Args)]
pub struct PauseArgs {
    /// Box name(s) or ID(s)
    #[arg(required = true)]
    pub boxes: Vec<String>,
}

pub async fn execute(args: PauseArgs) -> Result<(), Box<dyn std::error::Error>> {
    let mut state = StateFile::load_default()?;
    let mut errors: Vec<String> = Vec::new();

    for query in &args.boxes {
        if let Err(e) = pause_one(&mut state, query) {
            errors.push(format!("{query}: {e}"));
        }
    }

    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors.join("\n").into())
    }
}

fn pause_one(state: &mut StateFile, query: &str) -> Result<(), Box<dyn std::error::Error>> {
    let record = resolve::resolve(state, query)?;

    if record.status != "running" {
        return Err(format!("Box {} is not running", record.name).into());
    }

    let box_id = record.id.clone();
    let name = record.name.clone();

    if let Some(pid) = record.pid {
        // Safety: sending SIGSTOP to pause the process
        unsafe {
            libc::kill(pid as i32, libc::SIGSTOP);
        }
    }

    // Update status to paused
    let record = resolve::resolve_mut(state, &box_id)?;
    record.status = "paused".to_string();
    state.save()?;

    println!("{name}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_helpers::fixtures::{make_record, setup_state};

    #[test]
    fn test_pause_rejects_non_running() {
        let (_tmp, mut state) =
            setup_state(vec![make_record("id-1", "stopped_box", "stopped", None)]);
        let result = pause_one(&mut state, "stopped_box");
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("not running"));
    }

    #[test]
    fn test_pause_rejects_created() {
        let (_tmp, mut state) =
            setup_state(vec![make_record("id-1", "created_box", "created", None)]);
        let result = pause_one(&mut state, "created_box");
        assert!(result.is_err());
    }

    #[test]
    fn test_pause_rejects_already_paused() {
        let (_tmp, mut state) = setup_state(vec![make_record(
            "id-1",
            "paused_box",
            "paused",
            Some(99999),
        )]);
        let result = pause_one(&mut state, "paused_box");
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("not running"));
    }

    #[test]
    fn test_pause_not_found() {
        let (_tmp, mut state) = setup_state(vec![]);
        let result = pause_one(&mut state, "nonexistent");
        assert!(result.is_err());
    }
}
