//! `a3s-box kill` command — Send a signal to one or more running boxes.

use clap::Args;

use crate::cleanup;
use crate::resolve;
use crate::state::StateFile;

#[derive(Args)]
pub struct KillArgs {
    /// Box name(s) or ID(s)
    #[arg(required = true)]
    pub boxes: Vec<String>,

    /// Signal to send to the box process
    #[arg(short = 's', long, default_value = "KILL")]
    pub signal: String,
}

/// Parse a signal name or number into a libc signal constant.
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
        "KILL" => Ok(libc::SIGKILL),
        "TERM" => Ok(libc::SIGTERM),
        "INT" => Ok(libc::SIGINT),
        "HUP" => Ok(libc::SIGHUP),
        "QUIT" => Ok(libc::SIGQUIT),
        "USR1" => Ok(libc::SIGUSR1),
        "USR2" => Ok(libc::SIGUSR2),
        "STOP" => Ok(libc::SIGSTOP),
        "CONT" => Ok(libc::SIGCONT),
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
    let mut state = StateFile::load_default()?;
    let mut errors: Vec<String> = Vec::new();

    for query in &args.boxes {
        if let Err(e) = kill_one(&mut state, query, signal) {
            errors.push(format!("{query}: {e}"));
        }
    }

    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors.join("\n").into())
    }
}

fn kill_one(
    state: &mut StateFile,
    query: &str,
    signal: i32,
) -> Result<(), Box<dyn std::error::Error>> {
    let record = resolve::resolve(state, query)?;

    if record.status != "running" {
        return Err(format!("Box {} is not running", record.name).into());
    }

    let box_id = record.id.clone();
    let name = record.name.clone();
    let network_name = record.network_name.clone();
    let volume_names = record.volume_names.clone();

    if let Some(pid) = record.pid {
        unsafe {
            libc::kill(pid as i32, signal);
        }
    }

    // Only update state to stopped for terminating signals
    if signal == libc::SIGKILL || signal == libc::SIGTERM {
        cleanup::cleanup_box_resources(&box_id, &volume_names, network_name.as_deref());

        let record = resolve::resolve_mut(state, &box_id)?;
        record.status = "stopped".to_string();
        record.pid = None;
        record.stopped_by_user = true;
        state.save()?;
    }

    println!("{name}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_signal_kill() {
        assert_eq!(parse_signal("KILL").unwrap(), libc::SIGKILL);
        assert_eq!(parse_signal("SIGKILL").unwrap(), libc::SIGKILL);
        assert_eq!(parse_signal("kill").unwrap(), libc::SIGKILL);
        assert_eq!(parse_signal("sigkill").unwrap(), libc::SIGKILL);
    }

    #[test]
    fn test_parse_signal_term() {
        assert_eq!(parse_signal("TERM").unwrap(), libc::SIGTERM);
        assert_eq!(parse_signal("SIGTERM").unwrap(), libc::SIGTERM);
        assert_eq!(parse_signal("term").unwrap(), libc::SIGTERM);
    }

    #[test]
    fn test_parse_signal_int() {
        assert_eq!(parse_signal("INT").unwrap(), libc::SIGINT);
        assert_eq!(parse_signal("SIGINT").unwrap(), libc::SIGINT);
    }

    #[test]
    fn test_parse_signal_hup() {
        assert_eq!(parse_signal("HUP").unwrap(), libc::SIGHUP);
        assert_eq!(parse_signal("SIGHUP").unwrap(), libc::SIGHUP);
    }

    #[test]
    fn test_parse_signal_quit() {
        assert_eq!(parse_signal("QUIT").unwrap(), libc::SIGQUIT);
        assert_eq!(parse_signal("SIGQUIT").unwrap(), libc::SIGQUIT);
    }

    #[test]
    fn test_parse_signal_usr() {
        assert_eq!(parse_signal("USR1").unwrap(), libc::SIGUSR1);
        assert_eq!(parse_signal("SIGUSR1").unwrap(), libc::SIGUSR1);
        assert_eq!(parse_signal("USR2").unwrap(), libc::SIGUSR2);
        assert_eq!(parse_signal("SIGUSR2").unwrap(), libc::SIGUSR2);
    }

    #[test]
    fn test_parse_signal_stop_cont() {
        assert_eq!(parse_signal("STOP").unwrap(), libc::SIGSTOP);
        assert_eq!(parse_signal("CONT").unwrap(), libc::SIGCONT);
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
}
