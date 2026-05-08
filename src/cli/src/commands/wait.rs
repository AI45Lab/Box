//! `a3s-box wait` command — Block until one or more boxes stop, then print exit codes.

use clap::Args;

use crate::process;
use crate::resolve;
use crate::state::{BoxRecord, StateFile};

#[derive(Args)]
pub struct WaitArgs {
    /// Box name(s) or ID(s)
    #[arg(required = true)]
    pub boxes: Vec<String>,
}

pub async fn execute(args: WaitArgs) -> Result<(), Box<dyn std::error::Error>> {
    for query in &args.boxes {
        wait_one(query).await?;
    }
    Ok(())
}

async fn wait_one(query: &str) -> Result<(), Box<dyn std::error::Error>> {
    loop {
        let state = StateFile::load_default()?;
        let record = resolve::resolve(&state, query)?;

        match wait_poll_action(record) {
            WaitPollAction::Finish(exit_code) => {
                println!("{exit_code}");
                return Ok(());
            }
            WaitPollAction::Sleep => {
                tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WaitPollAction {
    Finish(i32),
    Sleep,
}

fn wait_poll_action(record: &BoxRecord) -> WaitPollAction {
    match record.status.as_str() {
        "running" | "paused" => match record.pid {
            Some(pid) if process::is_process_alive(pid) => WaitPollAction::Sleep,
            _ => WaitPollAction::Finish(wait_exit_code(record)),
        },
        "created" => WaitPollAction::Sleep,
        "stopped" | "dead" => WaitPollAction::Finish(wait_exit_code(record)),
        _ => WaitPollAction::Finish(0),
    }
}

fn wait_exit_code(record: &BoxRecord) -> i32 {
    record.exit_code.unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_wait_exit_code_defaults_to_success() {
        let record = crate::test_helpers::fixtures::make_record("id", "box", "stopped", None);
        assert_eq!(wait_exit_code(&record), 0);
    }

    #[test]
    fn test_wait_exit_code_uses_recorded_code() {
        let mut record = crate::test_helpers::fixtures::make_record("id", "box", "stopped", None);
        record.exit_code = Some(42);
        assert_eq!(wait_exit_code(&record), 42);
    }

    #[test]
    fn test_wait_poll_action_keeps_waiting_for_paused_live_process() {
        let record = crate::test_helpers::fixtures::make_record(
            "id",
            "box",
            "paused",
            Some(std::process::id()),
        );

        assert_eq!(wait_poll_action(&record), WaitPollAction::Sleep);
    }

    #[test]
    fn test_wait_poll_action_finishes_for_paused_without_pid() {
        let record = crate::test_helpers::fixtures::make_record("id", "box", "paused", None);

        assert_eq!(wait_poll_action(&record), WaitPollAction::Finish(0));
    }
}
