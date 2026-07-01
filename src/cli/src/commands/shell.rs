//! `a3s-box shell` command — Open an interactive shell in a running box.
//!
//! Convenience wrapper around `exec -t <box> /bin/sh`.
//! Equivalent to: `a3s-box exec -t <box> -- /bin/sh`

use clap::Args;

#[cfg(not(windows))]
use super::common;
#[cfg(not(windows))]
use crate::resolve;
#[cfg(not(windows))]
use crate::state::StateFile;

#[derive(Args)]
pub struct ShellArgs {
    /// Box name or ID
    pub r#box: String,

    /// Shell to launch (default: /bin/sh)
    #[arg(long, default_value = "/bin/sh")]
    pub shell: String,

    /// Run as a specific user (supported: root, UID, UID:GID)
    #[arg(short = 'u', long)]
    pub user: Option<String>,

    /// Working directory inside the box
    #[arg(short = 'w', long)]
    pub workdir: Option<String>,
}

#[cfg(windows)]
pub async fn execute(_args: ShellArgs) -> Result<(), Box<dyn std::error::Error>> {
    Err(crate::platform::unsupported_command(
        "shell",
        "interactive PTY support",
    ))
}

#[cfg(not(windows))]
pub async fn execute(args: ShellArgs) -> Result<(), Box<dyn std::error::Error>> {
    use crate::terminal;
    use a3s_box_core::pty::PtyRequest;

    let user = common::normalize_user_option(args.user.as_deref())
        .map_err(|e| -> Box<dyn std::error::Error> { e.into() })?;
    common::validate_workdir_option(args.workdir.as_deref())
        .map_err(|e| -> Box<dyn std::error::Error> { e.into() })?;

    let state = StateFile::load_default()?;
    let record = resolve::resolve(&state, &args.r#box)?;
    let pty_socket_path = crate::socket_paths::require_runtime_socket(
        record,
        crate::socket_paths::RuntimeSocket::Pty,
    )
    .map_err(|e| -> Box<dyn std::error::Error> { e.into() })?;

    let (cols, rows) = terminal::size().unwrap_or((80, 24));
    let mut client =
        super::exec::connect_pty_with_retry(&pty_socket_path, std::time::Duration::from_secs(10))
            .await?;
    client
        .send_request(&PtyRequest {
            cmd: vec![args.shell],
            env: vec![],
            working_dir: args.workdir,
            rootfs: None,
            user,
            cols,
            rows,
        })
        .await?;

    let (read_half, write_half) = client.into_split();
    let exit_code = {
        let _raw_mode = terminal::raw_mode()?;
        super::exec::run_pty_session(read_half, write_half).await
    };

    if exit_code != 0 {
        std::process::exit(exit_code);
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_shell_args_defaults() {
        // Verify the default shell is /bin/sh
        let args = ShellArgs {
            r#box: "mybox".to_string(),
            shell: "/bin/sh".to_string(),
            user: None,
            workdir: None,
        };
        assert_eq!(args.shell, "/bin/sh");
        assert!(args.user.is_none());
        assert!(args.workdir.is_none());
    }

    #[test]
    fn test_shell_args_custom() {
        let args = ShellArgs {
            r#box: "mybox".to_string(),
            shell: "/bin/bash".to_string(),
            user: Some("root".to_string()),
            workdir: Some("/workspace".to_string()),
        };
        assert_eq!(args.shell, "/bin/bash");
        assert_eq!(args.user.as_deref(), Some("root"));
        assert_eq!(args.workdir.as_deref(), Some("/workspace"));
    }
}
