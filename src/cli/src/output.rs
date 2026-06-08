//! Table formatting helpers for CLI output.

use comfy_table::{ContentArrangement, Table};

/// Create a styled table with the given headers.
pub fn new_table(headers: &[&str]) -> Table {
    let mut table = Table::new();
    table.set_content_arrangement(ContentArrangement::Dynamic);
    table.load_preset(comfy_table::presets::NOTHING);
    table.set_header(headers);
    table
}

/// Format a byte count as a human-readable string.
pub fn format_bytes(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = 1024 * KB;
    const GB: u64 = 1024 * MB;

    if bytes >= GB {
        format!("{:.1} GB", bytes as f64 / GB as f64)
    } else if bytes >= MB {
        format!("{:.1} MB", bytes as f64 / MB as f64)
    } else if bytes >= KB {
        format!("{:.1} KB", bytes as f64 / KB as f64)
    } else {
        format!("{} B", bytes)
    }
}

/// Format a chrono timestamp as a relative "ago" string.
pub fn format_ago(dt: &chrono::DateTime<chrono::Utc>) -> String {
    let now = chrono::Utc::now();
    let duration = now.signed_duration_since(*dt);

    let secs = duration.num_seconds();
    if secs < 0 {
        return "just now".to_string();
    }

    if secs < 60 {
        return format!("{secs} seconds ago");
    }

    let mins = duration.num_minutes();
    if mins < 60 {
        return format!("{mins} minutes ago");
    }

    let hours = duration.num_hours();
    if hours < 24 {
        return format!("{hours} hours ago");
    }

    let days = duration.num_days();
    if days < 30 {
        return format!("{days} days ago");
    }

    let months = days / 30;
    if months < 12 {
        return format!("{months} months ago");
    }

    let years = days / 365;
    format!("{years} years ago")
}

/// Parse a size string like "500m", "10g", "1t" into bytes.
///
/// Supported suffixes (case-insensitive): `b`, `k`/`kb`, `m`/`mb`, `g`/`gb`, `t`/`tb`.
/// No suffix assumes bytes.
pub fn parse_size_bytes(s: &str) -> Result<u64, String> {
    let s = s.trim().to_lowercase();
    if s.is_empty() {
        return Err("empty size value".to_string());
    }

    const KB: u64 = 1024;
    const MB: u64 = 1024 * KB;
    const GB: u64 = 1024 * MB;
    const TB: u64 = 1024 * GB;

    let (num_str, multiplier) = if let Some(n) = s.strip_suffix("tb") {
        (n, TB)
    } else if let Some(n) = s.strip_suffix('t') {
        (n, TB)
    } else if let Some(n) = s.strip_suffix("gb") {
        (n, GB)
    } else if let Some(n) = s.strip_suffix('g') {
        (n, GB)
    } else if let Some(n) = s.strip_suffix("mb") {
        (n, MB)
    } else if let Some(n) = s.strip_suffix('m') {
        (n, MB)
    } else if let Some(n) = s.strip_suffix("kb") {
        (n, KB)
    } else if let Some(n) = s.strip_suffix('k') {
        (n, KB)
    } else if let Some(n) = s.strip_suffix('b') {
        (n, 1u64)
    } else {
        // Assume bytes if no suffix
        (s.as_str(), 1u64)
    };

    let num: u64 = num_str
        .parse()
        .map_err(|_| format!("invalid size value: {s}"))?;

    Ok(num * multiplier)
}

