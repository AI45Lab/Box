//! `a3s-box unpause` command — Unpause one or more paused boxes.
//!
//! Sends SIGCONT to the box process and updates the status back to "running".

use clap::Args;

use crate::lifecycle;
#[cfg(unix)]
use crate::process;
use crate::resolve;
use crate::state::StateFile;

#[derive(Args)]
pub struct UnpauseArgs {
    /// Box name(s) or ID(s)
    #[arg(required = true)]
    pub boxes: Vec<String>,
}

pub async fn execute(args: UnpauseArgs) -> Result<(), Box<dyn std::error::Error>> {
    let state = StateFile::load_default()?;
    let mut errors: Vec<String> = Vec::new();

    for query in &args.boxes {
        if let Err(e) = unpause_one(&state, query) {
            errors.push(format!("{query}: {e}"));
        }
    }

    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors.join("\n").into())
    }
}

fn unpause_one(state: &StateFile, query: &str) -> Result<(), Box<dyn std::error::Error>> {
    let record = resolve::resolve(state, query)?;

    if record.status != "paused" {
        return Err(format!(
            "Cannot unpause box {} because it is {}. Use `a3s-box ps -a` to inspect state.",
            record.name, record.status
        )
        .into());
    }

    let pid = lifecycle::require_live_pid(record, "unpause")?;

    #[cfg(windows)]
    {
        let _ = pid;
        return Err(crate::platform::unsupported_command(
            "unpause",
            "host process resume support",
        ));
    }

    #[cfg(unix)]
    {
        let box_id = record.id.clone();
        let name = record.name.clone();

        process::send_signal(pid, libc::SIGCONT)
            .map_err(|err| format!("Failed to unpause box {name} with SIGCONT: {err}"))?;

        // Persist the status flip atomically under the state lock so it cannot
        // clobber a concurrent writer with our pre-signal snapshot.
        StateFile::modify(|s| {
            if let Some(record) = s.find_by_id_mut(&box_id) {
                record.status = "running".to_string();
            }
            Ok::<(), std::io::Error>(())
        })?;

        println!("{name}");
        Ok(())
    }

    #[cfg(not(any(unix, windows)))]
    {
        let _ = pid;
        Err("'unpause' requires host process resume support".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_helpers::fixtures::{make_record, setup_state};

    #[test]
    fn test_unpause_rejects_running() {
        let (_tmp, state) = setup_state(vec![make_record(
            "id-1",
            "running_box",
            "running",
            Some(99999),
        )]);
        let result = unpause_one(&state, "running_box");
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("Cannot unpause"));
    }

    #[test]
    fn test_unpause_rejects_stopped() {
        let (_tmp, state) = setup_state(vec![make_record("id-1", "stopped_box", "stopped", None)]);
        let result = unpause_one(&state, "stopped_box");
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("Cannot unpause"));
    }

    #[test]
    fn test_unpause_rejects_paused_without_pid() {
        let (_tmp, state) = setup_state(vec![make_record("id-1", "paused_box", "paused", None)]);

        let result = unpause_one(&state, "paused_box");

        assert!(result.is_err());
        let error = result.unwrap_err().to_string();
        assert!(error.contains("no recorded PID"));
        assert!(error.contains("a3s-box ps"));
        assert_eq!(
            state.find_by_id("id-1").unwrap().status,
            "paused",
            "stale PID failures must not mark the box running"
        );
    }

    #[test]
    fn test_unpause_not_found() {
        let (_tmp, state) = setup_state(vec![]);
        let result = unpause_one(&state, "nonexistent");
        assert!(result.is_err());
    }
}
