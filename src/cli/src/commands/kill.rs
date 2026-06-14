//! `a3s-box kill` command — Send a signal to one or more active boxes.

use clap::Args;

use crate::cleanup;
use crate::lifecycle;
use crate::process;
use crate::resolve;
use crate::state::StateFile;
use crate::status;

// Signal constants — use libc on Unix, define numerically on Windows.
#[cfg(unix)]
use libc::{SIGCONT, SIGHUP, SIGINT, SIGKILL, SIGQUIT, SIGSTOP, SIGTERM, SIGUSR1, SIGUSR2};

#[cfg(windows)]
mod win_signals {
    pub const SIGKILL: i32 = 9;
    pub const SIGTERM: i32 = 15;
    pub const SIGINT: i32 = 2;
    pub const SIGHUP: i32 = 1;
    pub const SIGQUIT: i32 = 3;
    pub const SIGUSR1: i32 = 10;
    pub const SIGUSR2: i32 = 12;
    pub const SIGSTOP: i32 = 19;
    pub const SIGCONT: i32 = 18;
}
#[cfg(windows)]
use win_signals::*;

#[derive(Args)]
pub struct KillArgs {
    /// Box name(s) or ID(s)
    #[arg(required = true)]
    pub boxes: Vec<String>,

    /// Signal to send to the box process
    #[arg(short = 's', long, default_value = "KILL")]
    pub signal: String,
}

/// Parse a signal name or number into a signal constant.
///
/// Supports common signal names with or without the "SIG" prefix:
/// KILL/SIGKILL, TERM/SIGTERM, INT/SIGINT, HUP/SIGHUP, QUIT/SIGQUIT,
/// USR1/SIGUSR1, USR2/SIGUSR2, STOP/SIGSTOP, CONT/SIGCONT.
/// Also accepts numeric signal values (e.g., "9" for SIGKILL).
fn parse_signal(name: &str) -> Result<i32, String> {
    // Strip optional "SIG" prefix for matching
    let normalized = name
        .to_uppercase()
        .strip_prefix("SIG")
        .map(String::from)
        .unwrap_or_else(|| name.to_uppercase());

    match normalized.as_str() {
        "KILL" => Ok(SIGKILL),
        "TERM" => Ok(SIGTERM),
        "INT" => Ok(SIGINT),
        "HUP" => Ok(SIGHUP),
        "QUIT" => Ok(SIGQUIT),
        "USR1" => Ok(SIGUSR1),
        "USR2" => Ok(SIGUSR2),
        "STOP" => Ok(SIGSTOP),
        "CONT" => Ok(SIGCONT),
        other => {
            // Try parsing as a numeric signal
            other
                .parse::<i32>()
                .map_err(|_| format!("Unknown signal: {}", name))
        }
    }
}

pub async fn execute(args: KillArgs) -> Result<(), Box<dyn std::error::Error>> {
    let signal = parse_signal(&args.signal)?;
    let state = StateFile::load_default()?;
    let mut errors: Vec<String> = Vec::new();

    for query in &args.boxes {
        if let Err(e) = kill_one(&state, query, signal).await {
            errors.push(format!("{query}: {e}"));
        }
    }

    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors.join("\n").into())
    }
}

async fn kill_one(
    state: &StateFile,
    query: &str,
    signal: i32,
) -> Result<(), Box<dyn std::error::Error>> {
    let record = resolve::resolve(state, query)?.clone();

    status::require_active(&record, "send a signal to")
        .map_err(|error| -> Box<dyn std::error::Error> { error.into() })?;
    let pid = lifecycle::require_live_pid(&record, "send a signal to")
        .map_err(|error| -> Box<dyn std::error::Error> { error.into() })?;

    let box_id = record.id.clone();
    let name = record.name.clone();

    if record.status == "paused" && is_stopping_signal(signal) && signal != SIGKILL {
        lifecycle::resume_paused_for_termination(&record, pid, "kill")
            .map_err(|error| -> Box<dyn std::error::Error> { error.into() })?;
    }

    #[cfg(unix)]
    {
        // Deliver the signal to the container's main process inside the guest;
        // signalling the host shim never reaches the container and would kill the
        // VM abruptly. Fall back to a host signal only when no guest exec server
        // is reachable (older box / socket gone).
        let exec_socket = crate::socket_paths::exec(&record);
        if !process::deliver_signal_via_guest(&exec_socket, signal).await {
            process::send_signal(pid, signal).map_err(|err| {
                format!(
                    "Failed to send signal {signal} to box {} (PID {pid}): {err}",
                    record.name
                )
            })?;
        }
    }
    #[cfg(windows)]
    {
        if is_stopping_signal(signal) {
            process::terminate_process(pid);
        } else {
            return Err(crate::platform::unsupported_command(
                "kill",
                "non-terminating host signals",
            ));
        }
    }

    // Only update state to stopped for terminating signals. Each state write
    // goes through the atomic load-fresh + mutate + save primitive so it cannot
    // clobber a concurrent run/monitor/compose write with our pre-await snapshot.
    if is_stopping_signal(signal) {
        if record.auto_remove {
            cleanup::cleanup_removed_box(&record);
            StateFile::remove_record(&box_id)?;
            println!("{name} (auto-removed)");
            return Ok(());
        }

        cleanup::cleanup_stopped_box(&record);

        let exit_code = signaled_exit_code(signal);
        StateFile::modify(|s| {
            if let Some(state_record) = s.find_by_id_mut(&box_id) {
                state_record.status = "stopped".to_string();
                state_record.pid = None;
                state_record.stopped_by_user = true;
                state_record.exit_code = Some(exit_code);
                state_record.health_status = "none".to_string();
                state_record.health_retries = 0;
            }
            Ok::<(), std::io::Error>(())
        })?;
    } else if let Some(new_status) = signal_status_transition(signal) {
        StateFile::modify(|s| {
            if let Some(state_record) = s.find_by_id_mut(&box_id) {
                state_record.status = new_status.to_string();
            }
            Ok::<(), std::io::Error>(())
        })?;
    }

    println!("{name}");
    Ok(())
}

