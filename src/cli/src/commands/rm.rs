//! `a3s-box rm` command — Remove one or more boxes.

use clap::Args;

use crate::cleanup;
use crate::resolve;
use crate::state::StateFile;
use crate::status;

#[derive(Args)]
pub struct RmArgs {
    /// Box name(s) or ID(s)
    #[arg(required = true)]
    pub boxes: Vec<String>,

    /// Force removal of active boxes (terminates them first)
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
    let record = resolve::resolve(state, query)?.clone();

    if status::is_active(&record) {
        if !force {
            return Err(format!(
                "Box {} is {}. Use --force to remove an active box.",
                record.name, record.status
            )
            .into());
        }

        // Force-kill the active box. A missing PID is treated as stale state;
        // --force still removes metadata and resources below.
        if let Some(pid) = record.pid {
            crate::process::terminate_process(pid);
        }
    }

    let box_id = record.id.clone();
    let name = record.name.clone();
    cleanup::cleanup_removed_box(&record);

    // Remove from state atomically under the lock (avoids clobbering concurrent
    // monitor/CLI writers that rewrite the whole record vector), then keep this
    // in-memory handle consistent without a second persisting write.
    StateFile::remove_record(&box_id)?;
    state.forget(&box_id);
    crate::audit::record(
        a3s_box_core::audit::AuditAction::BoxDestroy,
        a3s_box_core::audit::AuditOutcome::Success,
        &box_id,
        &format!("removed box {name}"),
    );
    println!("{name}");

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_helpers::fixtures::{make_record, setup_state};

    #[test]
    fn test_rm_rejects_paused_without_force() {
        let (_tmp, mut state) =
            setup_state(vec![make_record("id-1", "paused_box", "paused", None)]);

        let result = rm_one(&mut state, "paused_box", false);

        assert!(result.is_err());
        let error = result.unwrap_err().to_string();
        assert!(error.contains("paused"));
        assert!(error.contains("--force"));
        assert!(state.find_by_id("id-1").is_some());
    }

    #[test]
    fn test_rm_force_removes_paused_stale_record() {
        let (_tmp, mut state) =
            setup_state(vec![make_record("id-1", "paused_box", "paused", None)]);

        rm_one(&mut state, "paused_box", true).unwrap();

        assert!(state.find_by_id("id-1").is_none());
    }
}
