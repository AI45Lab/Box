//! `a3s-box attach` command — attach to a running box.
//!
//! Without `-it`, tails the console log (read-only, original behavior).
//! With `-it`, opens an interactive PTY session to a shell inside the box.

use clap::Args;

use crate::resolve;
use crate::state::StateFile;

#[derive(Args)]
pub struct AttachArgs {
    /// Box name or ID
    pub r#box: String,

    /// Keep STDIN open
    #[arg(short = 'i', long = "interactive")]
    pub interactive: bool,

    /// Allocate a pseudo-TTY
    #[arg(short = 't', long = "tty")]
    pub tty: bool,
}

pub async fn execute(args: AttachArgs) -> Result<(), Box<dyn std::error::Error>> {
    let state = StateFile::load_default()?;
    let record = resolve::resolve(&state, &args.r#box)?;
    crate::socket_paths::require_running(record, "attach to")
        .map_err(|e| -> Box<dyn std::error::Error> { e.into() })?;

    // Interactive PTY mode
    if args.tty {
        #[cfg(not(windows))]
        return execute_pty_attach(record).await;
        #[cfg(windows)]
        return Err(crate::platform::unsupported_command(
            "attach -it",
            "interactive PTY support",
        ));
    }

    // Original behavior: tail console log
    let console_log = record.console_log.clone();
    if !console_log.exists() {
        return Err(format!(
            "Console log is missing for running box {} at {}. The box may still be starting or the state may be stale; try `a3s-box logs -f {}` or `a3s-box ps`.",
            record.name,
            console_log.display(),
            record.name
        )
        .into());
    }

    println!("Attached to box {}. Press Ctrl-C to detach.", record.name);

    let console_err = console_log.with_file_name("console.err.log");
    let log_handle = tokio::spawn(async move {
        tokio::join!(
            super::tail_file(&console_log),
            super::tail_file_stream(&console_err, true),
        );
    });

    let _ = tokio::signal::ctrl_c().await;
    println!("\nDetached from box {}.", record.name);

    log_handle.abort();

    Ok(())
}

/// Attach to a running box with an interactive PTY session.
#[cfg(not(windows))]
async fn execute_pty_attach(
    record: &crate::state::BoxRecord,
) -> Result<(), Box<dyn std::error::Error>> {
    use crate::terminal;
    use a3s_box_core::pty::PtyRequest;

    let pty_socket_path = crate::socket_paths::require_runtime_socket(
        record,
        crate::socket_paths::RuntimeSocket::Pty,
    )
    .map_err(|e| -> Box<dyn std::error::Error> { e.into() })?;

    let (cols, rows) = terminal::size().unwrap_or((80, 24));

    let mut client =
        super::exec::connect_pty_with_retry(&pty_socket_path, std::time::Duration::from_secs(10))
            .await?;

    // Attach opens a shell
    let request = PtyRequest {
        cmd: vec!["/bin/sh".to_string()],
        env: vec![],
        working_dir: None,
        rootfs: None,
        user: None,
        cols,
        rows,
    };
    client.send_request(&request).await?;

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
