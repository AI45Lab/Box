//! Helper utilities for Dockerfile parsing.

use a3s_box_core::error::{BoxError, Result};

/// Parse a JSON array string like `["a", "b", "c"]` into a Vec<String>.
pub(super) fn parse_json_array(s: &str, line_num: usize) -> Result<Vec<String>> {
    let parsed: Vec<String> = serde_json::from_str(s).map_err(|e| {
        BoxError::BuildError(format!(
            "Line {}: Invalid JSON array '{}': {}",
            line_num, s, e
        ))
    })?;
    Ok(parsed)
}

/// Remove surrounding quotes from a string.
pub(super) fn unquote(s: &str) -> String {
    let s = s.trim();
    if (s.starts_with('"') && s.ends_with('"')) || (s.starts_with('\'') && s.ends_with('\'')) {
        s[1..s.len() - 1].to_string()
    } else {
        s.to_string()
    }
}

/// Simple whitespace-based split that respects quoted strings.
pub(super) fn shell_split(s: &str) -> Vec<&str> {
    s.split_whitespace().collect()
}

/// Parse a duration string like "30s", "5m", "1h" into seconds.
/// Plain numbers are treated as seconds.
pub(super) fn parse_duration_secs(s: &str, line_num: usize) -> Result<u64> {
    let s = s.trim();
    if s.is_empty() {
        return Ok(0);
    }

    if let Ok(n) = s.parse::<u64>() {
        return Ok(n);
    }

    let (num_str, suffix) = if let Some(stripped) = s.strip_suffix('s') {
        (stripped, "s")
    } else if let Some(stripped) = s.strip_suffix('m') {
        (stripped, "m")
    } else if let Some(stripped) = s.strip_suffix('h') {
        (stripped, "h")
    } else {
        return Err(BoxError::BuildError(format!(
            "Line {}: Invalid duration '{}' (use s/m/h suffix)",
            line_num, s
        )));
    };

    let num: u64 = num_str.parse().map_err(|_| {
        BoxError::BuildError(format!(
            "Line {}: Invalid duration number '{}'",
            line_num, num_str
        ))
    })?;

    match suffix {
        "s" => Ok(num),
        "m" => Ok(num * 60),
        "h" => Ok(num * 3600),
        _ => unreachable!(),
    }
}
