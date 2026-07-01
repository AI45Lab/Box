//! Environment variable parsing and merging helpers.

use std::path::Path;

/// Parse `KEY=VALUE` strings into ordered pairs.
pub fn parse_env_vars(vars: &[String]) -> Result<Vec<(String, String)>, String> {
    vars.iter().map(|var| parse_env_var(var)).collect()
}

/// Parse a single `KEY=VALUE` string. A bare key (no `=`) is rejected; use
/// [`parse_runtime_env_var`] for the `docker run -e KEY` host-passthrough form.
pub fn parse_env_var(var: &str) -> Result<(String, String), String> {
    let (key, value) = var
        .split_once('=')
        .ok_or_else(|| format!("Invalid environment variable (expected KEY=VALUE): {var}"))?;
    Ok((key.to_string(), value.to_string()))
}

/// Parse a runtime `-e`/`--env` value, matching `docker run -e`.
///
/// `KEY=VALUE` sets the value explicitly; a bare `KEY` (no `=`) copies the
/// value from the host environment (empty string if unset) — Docker's
/// host-environment passthrough. Unlike [`parse_env_var`] this never errors, so
/// it must only back the runtime `--env` path (not `--label`/`--log-opt`).
pub fn parse_runtime_env_var(var: &str) -> (String, String) {
    match var.split_once('=') {
        Some((key, value)) => (key.to_string(), value.to_string()),
        None => (var.to_string(), std::env::var(var).unwrap_or_default()),
    }
}

/// Parse runtime `-e`/`--env` values (see [`parse_runtime_env_var`]).
pub fn parse_runtime_env_vars(vars: &[String]) -> Vec<(String, String)> {
    vars.iter().map(|var| parse_runtime_env_var(var)).collect()
}

/// Load environment variables from a Docker-style env file.
pub fn parse_env_file(path: impl AsRef<Path>) -> Result<Vec<(String, String)>, String> {
    let path = path.as_ref();
    let content = std::fs::read_to_string(path)
        .map_err(|e| format!("Failed to read env file '{}': {}", path.display(), e))?;
    Ok(parse_env_file_content(&content))
}

/// Parse Docker-style env file content.
///
/// Empty lines and `#` comments are skipped. A key without `=` gets an empty
/// value. The value after the first `=` is kept VERBATIM (leading/trailing
/// whitespace preserved), matching Docker — only the key is trimmed, and a
/// trailing CR from Windows line endings is stripped.
pub fn parse_env_file_content(content: &str) -> Vec<(String, String)> {
    content
        .lines()
        .filter_map(|line| {
            // Decide blank/comment on a leading-trimmed view without mutating
            // the value's whitespace.
            if line.trim_start().is_empty() || line.trim_start().starts_with('#') {
                return None;
            }
            let line = line.strip_suffix('\r').unwrap_or(line);
            match line.split_once('=') {
                Some((key, value)) => Some((key.trim().to_string(), value.to_string())),
                None => Some((line.trim().to_string(), String::new())),
            }
        })
        .collect()
}

/// Merge environment overrides into a base vector.
///
/// Existing keys are updated in place; new keys are appended.
pub fn merge_env_pairs(base: &mut Vec<(String, String)>, overrides: &[(String, String)]) {
    for (key, value) in overrides {
        if let Some(existing) = base
            .iter_mut()
            .find(|(existing_key, _)| existing_key == key)
        {
            existing.1 = value.clone();
        } else {
            base.push((key.clone(), value.clone()));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_parse_env_vars() {
        let vars = vec!["FOO=bar".to_string(), "A=B=C".to_string()];
        let parsed = parse_env_vars(&vars).unwrap();
        assert_eq!(
            parsed,
            vec![
                ("FOO".to_string(), "bar".to_string()),
                ("A".to_string(), "B=C".to_string())
            ]
        );
    }

    #[test]
    fn test_parse_env_vars_rejects_missing_equals() {
        let err = parse_env_var("FOO").unwrap_err();
        assert!(err.contains("KEY=VALUE"));
    }

    #[test]
    fn test_parse_runtime_env_var_explicit_and_host_passthrough() {
        // KEY=VALUE explicit
        assert_eq!(
            parse_runtime_env_var("FOO=bar"),
            ("FOO".to_string(), "bar".to_string())
        );
        // Bare KEY copies from the host env (docker run -e KEY).
        std::env::set_var("A3S_TEST_HOSTPASS", "host_value_123");
        assert_eq!(
            parse_runtime_env_var("A3S_TEST_HOSTPASS"),
            (
                "A3S_TEST_HOSTPASS".to_string(),
                "host_value_123".to_string()
            )
        );
        // Bare KEY unset on host => empty value, never an error.
        assert_eq!(
            parse_runtime_env_var("A3S_TEST_DEFINITELY_UNSET_XYZ"),
            ("A3S_TEST_DEFINITELY_UNSET_XYZ".to_string(), String::new())
        );
    }

    #[test]
    fn test_parse_env_file_preserves_value_whitespace() {
        // Docker keeps the value verbatim after the first '='; only the key is trimmed.
        let parsed = parse_env_file_content("PADDED=  spaced value  \nKEY=v\n");
        assert_eq!(
            parsed[0],
            ("PADDED".to_string(), "  spaced value  ".to_string())
        );
        assert_eq!(parsed[1], ("KEY".to_string(), "v".to_string()));
    }

    #[test]
    fn test_parse_env_file_content() {
        let parsed = parse_env_file_content(
            r#"
# comment
FOO=bar
EMPTY
WITH_EQUALS=a=b
"#,
        );

        assert_eq!(
            parsed,
            vec![
                ("FOO".to_string(), "bar".to_string()),
                ("EMPTY".to_string(), String::new()),
                ("WITH_EQUALS".to_string(), "a=b".to_string())
            ]
        );
    }

    #[test]
    fn test_parse_env_file() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("env");
        std::fs::write(&path, "FOO=bar\n").unwrap();

        let parsed = parse_env_file(&path).unwrap();

        assert_eq!(parsed, vec![("FOO".to_string(), "bar".to_string())]);
    }

    #[test]
    fn test_merge_env_pairs_overrides_and_appends() {
        let mut base = vec![
            ("FOO".to_string(), "image".to_string()),
            ("BAR".to_string(), "image".to_string()),
        ];
        let overrides = vec![
            ("FOO".to_string(), "cli".to_string()),
            ("BAZ".to_string(), "cli".to_string()),
        ];

        merge_env_pairs(&mut base, &overrides);

        assert_eq!(
            base,
            vec![
                ("FOO".to_string(), "cli".to_string()),
                ("BAR".to_string(), "image".to_string()),
                ("BAZ".to_string(), "cli".to_string())
            ]
        );
    }
}
