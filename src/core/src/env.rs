//! Environment variable parsing and merging helpers.

use std::path::Path;

/// Parse `KEY=VALUE` strings into ordered pairs.
pub fn parse_env_vars(vars: &[String]) -> Result<Vec<(String, String)>, String> {
    vars.iter().map(|var| parse_env_var(var)).collect()
}

/// Parse a single `KEY=VALUE` string.
pub fn parse_env_var(var: &str) -> Result<(String, String), String> {
    let (key, value) = var
        .split_once('=')
        .ok_or_else(|| format!("Invalid environment variable (expected KEY=VALUE): {var}"))?;
    Ok((key.to_string(), value.to_string()))
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
/// value, matching Docker env-file behavior.
pub fn parse_env_file_content(content: &str) -> Vec<(String, String)> {
    content
        .lines()
        .filter_map(|line| {
            let trimmed = line.trim();
            if trimmed.is_empty() || trimmed.starts_with('#') {
                return None;
            }
            let (key, value) = trimmed
                .split_once('=')
                .map(|(key, value)| (key.trim(), value.trim()))
                .unwrap_or((trimmed, ""));
            Some((key.to_string(), value.to_string()))
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
