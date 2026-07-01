//! `a3s-box top` command — Display running processes in a box.
//!
//! Convenience wrapper that runs `ps` inside the box via the exec channel.

use clap::{Args, ValueEnum};
use serde::Serialize;

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
#[cfg(not(windows))]
const JSON_PS_ARGS: &[&str] = &["-eo", "pid,ppid,pcpu,pmem,etime,args"];

#[derive(Args)]
pub struct TopArgs {
    /// Box name or ID
    pub r#box: String,

    /// Output format
    #[arg(long, value_enum, default_value_t = TopFormat::Table)]
    pub format: TopFormat,

    /// Arguments to pass to ps (default: aux)
    #[arg(last = true)]
    pub ps_args: Vec<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum TopFormat {
    Table,
    Json,
}

#[cfg(not(windows))]
#[derive(Debug, Clone, PartialEq, Serialize)]
struct TopProcess {
    pid: String,
    ppid: Option<String>,
    cpu_percent: Option<f32>,
    memory_percent: Option<f32>,
    elapsed: Option<String>,
    command: String,
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

    let ps_args = if args.ps_args.is_empty() && args.format == TopFormat::Json {
        JSON_PS_ARGS.iter().map(|s| s.to_string()).collect()
    } else if args.ps_args.is_empty() {
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

    if !output.stderr.is_empty() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        eprint!("{stderr}");
    }

    if output.exit_code != 0 {
        std::process::exit(output.exit_code);
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    match args.format {
        TopFormat::Table => print!("{stdout}"),
        TopFormat::Json => print_top_json(&stdout)?,
    }

    Ok(())
}

#[cfg(not(windows))]
fn print_top_json(stdout: &str) -> Result<(), serde_json::Error> {
    let rows = parse_ps_table(stdout);
    println!("{}", serde_json::to_string(&rows)?);
    Ok(())
}

#[cfg(not(windows))]
fn parse_ps_table(text: &str) -> Vec<TopProcess> {
    let mut lines = text.lines().filter(|line| !line.trim().is_empty());
    let Some(header) = lines.next() else {
        return Vec::new();
    };
    let headers = header
        .split_whitespace()
        .map(|part| part.trim().to_ascii_uppercase())
        .collect::<Vec<_>>();
    let pid_idx = headers.iter().position(|part| part == "PID");
    let ppid_idx = headers.iter().position(|part| part == "PPID");
    let cpu_idx = headers
        .iter()
        .position(|part| matches!(part.as_str(), "%CPU" | "PCPU" | "CPU%"));
    let mem_idx = headers
        .iter()
        .position(|part| matches!(part.as_str(), "%MEM" | "PMEM" | "MEM%"));
    let elapsed_idx = headers
        .iter()
        .position(|part| matches!(part.as_str(), "ELAPSED" | "ETIME" | "TIME"));
    let command_idx = headers
        .iter()
        .position(|part| matches!(part.as_str(), "COMMAND" | "CMD" | "ARGS"));

    lines
        .filter_map(|line| {
            parse_ps_line(
                line,
                pid_idx?,
                ppid_idx,
                cpu_idx,
                mem_idx,
                elapsed_idx,
                command_idx,
            )
        })
        .collect()
}

#[cfg(not(windows))]
fn parse_ps_line(
    line: &str,
    pid_idx: usize,
    ppid_idx: Option<usize>,
    cpu_idx: Option<usize>,
    mem_idx: Option<usize>,
    elapsed_idx: Option<usize>,
    command_idx: Option<usize>,
) -> Option<TopProcess> {
    let parts = line.split_whitespace().collect::<Vec<_>>();
    let pid = parts.get(pid_idx)?.to_string();
    Some(TopProcess {
        pid,
        ppid: ppid_idx
            .and_then(|idx| parts.get(idx))
            .map(|part| (*part).to_string()),
        cpu_percent: cpu_idx
            .and_then(|idx| parts.get(idx))
            .and_then(|value| parse_percent(value)),
        memory_percent: mem_idx
            .and_then(|idx| parts.get(idx))
            .and_then(|value| parse_percent(value)),
        elapsed: elapsed_idx
            .and_then(|idx| parts.get(idx))
            .map(|part| (*part).to_string()),
        command: command_idx
            .and_then(|idx| (idx < parts.len()).then(|| parts[idx..].join(" ")))
            .unwrap_or_else(|| "-".to_string()),
    })
}

#[cfg(not(windows))]
fn parse_percent(value: &str) -> Option<f32> {
    value.trim_end_matches('%').parse().ok()
}

/// Build the ps command from user-provided arguments or defaults.
#[cfg(all(test, not(windows)))]
fn build_ps_command(format: TopFormat, ps_args: &[String]) -> Vec<String> {
    let mut cmd = vec!["ps".to_string()];
    if ps_args.is_empty() && format == TopFormat::Json {
        cmd.extend(JSON_PS_ARGS.iter().map(|s| s.to_string()));
    } else if ps_args.is_empty() {
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
        let cmd = build_ps_command(TopFormat::Table, &[]);
        assert_eq!(cmd, vec!["ps", "aux"]);
    }

    #[test]
    fn test_build_ps_command_json_default() {
        let cmd = build_ps_command(TopFormat::Json, &[]);
        assert_eq!(cmd, vec!["ps", "-eo", "pid,ppid,pcpu,pmem,etime,args"]);
    }

    #[test]
    fn test_build_ps_command_custom() {
        let args = vec!["-eo".to_string(), "pid,user,%cpu,%mem".to_string()];
        let cmd = build_ps_command(TopFormat::Table, &args);
        assert_eq!(cmd, vec!["ps", "-eo", "pid,user,%cpu,%mem"]);
    }

    #[test]
    fn test_build_ps_command_single_arg() {
        let args = vec!["-ef".to_string()];
        let cmd = build_ps_command(TopFormat::Table, &args);
        assert_eq!(cmd, vec!["ps", "-ef"]);
    }

    #[test]
    fn test_default_ps_args_constant() {
        assert_eq!(DEFAULT_PS_ARGS, &["aux"]);
    }

    #[test]
    fn parses_ps_table_for_json_output() {
        let rows = parse_ps_table(
            "PID PPID %CPU %MEM ELAPSED COMMAND\n1 0 0.0 0.1 02:00 /sbin/init\n42 1 1.5 0.3 00:01 node server.js\n",
        );

        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].pid, "1");
        assert_eq!(rows[0].ppid.as_deref(), Some("0"));
        assert_eq!(rows[0].cpu_percent, Some(0.0));
        assert_eq!(rows[0].memory_percent, Some(0.1));
        assert_eq!(rows[0].elapsed.as_deref(), Some("02:00"));
        assert_eq!(rows[1].command, "node server.js");
    }

    #[test]
    fn parses_aux_style_table_for_json_output() {
        let rows =
            parse_ps_table("USER PID %CPU %MEM VSZ RSS TTY STAT START TIME COMMAND\nroot 7 2.5 0.4 1 2 ? S 10:00 00:01 worker --serve\n");

        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].pid, "7");
        assert_eq!(rows[0].ppid, None);
        assert_eq!(rows[0].cpu_percent, Some(2.5));
        assert_eq!(rows[0].memory_percent, Some(0.4));
        assert_eq!(rows[0].elapsed.as_deref(), Some("00:01"));
        assert_eq!(rows[0].command, "worker --serve");
    }
}
