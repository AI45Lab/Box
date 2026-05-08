//! `a3s-box inject-secret` command — Inject secrets into a TEE via RA-TLS.
//!
//! Connects to a running box's RA-TLS attestation server, verifies the TEE,
//! then injects secrets over the encrypted channel. Secrets are stored in
//! `/run/secrets/<name>` inside the guest (tmpfs, mode 0600).

use clap::Args;

#[cfg(not(windows))]
use crate::resolve;
#[cfg(not(windows))]
use crate::state::StateFile;

#[cfg(not(windows))]
use a3s_box_runtime::{tee::AttestationPolicy, SecretEntry, SecretInjector};

#[derive(Args)]
pub struct InjectSecretArgs {
    /// Box name or ID
    pub r#box: String,

    /// Secret in NAME=VALUE format, can be repeated
    #[arg(short = 's', long = "secret")]
    pub secrets: Vec<String>,

    /// Also set the secret as an environment variable in the guest
    #[arg(long)]
    pub set_env: bool,

    /// Accept simulated (non-hardware) TEE reports for development/testing
    #[arg(long)]
    pub allow_simulated: bool,

    /// Read secrets from a file (one NAME=VALUE per line)
    #[arg(long)]
    pub file: Option<String>,
}

/// JSON output for the inject-secret command.
#[cfg(not(windows))]
#[derive(serde::Serialize)]
struct InjectOutput {
    box_name: String,
    injected: usize,
    secrets: Vec<String>,
}

#[cfg(windows)]
pub async fn execute(_args: InjectSecretArgs) -> Result<(), Box<dyn std::error::Error>> {
    Err(crate::platform::unsupported_command(
        "inject-secret",
        "RA-TLS secret injection channel support",
    ))
}

#[cfg(not(windows))]
pub async fn execute(args: InjectSecretArgs) -> Result<(), Box<dyn std::error::Error>> {
    let state = StateFile::load_default()?;
    let record = resolve::resolve(&state, &args.r#box)?;
    let attest_socket_path = crate::socket_paths::require_runtime_socket(
        record,
        crate::socket_paths::RuntimeSocket::Attest,
    )
    .map_err(|e| -> Box<dyn std::error::Error> { e.into() })?;
    let socket_path = &attest_socket_path;

    // Collect secrets from --secret and --file
    let mut entries = Vec::new();

    for secret_str in &args.secrets {
        let entry = parse_secret(secret_str, args.set_env)?;
        entries.push(entry);
    }

    if let Some(path) = &args.file {
        let content = std::fs::read_to_string(path)
            .map_err(|e| format!("Failed to read secrets file '{}': {}", path, e))?;
        for line in content.lines() {
            let trimmed = line.trim();
            if trimmed.is_empty() || trimmed.starts_with('#') {
                continue;
            }
            let entry = parse_secret(trimmed, args.set_env)?;
            entries.push(entry);
        }
    }

    if entries.is_empty() {
        return Err("No secrets provided. Use --secret NAME=VALUE or --file PATH".into());
    }

    let injector = SecretInjector::new(socket_path);
    let result = injector
        .inject(&entries, AttestationPolicy::default(), args.allow_simulated)
        .await?;

    let secret_names: Vec<String> = entries.iter().map(|e| e.name.clone()).collect();

    let output = InjectOutput {
        box_name: record.name.clone(),
        injected: result.injected,
        secrets: secret_names,
    };

    println!("{}", serde_json::to_string_pretty(&output)?);
    Ok(())
}

/// Parse a "NAME=VALUE" string into a SecretEntry.
#[cfg(not(windows))]
fn parse_secret(s: &str, set_env: bool) -> Result<SecretEntry, String> {
    let (name, value) = s
        .split_once('=')
        .ok_or_else(|| format!("Invalid secret format (expected NAME=VALUE): {}", s))?;

    if name.is_empty() {
        return Err(format!("Secret name cannot be empty: {}", s));
    }

    Ok(SecretEntry {
        name: name.to_string(),
        value: value.to_string(),
        set_env,
    })
}

#[cfg(test)]
#[cfg(not(windows))]
mod tests {
    use super::*;

    #[test]
    fn test_parse_secret_basic() {
        let entry = parse_secret("API_KEY=sk-12345", false).unwrap();
        assert_eq!(entry.name, "API_KEY");
        assert_eq!(entry.value, "sk-12345");
        assert!(!entry.set_env);
    }

    #[test]
    fn test_parse_secret_with_env() {
        let entry = parse_secret("DB_PASS=secret", true).unwrap();
        assert_eq!(entry.name, "DB_PASS");
        assert_eq!(entry.value, "secret");
        assert!(entry.set_env);
    }

    #[test]
    fn test_parse_secret_value_with_equals() {
        let entry = parse_secret("CONN=postgres://host?opt=1", false).unwrap();
        assert_eq!(entry.name, "CONN");
        assert_eq!(entry.value, "postgres://host?opt=1");
    }

    #[test]
    fn test_parse_secret_empty_value() {
        let entry = parse_secret("EMPTY=", false).unwrap();
        assert_eq!(entry.name, "EMPTY");
        assert_eq!(entry.value, "");
    }

    #[test]
    fn test_parse_secret_no_equals() {
        assert!(parse_secret("INVALID", false).is_err());
    }

    #[test]
    fn test_parse_secret_empty_name() {
        assert!(parse_secret("=value", false).is_err());
    }
}
