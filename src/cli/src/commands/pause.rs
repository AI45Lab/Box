//! `a3s-box pause` command — Pause one or more running boxes.
//!
//! Sends SIGSTOP to the box process and updates the status to "paused".

use clap::Args;

use crate::lifecycle;
#[cfg(unix)]
use crate::process;
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
        return Err(format!(
            "Cannot pause box {} because it is {}. Use `a3s-box start {}` to start it or `a3s-box ps -a` to inspect state.",
            record.name, record.status, record.name
        )
        .into());
    }

    let pid = lifecycle::require_live_pid(record, "pause")?;
    let box_id = record.id.clone();
    let name = record.name.clone();

    #[cfg(unix)]
    {
        process::send_signal(pid, libc::SIGSTOP)
            .map_err(|err| format!("Failed to pause box {name} with SIGSTOP: {err}"))?;
    }
    #[cfg(windows)]
    {
        let _ = pid;
        return Err(crate::platform::unsupported_command(
            "pause",
            "host process suspension support",
        ));
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
        assert!(result.unwrap_err().to_string().contains("Cannot pause"));
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
        assert!(result.unwrap_err().to_string().contains("Cannot pause"));
    }

    #[test]
    fn test_pause_rejects_running_without_pid() {
        let (_tmp, mut state) =
            setup_state(vec![make_record("id-1", "running_box", "running", None)]);

        let result = pause_one(&mut state, "running_box");

        assert!(result.is_err());
        let error = result.unwrap_err().to_string();
        assert!(error.contains("no recorded PID"));
        assert!(error.contains("a3s-box ps"));
        assert_eq!(
            state.find_by_id("id-1").unwrap().status,
            "running",
            "stale PID failures must not mark the box paused"
        );
    }

    #[test]
    fn test_pause_not_found() {
        let (_tmp, mut state) = setup_state(vec![]);
        let result = pause_one(&mut state, "nonexistent");
        assert!(result.is_err());
    }
}
