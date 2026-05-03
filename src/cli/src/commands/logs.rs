//! `a3s-box logs` command — View box console logs.

use std::io::{BufRead, BufReader, Seek, SeekFrom};

use chrono::{DateTime, Utc};
use clap::Args;

use a3s_box_core::log::{LogDriver, LogEntry};

use crate::resolve;
use crate::state::StateFile;

#[derive(Args)]
pub struct LogsArgs {
    /// Box name or ID
    pub r#box: String,

    /// Follow log output
    #[arg(short, long)]
    pub follow: bool,

    /// Number of lines to show from the end, or "all"
    #[arg(long)]
    pub tail: Option<String>,

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

    // Prefer structured JSON log (container.json) when available
    let log_dir = record.box_dir.join("logs");
    let json_log = a3s_box_runtime::log::json_log_path(&log_dir);
    let use_json = json_log.exists();

    let log_path = if use_json {
        &json_log
    } else {
        &record.console_log
    };
    if !log_path.exists() {
        return Err(format!("No logs found for box {}", record.name).into());
    }

    let since = args.since.as_deref().map(parse_time_filter).transpose()?;
    let until = args.until.as_deref().map(parse_time_filter).transpose()?;
    let tail = args.tail.as_deref().map(parse_tail).transpose()?;

    let has_time_filter = since.is_some() || until.is_some();

    if let Some(TailMode::Lines(tail_n)) = tail {
        let file = std::fs::File::open(log_path)?;
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
    } else if tail == Some(TailMode::All) || !args.follow {
        let file = std::fs::File::open(log_path)?;
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
        let file = std::fs::File::open(log_path)?;
        let mut reader = BufReader::new(file);

        reader.seek(SeekFrom::End(0))?;

        loop {
            let mut line = String::new();
            match reader.read_line(&mut line) {
                Ok(0) => {
                    tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;
                }
                Ok(_) => {
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TailMode {
    All,
    Lines(usize),
}

fn parse_tail(value: &str) -> Result<TailMode, String> {
    if value == "all" {
        return Ok(TailMode::All);
    }

    let lines = value.parse::<usize>().map_err(|_| {
        format!("Invalid --tail value {value:?}: expected non-negative integer or 'all'")
    })?;
    Ok(TailMode::Lines(lines))
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

    // The log field already contains the trailing newline
    if timestamps {
        print!("{} {}", entry.time, entry.log);
    } else {
        print!("{}", entry.log);
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

/// Parse a human-readable duration string into a chrono::Duration.
///
/// Supports: "30s", "5m", "1h", "2d", "1h30m"
fn parse_duration(s: &str) -> Result<chrono::Duration, String> {
    let mut total_secs: i64 = 0;
    let mut num_buf = String::new();

    for ch in s.chars() {
        if ch.is_ascii_digit() {
            num_buf.push(ch);
        } else {
            let n: i64 = num_buf.parse().map_err(|_| {
                format!("Invalid duration: {s:?} (expected format like '1h', '30m', '2d')")
            })?;
            num_buf.clear();

            match ch {
                's' => total_secs += n,
                'm' => total_secs += n * 60,
                'h' => total_secs += n * 3600,
                'd' => total_secs += n * 86400,
                _ => {
                    return Err(format!(
                        "Unknown duration unit '{ch}' in {s:?} (expected s/m/h/d)"
                    ))
                }
            }
        }
    }

    if !num_buf.is_empty() {
        // Bare number without unit — treat as seconds
        let n: i64 = num_buf
            .parse()
            .map_err(|_| format!("Invalid duration: {s:?}"))?;
        total_secs += n;
    }

    if total_secs == 0 {
        return Err(format!("Invalid duration: {s:?} (resolved to 0)"));
    }

    Ok(chrono::Duration::seconds(total_secs))
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
    fn test_parse_tail_all() {
        assert_eq!(parse_tail("all").unwrap(), TailMode::All);
    }

    #[test]
    fn test_parse_tail_lines() {
        assert_eq!(parse_tail("25").unwrap(), TailMode::Lines(25));
    }

    #[test]
    fn test_parse_tail_zero() {
        assert_eq!(parse_tail("0").unwrap(), TailMode::Lines(0));
    }

    #[test]
    fn test_parse_tail_invalid() {
        assert!(parse_tail("latest").is_err());
        assert!(parse_tail("-1").is_err());
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
}
