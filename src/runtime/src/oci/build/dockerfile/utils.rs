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

/// Parse a Go-style duration into whole seconds, matching Docker's HEALTHCHECK.
///
/// Accepts a bare integer (seconds) or one or more `<number><unit>` segments —
/// units `ns`, `us`/`µs`, `ms`, `s`, `m`, `h` — including compound forms like
/// `1m30s` (=90) or `2h45m`. Sub-second components round to the nearest second.
pub(super) fn parse_duration_secs(s: &str, line_num: usize) -> Result<u64> {
    let s = s.trim();
    if s.is_empty() {
        return Ok(0);
    }
    if let Ok(n) = s.parse::<u64>() {
        return Ok(n);
    }

    let invalid = || BoxError::BuildError(format!("Line {}: Invalid duration '{}'", line_num, s));

    let mut total_secs = 0f64;
    let mut num = String::new();
    let mut saw_unit = false;
    let mut chars = s.chars().peekable();
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
            return Err(invalid());
        }
        let value: f64 = num.parse().map_err(|_| invalid())?;
        let unit_secs = match unit.as_str() {
            "ns" => 1e-9,
            "us" | "µs" => 1e-6,
            "ms" => 1e-3,
            "s" => 1.0,
            "m" => 60.0,
            "h" => 3600.0,
            _ => return Err(invalid()),
        };
        total_secs += value * unit_secs;
        num.clear();
        saw_unit = true;
    }
    if !num.is_empty() || !saw_unit {
        return Err(invalid());
    }
    Ok(total_secs.round() as u64)
}
