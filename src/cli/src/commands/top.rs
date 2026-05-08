//! `a3s-box top` command — Display running processes in a box.
//!
//! Convenience wrapper that runs `ps` inside the box via the exec channel.

use clap::Args;

#[cfg(not(windows))]
use a3s_box_core::exec::{ExecRequest, DEFAULT_EXEC_TIMEOUT_NS};
#[cfg(not(windows))]
use a3s_box_runtime::ExecClient;

#[cfg(not(windows))]
use crate::resolve;
#[cfg(not(windows))]
use crate::state::StateFile;

/// Default ps arguments when none are specified.
#[cfg(not(windows))]
const DEFAULT_PS_ARGS: &[&str] = &["aux"];

#[derive(Args)]
pub struct TopArgs {
    /// Box name or ID
    pub r#box: String,

    /// Arguments to pass to ps (default: aux)
    #[arg(last = true)]
    pub ps_args: Vec<String>,
}

#[cfg(windows)]
pub async fn execute(_args: TopArgs) -> Result<(), Box<dyn std::error::Error>> {
    Err(crate::platform::unsupported_command(
        "top",
        "guest exec channel support",
    ))
}

#[cfg(not(windows))]
pub async fn execute(args: TopArgs) -> Result<(), Box<dyn std::error::Error>> {
    let state = StateFile::load_default()?;
    let record = resolve::resolve(&state, &args.r#box)?;
    let exec_socket_path = crate::socket_paths::require_runtime_socket(
        record,
        crate::socket_paths::RuntimeSocket::Exec,
    )
    .map_err(|e| -> Box<dyn std::error::Error> { e.into() })?;

    let client = ExecClient::connect(&exec_socket_path).await?;

    let ps_args = if args.ps_args.is_empty() {
        DEFAULT_PS_ARGS.iter().map(|s| s.to_string()).collect()
    } else {
        args.ps_args
    };

    let mut cmd = vec!["ps".to_string()];
    cmd.extend(ps_args);

    let request = ExecRequest {
        cmd,
        timeout_ns: DEFAULT_EXEC_TIMEOUT_NS,
        env: vec![],
        working_dir: None,
        rootfs: None,
        stdin: None,
        stdin_streaming: false,
        user: None,
        streaming: false,
    };

    let output = client.exec_command(&request).await?;

    if !output.stdout.is_empty() {
        let stdout = String::from_utf8_lossy(&output.stdout);
        print!("{stdout}");
    }

    if !output.stderr.is_empty() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        eprint!("{stderr}");
    }

    if output.exit_code != 0 {
        std::process::exit(output.exit_code);
    }

    Ok(())
}

/// Build the ps command from user-provided arguments or defaults.
#[cfg(all(test, not(windows)))]
fn build_ps_command(ps_args: &[String]) -> Vec<String> {
    let mut cmd = vec!["ps".to_string()];
    if ps_args.is_empty() {
        cmd.extend(DEFAULT_PS_ARGS.iter().map(|s| s.to_string()));
    } else {
        cmd.extend_from_slice(ps_args);
    }
    cmd
}

#[cfg(all(test, not(windows)))]
mod tests {
    use super::*;

    #[test]
    fn test_build_ps_command_default() {
        let cmd = build_ps_command(&[]);
        assert_eq!(cmd, vec!["ps", "aux"]);
    }

    #[test]
    fn test_build_ps_command_custom() {
        let args = vec!["-eo".to_string(), "pid,user,%cpu,%mem".to_string()];
        let cmd = build_ps_command(&args);
        assert_eq!(cmd, vec!["ps", "-eo", "pid,user,%cpu,%mem"]);
    }

    #[test]
    fn test_build_ps_command_single_arg() {
        let args = vec!["-ef".to_string()];
        let cmd = build_ps_command(&args);
        assert_eq!(cmd, vec!["ps", "-ef"]);
    }

    #[test]
    fn test_default_ps_args_constant() {
        assert_eq!(DEFAULT_PS_ARGS, &["aux"]);
    }
}