/// Parse a Docker/Go-style duration string into whole seconds.
///
/// Accepts a bare integer (seconds, kept for backward compatibility) or a
/// duration with unit suffixes — `ns`, `us`/`µs`, `ms`, `s`, `m`, `h`, `d` —
/// including compounds like `1m30s` or `2h45m`. Sub-second components are
/// rounded to the nearest second. Used for Docker-compatible
/// `--health-interval`/`--health-timeout`/`--health-start-period` and for
/// `logs --since`/`--until` (via `logs::parse_duration`).
pub fn parse_duration_secs(s: &str) -> Result<u64, String> {
    let t = s.trim();
    if t.is_empty() {
        return Err("empty duration value".to_string());
    }
    // Bare integer → seconds (backward compatible with the old plain-int form).
    if let Ok(n) = t.parse::<u64>() {
        return Ok(n);
    }
    if t.starts_with('-') {
        return Err(format!("negative duration not allowed: {s}"));
    }

    let mut total_secs = 0f64;
    let mut num = String::new();
    let mut saw_unit = false;
    let mut chars = t.chars().peekable();
    while let Some(&c) = chars.peek() {
        if c.is_ascii_digit() || c == '.' {
            num.push(c);
            chars.next();
            continue;
        }
        let mut unit = String::new();
        while let Some(&u) = chars.peek() {
            if u.is_ascii_alphabetic() || u == 'µ' {
                unit.push(u);
                chars.next();
            } else {
                break;
            }
        }
        if num.is_empty() || unit.is_empty() {
            return Err(format!("invalid duration: {s}"));
        }
        let value: f64 = num
            .parse()
            .map_err(|_| format!("invalid duration number in: {s}"))?;
        let unit_secs = match unit.as_str() {
            "ns" => 1e-9,
            "us" | "µs" => 1e-6,
            "ms" => 1e-3,
            "s" => 1.0,
            "m" => 60.0,
            "h" => 3600.0,
            "d" => 86400.0,
            other => return Err(format!("unknown duration unit '{other}' in: {s}")),
        };
        total_secs += value * unit_secs;
        num.clear();
        saw_unit = true;
    }
    if !num.is_empty() {
        return Err(format!("duration missing unit in: {s}"));
    }
    if !saw_unit {
        return Err(format!("invalid duration: {s}"));
    }
    Ok(total_secs.round() as u64)
}

