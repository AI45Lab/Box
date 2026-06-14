//! `a3s-box stop` command — Graceful stop of one or more boxes.

use clap::Args;

use a3s_box_core::vmm::parse_signal_name;

use crate::cleanup;
use crate::lifecycle;
use crate::process;
use crate::resolve;
use crate::state::StateFile;
use crate::status;

#[derive(Args)]
pub struct StopArgs {
    /// Box name(s) or ID(s)
    #[arg(required = true)]
    pub boxes: Vec<String>,

    /// Seconds to wait before force-killing (overrides per-box stop-timeout)
    #[arg(short = 't', long)]
    pub timeout: Option<u64>,
}

pub async fn execute(args: StopArgs) -> Result<(), Box<dyn std::error::Error>> {
    let state = StateFile::load_default()?;
    let mut errors: Vec<String> = Vec::new();

    for query in &args.boxes {
        if let Err(e) = stop_one(&state, query, args.timeout).await {
            errors.push(format!("{query}: {e}"));
        }
    }

    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors.join("\n").into())
    }
}

async fn stop_one(
    state: &StateFile,
    query: &str,
    timeout: Option<u64>,
) -> Result<(), Box<dyn std::error::Error>> {
    let record = resolve::resolve(state, query)?;

    status::require_active(record, "stop")
        .map_err(|error| -> Box<dyn std::error::Error> { error.into() })?;
    let pid = lifecycle::require_live_pid(record, "stop")
        .map_err(|error| -> Box<dyn std::error::Error> { error.into() })?;

    let box_id = record.id.clone();
    let name = record.name.clone();
    let auto_remove = record.auto_remove;
    let record_snapshot = record.clone();
    let previous_exit_code = record.exit_code;

    // Resolve stop signal: CLI --stop-signal > BoxRecord.stop_signal > SIGTERM
    let stop_signal = record
        .stop_signal
        .as_deref()
        .map(parse_signal_name)
        .unwrap_or(15); // SIGTERM = 15

    // Resolve timeout: CLI -t > BoxRecord.stop_timeout > 10s
    let effective_timeout = timeout.or(record.stop_timeout).unwrap_or(10);

    // Exec socket used to deliver the stop signal inside the guest.
    let exec_socket = crate::socket_paths::exec(record);

    // Deliver the stop signal to the container (honouring its STOPSIGNAL), then
    // wait for the VM to exit; SIGKILL the shim after the timeout.
    lifecycle::resume_paused_for_termination(record, pid, "stop")
        .map_err(|error| -> Box<dyn std::error::Error> { error.into() })?;
    let stop_outcome = Some(
        process::graceful_stop_via_guest(pid, &exec_socket, stop_signal, effective_timeout).await,
    );

    if auto_remove {
        cleanup::cleanup_removed_box(&record_snapshot);
        StateFile::remove_record(&box_id)?;
        println!("{name} (auto-removed)");
        return Ok(());
    }

    cleanup::cleanup_stopped_box(&record_snapshot);

    // Apply the status change atomically (load-fresh + mutate + save under the
    // state lock) so it cannot clobber a concurrent run/monitor/compose write
    // with our pre-await snapshot.
    let new_exit_code = stopped_exit_code(previous_exit_code, stop_outcome, stop_signal);
    StateFile::modify(|s| {
        if let Some(record) = s.find_by_id_mut(&box_id) {
            record.status = "stopped".to_string();
            record.pid = None;
            record.stopped_by_user = true;
            record.exit_code = new_exit_code;
            record.health_status = "none".to_string();
            record.health_retries = 0;
        }
        Ok::<(), std::io::Error>(())
    })?;
    println!("{name}");

    Ok(())
}

fn stopped_exit_code(
    previous_exit_code: Option<i32>,
    outcome: Option<process::StopOutcome>,
    stop_signal: i32,
) -> Option<i32> {
    outcome
        .and_then(|outcome| outcome.inferred_exit_code(stop_signal))
        .or(previous_exit_code)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_helpers::fixtures::make_record;

    #[test]
    fn test_stopped_exit_code_uses_graceful_signal_code() {
        assert_eq!(
            stopped_exit_code(None, Some(process::StopOutcome::GracefulExit), 15),
            Some(143)
        );
    }

    #[test]
    fn test_stopped_exit_code_uses_forced_kill_code() {
        assert_eq!(
            stopped_exit_code(Some(7), Some(process::StopOutcome::ForceKilled), 15),
            Some(137)
        );
    }

    #[test]
    fn test_stopped_exit_code_preserves_previous_when_already_exited() {
        assert_eq!(
            stopped_exit_code(Some(7), Some(process::StopOutcome::AlreadyExited), 15),
            Some(7)
        );
    }

    #[test]
    fn test_stop_accepts_paused_status_as_active() {
        let record = make_record("id", "box", "paused", Some(1));

        assert!(status::require_active(&record, "stop").is_ok());
    }
}
