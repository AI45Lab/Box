//! `a3s-box logs` command — View box console logs.

use std::io::{BufRead, BufReader, Seek, SeekFrom};
use std::path::PathBuf;

use chrono::{DateTime, Utc};
use clap::Args;

use a3s_box_core::log::{LogDriver, LogEntry};

use crate::resolve;
use crate::state::{BoxRecord, StateFile};

#[derive(Args)]
pub struct LogsArgs {
    /// Box name or ID
    pub r#box: String,

    /// Follow log output
    #[arg(short, long)]
    pub follow: bool,

    /// Number of lines to show from the end
    #[arg(long)]
    pub tail: Option<usize>,

    /// Show logs since timestamp (e.g., "2024-01-01T00:00:00Z", "1h", "30m")
    #[arg(long)]
    pub since: Option<String>,

    /// Show logs until timestamp (e.g., "2024-01-01T12:00:00Z", "1h", "30m")
    #[arg(long)]
    pub until: Option<String>,

    /// Show timestamps on each line
    #[arg(short = 't', long)]
    pub timestamps: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct LogSource {
    path: PathBuf,
    structured: bool,
}

pub async fn execute(args: LogsArgs) -> Result<(), Box<dyn std::error::Error>> {
    let state = StateFile::load_default()?;
    let record = resolve::resolve(&state, &args.r#box)?;

    // If logging is disabled, tell the user
    if record.log_config.driver == LogDriver::None {
        return Err(format!(
            "Logging is disabled for box {} (log-driver=none)",
            record.name
        )
        .into());
    }

    let since = args.since.as_deref().map(parse_time_filter).transpose()?;
    let until = args.until.as_deref().map(parse_time_filter).transpose()?;

    let Some(log_source) = resolve_log_source(record) else {
        if args.follow && record.status == "running" {
            match wait_for_log_source(&record.id).await? {
                Some(source) => return stream_logs(source, args, since, until).await,
                None => return Ok(()),
            }
        }
        return Ok(());
    };

    stream_logs(log_source, args, since, until).await
}

async fn stream_logs(
    log_source: LogSource,
    args: LogsArgs,
    since: Option<DateTime<Utc>>,
    until: Option<DateTime<Utc>>,
) -> Result<(), Box<dyn std::error::Error>> {
    let use_json = log_source.structured;
    let log_path = log_source.path;
    let has_time_filter = since.is_some() || until.is_some();

    if let Some(tail_n) = args.tail {
        let file = std::fs::File::open(&log_path)?;
        let reader = BufReader::new(file);
        let lines: Vec<String> = reader.lines().collect::<Result<_, _>>()?;
        let start = lines.len().saturating_sub(tail_n);
        for line in &lines[start..] {
            if use_json {
                print_json_line(
                    line,
                    args.timestamps,
                    has_time_filter,
                    since.as_ref(),
                    until.as_ref(),
                );
            } else {
                if has_time_filter && !line_in_range(line, since.as_ref(), until.as_ref()) {
                    continue;
                }
                print_line(line, args.timestamps);
            }
        }
    } else if !args.follow {
        let file = std::fs::File::open(&log_path)?;
        let reader = BufReader::new(file);
        for line in reader.lines() {
            let line = line?;
            if use_json {
                print_json_line(
                    &line,
                    args.timestamps,
                    has_time_filter,
                    since.as_ref(),
                    until.as_ref(),
                );
            } else {
                if has_time_filter && !line_in_range(&line, since.as_ref(), until.as_ref()) {
                    continue;
                }
                print_line(&line, args.timestamps);
            }
        }
    }

    if args.follow {
        let file = std::fs::File::open(&log_path)?;
        let mut reader = BufReader::new(file);
        let mut pos = reader.seek(SeekFrom::End(0))?;

        loop {
            let mut line = String::new();
            match reader.read_line(&mut line) {
                Ok(0) => {
                    // Follow-by-name (like `tail -F`): the writer either ROTATES
                    // container.json (rename + new file → new inode) or TRUNCATES
                    // the raw console.log in place. Handle both so `--follow`
                    // never freezes on a stale fd after ~max-size of output.
                    //
                    // We only get here at EOF, so the old inode is already drained
                    // up to the rotation point — re-opening loses no lines.
                    let rotated = reader
                        .get_ref()
                        .metadata()
                        .map(|open| file_rotated(&open, &log_path))
                        .unwrap_or(false);
                    if rotated {
                        if let Ok(f) = std::fs::File::open(&log_path) {
                            reader = BufReader::new(f);
                            pos = 0;
                        }
                    } else if let Ok(meta) = reader.get_ref().metadata() {
                        // Truncated in place (bounded console.log): re-read from start.
                        if pos > meta.len() {
                            pos = reader.seek(SeekFrom::Start(0)).unwrap_or(0);
                        }
                    }
                    tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;
                }
                Ok(n) => {
                    pos += n as u64;
                    let trimmed = line.trim_end();
                    if use_json {
                        print_json_line(
                            trimmed,
                            args.timestamps,
                            has_time_filter,
                            since.as_ref(),
                            until.as_ref(),
                        );
                    } else {
                        if has_time_filter
                            && !line_in_range(trimmed, since.as_ref(), until.as_ref())
                        {
                            continue;
                        }
                        if args.timestamps {
                            print!("{} {}", Utc::now().to_rfc3339(), line);
                        } else {
                            print!("{line}");
                        }
                    }
                }
                Err(e) => {
                    return Err(format!("Error reading log: {e}").into());
                }
            }
        }
    }

    Ok(())
}

/// True if the file currently at `path` has a different inode than `open_meta` —
/// i.e. it was rotated (renamed away, a new one created) under our open fd. Lets
/// `--follow` re-open the new file (`tail -F`) instead of stalling on the old
/// inode after the writer rotates `container.json`.
#[cfg(unix)]
fn file_rotated(open_meta: &std::fs::Metadata, path: &std::path::Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata(path)
        .map(|cur| open_meta.ino() != cur.ino())
        .unwrap_or(false)
}

#[cfg(not(unix))]
fn file_rotated(_open_meta: &std::fs::Metadata, _path: &std::path::Path) -> bool {
    false
}

fn resolve_log_source(record: &BoxRecord) -> Option<LogSource> {
    let log_dir = record.box_dir.join("logs");
    let json_log = a3s_box_runtime::log::json_log_path(&log_dir);
    if json_log.exists() {
        return Some(LogSource {
            path: json_log,
            structured: true,
        });
    }

    if record.console_log.exists() {
        return Some(LogSource {
            path: record.console_log.clone(),
            structured: false,
        });
    }

    None
}

async fn wait_for_log_source(
    box_id: &str,
) -> Result<Option<LogSource>, Box<dyn std::error::Error>> {
    loop {
        let state = StateFile::load_default()?;
        let Some(record) = state.find_by_id(box_id) else {
            return Ok(None);
        };
        if let Some(source) = resolve_log_source(record) {
            return Ok(Some(source));
        }
        if record.status != "running" || record.log_config.driver == LogDriver::None {
            return Ok(None);
        }

        tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;
    }
}

/// Print a structured JSON log line, extracting the message and optional timestamp.
fn print_json_line(
    raw: &str,
    timestamps: bool,
    has_time_filter: bool,
    since: Option<&DateTime<Utc>>,
    until: Option<&DateTime<Utc>>,
) {
    let entry: LogEntry = match serde_json::from_str(raw) {
        Ok(e) => e,
        Err(_) => {
            // Fallback: print raw line if not valid JSON
            println!("{raw}");
            return;
        }
    };

    // Time filtering using the embedded timestamp
    if has_time_filter {
        if let Ok(ts) = entry.time.parse::<DateTime<Utc>>() {
            if let Some(s) = since {
                if ts < *s {
                    return;
                }
            }
            if let Some(u) = until {
                if ts > *u {
                    return;
                }
            }
        }
    }

    // The log field already contains the trailing newline. Route stderr lines to
    // the terminal's stderr, like Docker `logs`.
    let line = if timestamps {
        format!("{} {}", entry.time, entry.log)
    } else {
        entry.log.clone()
    };
    if entry.stream == "stderr" {
        use std::io::Write as _;
        let _ = std::io::stderr().write_all(line.as_bytes());
    } else {
        print!("{line}");
    }
}

/// Print a log line, optionally prepending a timestamp.
fn print_line(line: &str, timestamps: bool) {
    if timestamps {
        // Try to extract timestamp from the line, otherwise use current time
        println!("{} {}", Utc::now().to_rfc3339(), line);
    } else {
        println!("{line}");
    }
}

/// Parse a time filter string into a DateTime<Utc>.
///
/// Accepts:
/// - ISO 8601 timestamps: "2024-01-01T00:00:00Z"
/// - Relative durations: "1h", "30m", "2h30m", "1d"
fn parse_time_filter(s: &str) -> Result<DateTime<Utc>, String> {
    // Try ISO 8601 first
    if let Ok(dt) = s.parse::<DateTime<Utc>>() {
        return Ok(dt);
    }

    // Try relative duration (e.g., "1h", "30m", "1d", "2h30m")
    let duration = parse_duration(s)?;
    Ok(Utc::now() - duration)
}

/// Parse a relative duration (e.g. `30s`, `5m`, `1h`, `2d`, `1h30m`) into a
/// `chrono::Duration` for `--since`/`--until`. A zero duration is rejected here
/// because a relative time offset of 0 is not meaningful for log filtering.
fn parse_duration(s: &str) -> Result<chrono::Duration, String> {
    let secs = crate::output::parse_duration_secs(s)?;
    if secs == 0 {
        return Err(format!("Invalid duration: {s:?} (resolved to 0)"));
    }
    Ok(chrono::Duration::seconds(secs as i64))
}

/// Check if a log line falls within the [since, until] time range.
///
/// Tries to extract a timestamp from the beginning of the line.
/// If no timestamp is found, the line is included (permissive).
fn line_in_range(line: &str, since: Option<&DateTime<Utc>>, until: Option<&DateTime<Utc>>) -> bool {
    // Try to parse a timestamp from the start of the line
    let line_time = extract_line_timestamp(line);

    match line_time {
        Some(ts) => {
            if let Some(since) = since {
                if ts < *since {
                    return false;
                }
            }
            if let Some(until) = until {
                if ts > *until {
                    return false;
                }
            }
            true
        }
        // No timestamp found — include the line (permissive)
        None => true,
    }
}

/// Try to extract an ISO 8601 timestamp from the beginning of a log line.
fn extract_line_timestamp(line: &str) -> Option<DateTime<Utc>> {
    // Common log formats: "2024-01-01T00:00:00Z ...", "2024-01-01T00:00:00.000Z ..."
    // Try first 30 chars as a timestamp
    let prefix = if line.len() > 35 { &line[..35] } else { line };

    // Find the first space to isolate the potential timestamp
    let ts_str = prefix.split_whitespace().next()?;
    ts_str.parse::<DateTime<Utc>>().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[cfg(unix)]
    fn test_file_rotated_detects_inode_swap() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("container.json");
        std::fs::write(&path, b"old\n").unwrap();
        let open = std::fs::File::open(&path).unwrap().metadata().unwrap();

        // Same inode → not rotated.
        assert!(!file_rotated(&open, &path));

        // Rotate like the writer does: rename away, create a fresh file.
        std::fs::rename(&path, dir.path().join("container.json.1")).unwrap();
        std::fs::write(&path, b"new\n").unwrap();
        assert!(file_rotated(&open, &path));

        // Missing file → not rotated (no panic).
        assert!(!file_rotated(&open, &dir.path().join("gone")));
    }

    #[test]
    fn test_parse_duration_seconds() {
        let d = parse_duration("30s").unwrap();
        assert_eq!(d.num_seconds(), 30);
    }

    #[test]
    fn test_parse_duration_minutes() {
        let d = parse_duration("5m").unwrap();
        assert_eq!(d.num_seconds(), 300);
    }

    #[test]
    fn test_parse_duration_hours() {
        let d = parse_duration("2h").unwrap();
        assert_eq!(d.num_seconds(), 7200);
    }

    #[test]
    fn test_parse_duration_days() {
        let d = parse_duration("1d").unwrap();
        assert_eq!(d.num_seconds(), 86400);
    }

    #[test]
    fn test_parse_duration_combined() {
        let d = parse_duration("1h30m").unwrap();
        assert_eq!(d.num_seconds(), 5400);
    }

    #[test]
    fn test_parse_duration_bare_number_as_seconds() {
        let d = parse_duration("60").unwrap();
        assert_eq!(d.num_seconds(), 60);
    }

    #[test]
    fn test_parse_duration_invalid() {
        assert!(parse_duration("abc").is_err());
    }

    #[test]
    fn test_parse_time_filter_iso8601() {
        let dt = parse_time_filter("2024-06-15T12:00:00Z").unwrap();
        assert_eq!(dt.to_rfc3339(), "2024-06-15T12:00:00+00:00");
    }

    #[test]
    fn test_parse_time_filter_relative() {
        let before = Utc::now();
        let dt = parse_time_filter("1h").unwrap();
        let after = Utc::now();

        // Should be approximately 1 hour ago
        let diff = before - dt;
        assert!(diff.num_seconds() >= 3599 && diff.num_seconds() <= 3601);
        assert!(dt < after);
    }

    #[test]
    fn test_line_in_range_no_filter() {
        assert!(line_in_range("some log line", None, None));
    }

    #[test]
    fn test_line_in_range_no_timestamp_permissive() {
        let since = Utc::now();
        assert!(line_in_range("no timestamp here", Some(&since), None));
    }

    #[test]
    fn test_extract_line_timestamp_valid() {
        let ts = extract_line_timestamp("2024-06-15T12:00:00Z some log message");
        assert!(ts.is_some());
        assert_eq!(ts.unwrap().to_rfc3339(), "2024-06-15T12:00:00+00:00");
    }

    #[test]
    fn test_extract_line_timestamp_none() {
        let ts = extract_line_timestamp("just a regular log line");
        assert!(ts.is_none());
    }

    #[test]
    fn test_resolve_log_source_prefers_structured_json() {
        let tmp = tempfile::tempdir().unwrap();
        let mut record =
            crate::test_helpers::fixtures::make_record("log-id", "logs", "stopped", None);
        record.box_dir = tmp.path().join("box");
        let log_dir = record.box_dir.join("logs");
        std::fs::create_dir_all(&log_dir).unwrap();
        record.console_log = log_dir.join("console.log");
        std::fs::write(&record.console_log, "console\n").unwrap();
        let json_log = a3s_box_runtime::log::json_log_path(&log_dir);
        std::fs::write(&json_log, "{}\n").unwrap();

        let source = resolve_log_source(&record).unwrap();
        assert!(source.structured);
        assert_eq!(source.path, json_log);
    }

    #[test]
    fn test_resolve_log_source_falls_back_to_console_log() {
        let tmp = tempfile::tempdir().unwrap();
        let mut record =
            crate::test_helpers::fixtures::make_record("log-id", "logs", "stopped", None);
        record.box_dir = tmp.path().join("box");
        let log_dir = record.box_dir.join("logs");
        std::fs::create_dir_all(&log_dir).unwrap();
        record.console_log = log_dir.join("console.log");
        std::fs::write(&record.console_log, "console\n").unwrap();

        let source = resolve_log_source(&record).unwrap();
        assert!(!source.structured);
        assert_eq!(source.path, record.console_log);
    }

    #[test]
    fn test_resolve_log_source_missing_is_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let mut record =
            crate::test_helpers::fixtures::make_record("log-id", "logs", "stopped", None);
        record.box_dir = tmp.path().join("box");
        record.console_log = record.box_dir.join("logs").join("console.log");

        assert!(resolve_log_source(&record).is_none());
    }
}