/// Parse a memory string like "512m", "2g" into megabytes.
pub fn parse_memory(s: &str) -> Result<u32, String> {
    let s = s.trim().to_lowercase();
    if s.is_empty() {
        return Err("empty memory value".to_string());
    }

    let (num_str, multiplier) = if let Some(n) = s.strip_suffix('g') {
        (n, 1024u32)
    } else if let Some(n) = s.strip_suffix("gb") {
        (n, 1024)
    } else if let Some(n) = s.strip_suffix('m') {
        (n, 1)
    } else if let Some(n) = s.strip_suffix("mb") {
        (n, 1)
    } else {
        // Assume megabytes if no suffix
        (s.as_str(), 1)
    };

    let num: u32 = num_str
        .parse()
        .map_err(|_| format!("invalid memory value: {s}"))?;

    Ok(num * multiplier)
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- parse_duration_secs tests ---

    #[test]
    fn test_parse_duration_bare_integer_is_seconds() {
        assert_eq!(parse_duration_secs("30").unwrap(), 30);
        assert_eq!(parse_duration_secs("0").unwrap(), 0);
    }

    #[test]
    fn test_parse_duration_units() {
        assert_eq!(parse_duration_secs("30s").unwrap(), 30);
        assert_eq!(parse_duration_secs("1m").unwrap(), 60);
        assert_eq!(parse_duration_secs("2h").unwrap(), 7200);
        assert_eq!(parse_duration_secs("500ms").unwrap(), 1); // rounds to nearest second
        assert_eq!(parse_duration_secs("400ms").unwrap(), 0);
    }

    #[test]
    fn test_parse_duration_compound() {
        assert_eq!(parse_duration_secs("1m30s").unwrap(), 90);
        assert_eq!(parse_duration_secs("2h45m").unwrap(), 2 * 3600 + 45 * 60);
        assert_eq!(parse_duration_secs("1.5h").unwrap(), 5400);
    }

    #[test]
    fn test_parse_duration_rejects_garbage() {
        assert!(parse_duration_secs("").is_err());
        assert!(parse_duration_secs("abc").is_err());
        assert!(parse_duration_secs("10x").is_err());
        assert!(parse_duration_secs("-5s").is_err());
        assert!(parse_duration_secs("30 s").is_err());
    }

    // --- format_bytes tests ---

    #[test]
    fn test_format_bytes_zero() {
        assert_eq!(format_bytes(0), "0 B");
    }

    #[test]
    fn test_format_bytes_small() {
        assert_eq!(format_bytes(1), "1 B");
        assert_eq!(format_bytes(512), "512 B");
        assert_eq!(format_bytes(1023), "1023 B");
    }

    #[test]
    fn test_format_bytes_kilobytes() {
        assert_eq!(format_bytes(1024), "1.0 KB");
        assert_eq!(format_bytes(1536), "1.5 KB");
        assert_eq!(format_bytes(10240), "10.0 KB");
    }

    #[test]
    fn test_format_bytes_megabytes() {
        assert_eq!(format_bytes(1048576), "1.0 MB");
        assert_eq!(format_bytes(1048576 + 524288), "1.5 MB");
        assert_eq!(format_bytes(100 * 1048576), "100.0 MB");
    }

    #[test]
    fn test_format_bytes_gigabytes() {
        assert_eq!(format_bytes(1073741824), "1.0 GB");
        assert_eq!(format_bytes(10 * 1073741824), "10.0 GB");
    }

    // --- parse_memory tests ---

    #[test]
    fn test_parse_memory_megabytes() {
        assert_eq!(parse_memory("512m").unwrap(), 512);
        assert_eq!(parse_memory("512M").unwrap(), 512);
        assert_eq!(parse_memory("1024mb").unwrap(), 1024);
        assert_eq!(parse_memory("256MB").unwrap(), 256);
    }

    #[test]
    fn test_parse_memory_gigabytes() {
        assert_eq!(parse_memory("1g").unwrap(), 1024);
        assert_eq!(parse_memory("2G").unwrap(), 2048);
        assert_eq!(parse_memory("4gb").unwrap(), 4096);
        assert_eq!(parse_memory("4GB").unwrap(), 4096);
    }

    #[test]
    fn test_parse_memory_no_suffix() {
        assert_eq!(parse_memory("512").unwrap(), 512);
        assert_eq!(parse_memory("1024").unwrap(), 1024);
    }

    #[test]
    fn test_parse_memory_with_whitespace() {
        assert_eq!(parse_memory("  512m  ").unwrap(), 512);
        assert_eq!(parse_memory("  2g ").unwrap(), 2048);
    }

    #[test]
    fn test_parse_memory_invalid_empty() {
        assert!(parse_memory("").is_err());
    }

    #[test]
    fn test_parse_memory_invalid_letters() {
        assert!(parse_memory("abc").is_err());
    }

    #[test]
    fn test_parse_memory_invalid_float() {
        assert!(parse_memory("12.5m").is_err());
    }

    #[test]
    fn test_parse_memory_invalid_negative() {
        assert!(parse_memory("-512m").is_err());
    }

    #[test]
    fn test_parse_memory_zero() {
        assert_eq!(parse_memory("0m").unwrap(), 0);
        assert_eq!(parse_memory("0").unwrap(), 0);
    }

    // --- parse_size_bytes tests ---

    #[test]
    fn test_parse_size_bytes_bytes() {
        assert_eq!(parse_size_bytes("0").unwrap(), 0);
        assert_eq!(parse_size_bytes("1024").unwrap(), 1024);
        assert_eq!(parse_size_bytes("100b").unwrap(), 100);
        assert_eq!(parse_size_bytes("100B").unwrap(), 100);
    }

    #[test]
    fn test_parse_size_bytes_kilobytes() {
        assert_eq!(parse_size_bytes("1k").unwrap(), 1024);
        assert_eq!(parse_size_bytes("1K").unwrap(), 1024);
        assert_eq!(parse_size_bytes("1kb").unwrap(), 1024);
        assert_eq!(parse_size_bytes("1KB").unwrap(), 1024);
        assert_eq!(parse_size_bytes("512k").unwrap(), 512 * 1024);
    }

    #[test]
    fn test_parse_size_bytes_megabytes() {
        assert_eq!(parse_size_bytes("1m").unwrap(), 1024 * 1024);
        assert_eq!(parse_size_bytes("1M").unwrap(), 1024 * 1024);
        assert_eq!(parse_size_bytes("500mb").unwrap(), 500 * 1024 * 1024);
        assert_eq!(parse_size_bytes("500MB").unwrap(), 500 * 1024 * 1024);
    }

    #[test]
    fn test_parse_size_bytes_gigabytes() {
        assert_eq!(parse_size_bytes("1g").unwrap(), 1024 * 1024 * 1024);
        assert_eq!(parse_size_bytes("1G").unwrap(), 1024 * 1024 * 1024);
        assert_eq!(parse_size_bytes("10gb").unwrap(), 10 * 1024 * 1024 * 1024);
        assert_eq!(parse_size_bytes("10GB").unwrap(), 10 * 1024 * 1024 * 1024);
    }

    #[test]
    fn test_parse_size_bytes_terabytes() {
        assert_eq!(parse_size_bytes("1t").unwrap(), 1024 * 1024 * 1024 * 1024);
        assert_eq!(parse_size_bytes("1TB").unwrap(), 1024 * 1024 * 1024 * 1024);
    }

    #[test]
    fn test_parse_size_bytes_whitespace() {
        assert_eq!(
            parse_size_bytes("  10g  ").unwrap(),
            10 * 1024 * 1024 * 1024
        );
    }

    #[test]
    fn test_parse_size_bytes_invalid_empty() {
        assert!(parse_size_bytes("").is_err());
    }

    #[test]
    fn test_parse_size_bytes_invalid_letters() {
        assert!(parse_size_bytes("abc").is_err());
    }

    #[test]
    fn test_parse_size_bytes_invalid_float() {
        assert!(parse_size_bytes("1.5g").is_err());
    }

    #[test]
    fn test_parse_size_bytes_invalid_negative() {
        assert!(parse_size_bytes("-10g").is_err());
    }

    // --- new_table tests ---

    #[test]
    fn test_new_table() {
        let table = new_table(&["ID", "NAME", "STATUS"]);
        let output = table.to_string();
        assert!(output.contains("ID"));
        assert!(output.contains("NAME"));
        assert!(output.contains("STATUS"));
    }

    #[test]
    fn test_new_table_with_rows() {
        let mut table = new_table(&["COL1", "COL2"]);
        table.add_row(["hello", "world"]);
        table.add_row(["foo", "bar"]);
        let output = table.to_string();
        assert!(output.contains("hello"));
        assert!(output.contains("world"));
        assert!(output.contains("foo"));
        assert!(output.contains("bar"));
    }

    #[test]
    fn test_new_table_single_header() {
        let table = new_table(&["SINGLE"]);
        let output = table.to_string();
        assert!(output.contains("SINGLE"));
    }

    // --- format_ago tests ---

    #[test]
    fn test_format_ago_seconds() {
        let now = chrono::Utc::now();
        assert_eq!(format_ago(&now), "0 seconds ago");

        let thirty_sec = now - chrono::Duration::seconds(30);
        assert_eq!(format_ago(&thirty_sec), "30 seconds ago");
    }

    #[test]
    fn test_format_ago_minutes() {
        let now = chrono::Utc::now();
        let one_min = now - chrono::Duration::minutes(1);
        assert_eq!(format_ago(&one_min), "1 minutes ago");

        let five_min = now - chrono::Duration::minutes(5);
        assert_eq!(format_ago(&five_min), "5 minutes ago");

        let fifty_nine_min = now - chrono::Duration::minutes(59);
        assert_eq!(format_ago(&fifty_nine_min), "59 minutes ago");
    }

    #[test]
    fn test_format_ago_hours() {
        let now = chrono::Utc::now();
        let one_hour = now - chrono::Duration::hours(1);
        assert_eq!(format_ago(&one_hour), "1 hours ago");

        let two_hours = now - chrono::Duration::hours(2);
        assert_eq!(format_ago(&two_hours), "2 hours ago");

        let twenty_three_hours = now - chrono::Duration::hours(23);
        assert_eq!(format_ago(&twenty_three_hours), "23 hours ago");
    }

    #[test]
    fn test_format_ago_days() {
        let now = chrono::Utc::now();
        let one_day = now - chrono::Duration::days(1);
        assert_eq!(format_ago(&one_day), "1 days ago");

        let three_days = now - chrono::Duration::days(3);
        assert_eq!(format_ago(&three_days), "3 days ago");

        let twenty_nine_days = now - chrono::Duration::days(29);
        assert_eq!(format_ago(&twenty_nine_days), "29 days ago");
    }

    #[test]
    fn test_format_ago_months() {
        let now = chrono::Utc::now();
        let two_months = now - chrono::Duration::days(60);
        assert_eq!(format_ago(&two_months), "2 months ago");
    }

    #[test]
    fn test_format_ago_years() {
        let now = chrono::Utc::now();
        let two_years = now - chrono::Duration::days(730);
        assert_eq!(format_ago(&two_years), "2 years ago");
    }

    #[test]
    fn test_format_ago_future() {
        let now = chrono::Utc::now();
        let future = now + chrono::Duration::hours(1);
        assert_eq!(format_ago(&future), "just now");
    }
}
