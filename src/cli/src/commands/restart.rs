//! `a3s-box restart` command — Restart one or more boxes.
//!
//! Equivalent to `a3s-box stop` followed by `a3s-box start`.

use clap::Args;

use crate::boot;
use crate::lifecycle;
use crate::process;
use crate::resolve;
use crate::state::StateFile;
use crate::status;

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
    let restart_plan = restart_plan(record)?;
    let box_dir = record.box_dir.clone();
    let exec_socket_path = record.exec_socket_path.clone();

    // Phase 1: Stop the box if it is active.
    if restart_plan == RestartPlan::StopThenStart {
        let pid = lifecycle::require_live_pid(record, "restart")
            .map_err(|error| -> Box<dyn std::error::Error> { error.into() })?;
        let stop_signal = record
            .stop_signal
            .as_deref()
            .map(a3s_box_core::vmm::parse_signal_name)
            .unwrap_or(libc::SIGTERM);
        let effective_timeout = record.stop_timeout.unwrap_or(timeout);
        lifecycle::resume_paused_for_termination(record, pid, "restart")
            .map_err(|error| -> Box<dyn std::error::Error> { error.into() })?;
        // Deliver the stop signal inside the guest so the container honours its
        // STOPSIGNAL and runs its own shutdown (then the VM halts cleanly), as
        // `stop` does. Signalling the host shim never reaches the container and
        // kills the VM abruptly; graceful_stop_via_guest falls back to that only
        // when no guest exec server is reachable.
        process::graceful_stop_via_guest(pid, &exec_socket_path, stop_signal, effective_timeout)
            .await;

        // Update state to stopped
        let record = resolve::resolve_mut(state, &box_id)?;
        record.status = "stopped".to_string();
        record.pid = None;
        crate::cleanup::cleanup_external_socket_dir(&box_dir, &exec_socket_path);
        state.save()?;
    }

    // Phase 2: Start the box using shared boot logic
    let record = resolve::resolve(state, &box_id)?;
    let result = boot::boot_from_record(record).await?;

    // Update record to running
    let record = resolve::resolve_mut(state, &box_id)?;
    boot::apply_boot_result(record, result, boot::RestartCountUpdate::Preserve);
    state.save()?;

    println!("{name}");
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RestartPlan {
    StopThenStart,
    StartOnly,
}

fn restart_plan(record: &crate::state::BoxRecord) -> Result<RestartPlan, String> {
    if status::is_active(record) {
        return Ok(RestartPlan::StopThenStart);
    }

    match record.status.as_str() {
        "created" | "stopped" | "dead" => Ok(RestartPlan::StartOnly),
        other => Err(format!("Cannot restart box in state: {other}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_helpers::fixtures::make_record;

    #[test]
    fn test_restart_plan_stops_running_and_paused_first() {
        assert_eq!(
            restart_plan(&make_record("id-1", "running", "running", Some(1))).unwrap(),
            RestartPlan::StopThenStart
        );
        assert_eq!(
            restart_plan(&make_record("id-2", "paused", "paused", Some(1))).unwrap(),
            RestartPlan::StopThenStart
        );
    }

    #[test]
    fn test_restart_plan_starts_inactive_boxes_directly() {
        assert_eq!(
            restart_plan(&make_record("id-1", "created", "created", None)).unwrap(),
            RestartPlan::StartOnly
        );
        assert_eq!(
            restart_plan(&make_record("id-2", "stopped", "stopped", None)).unwrap(),
            RestartPlan::StartOnly
        );
        assert_eq!(
            restart_plan(&make_record("id-3", "dead", "dead", None)).unwrap(),
            RestartPlan::StartOnly
        );
    }
}