fn is_stopping_signal(signal: i32) -> bool {
    matches!(signal, SIGKILL | SIGTERM | SIGINT | SIGHUP | SIGQUIT)
}

fn signaled_exit_code(signal: i32) -> i32 {
    128 + signal
}

fn signal_status_transition(signal: i32) -> Option<&'static str> {
    match signal {
        SIGSTOP => Some("paused"),
        SIGCONT => Some("running"),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_signal_kill() {
        assert_eq!(parse_signal("KILL").unwrap(), SIGKILL);
        assert_eq!(parse_signal("SIGKILL").unwrap(), SIGKILL);
        assert_eq!(parse_signal("kill").unwrap(), SIGKILL);
        assert_eq!(parse_signal("sigkill").unwrap(), SIGKILL);
    }

    #[test]
    fn test_parse_signal_term() {
        assert_eq!(parse_signal("TERM").unwrap(), SIGTERM);
        assert_eq!(parse_signal("SIGTERM").unwrap(), SIGTERM);
        assert_eq!(parse_signal("term").unwrap(), SIGTERM);
    }

    #[test]
    fn test_parse_signal_int() {
        assert_eq!(parse_signal("INT").unwrap(), SIGINT);
        assert_eq!(parse_signal("SIGINT").unwrap(), SIGINT);
    }

    #[test]
    fn test_parse_signal_hup() {
        assert_eq!(parse_signal("HUP").unwrap(), SIGHUP);
        assert_eq!(parse_signal("SIGHUP").unwrap(), SIGHUP);
    }

    #[test]
    fn test_parse_signal_quit() {
        assert_eq!(parse_signal("QUIT").unwrap(), SIGQUIT);
        assert_eq!(parse_signal("SIGQUIT").unwrap(), SIGQUIT);
    }

    #[test]
    fn test_parse_signal_usr() {
        assert_eq!(parse_signal("USR1").unwrap(), SIGUSR1);
        assert_eq!(parse_signal("SIGUSR1").unwrap(), SIGUSR1);
        assert_eq!(parse_signal("USR2").unwrap(), SIGUSR2);
        assert_eq!(parse_signal("SIGUSR2").unwrap(), SIGUSR2);
    }

    #[test]
    fn test_parse_signal_stop_cont() {
        assert_eq!(parse_signal("STOP").unwrap(), SIGSTOP);
        assert_eq!(parse_signal("CONT").unwrap(), SIGCONT);
    }

    #[test]
    fn test_parse_signal_numeric() {
        assert_eq!(parse_signal("9").unwrap(), 9);
        assert_eq!(parse_signal("15").unwrap(), 15);
    }

    #[test]
    fn test_parse_signal_unknown() {
        assert!(parse_signal("INVALID").is_err());
        assert!(parse_signal("SIGFOO").is_err());
        assert!(parse_signal("").is_err());
    }

    #[test]
    fn test_is_stopping_signal() {
        assert!(is_stopping_signal(SIGKILL));
        assert!(is_stopping_signal(SIGTERM));
        assert!(is_stopping_signal(SIGINT));
        assert!(is_stopping_signal(SIGHUP));
        assert!(is_stopping_signal(SIGQUIT));
        assert!(!is_stopping_signal(SIGSTOP));
        assert!(!is_stopping_signal(SIGCONT));
        assert!(!is_stopping_signal(SIGUSR1));
    }

    #[test]
    fn test_signaled_exit_code() {
        assert_eq!(signaled_exit_code(SIGKILL), 137);
        assert_eq!(signaled_exit_code(SIGTERM), 143);
    }

    #[test]
    fn test_signal_status_transition() {
        assert_eq!(signal_status_transition(SIGSTOP), Some("paused"));
        assert_eq!(signal_status_transition(SIGCONT), Some("running"));
        assert_eq!(signal_status_transition(SIGUSR1), None);
    }

    #[test]
    fn test_kill_accepts_paused_status_as_active() {
        let record = crate::test_helpers::fixtures::make_record("id", "box", "paused", Some(1));

        assert!(status::require_active(&record, "send a signal to").is_ok());
    }
}
