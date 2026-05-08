//! `a3s-box unseal` command — Decrypt data inside a TEE.
//!
//! Connects to a running box's RA-TLS attestation server, verifies the TEE,
//! then decrypts a sealed blob using the TEE-bound key. Only succeeds if the
//! TEE identity matches the one that sealed the data.

use clap::Args;

#[cfg(not(windows))]
use crate::resolve;
#[cfg(not(windows))]
use crate::state::StateFile;

#[cfg(not(windows))]
use a3s_box_runtime::{tee::AttestationPolicy, SealClient};

#[derive(Args)]
pub struct UnsealArgs {
    /// Box name or ID
    pub r#box: String,

    /// Sealed blob (base64-encoded, from `a3s-box seal` output)
    #[arg(long)]
    pub blob: String,

    /// Context used during sealing
    #[arg(long, default_value = "default")]
    pub context: String,

    /// Sealing policy used during sealing: measurement-and-chip, measurement-only, chip-only
    #[arg(long, default_value = "measurement-and-chip")]
    pub policy: String,

    /// Accept simulated (non-hardware) TEE reports for development/testing
    #[arg(long)]
    pub allow_simulated: bool,

    /// Output raw bytes to stdout (for piping to files)
    #[arg(long)]
    pub raw: bool,
}

/// JSON output for the unseal command.
#[cfg(not(windows))]
#[derive(serde::Serialize)]
struct UnsealOutput {
    box_name: String,
    data: String,
    context: String,
    policy: String,
}

#[cfg(windows)]
pub async fn execute(_args: UnsealArgs) -> Result<(), Box<dyn std::error::Error>> {
    Err(crate::platform::unsupported_command(
        "unseal",
        "TEE sealed-storage channel support",
    ))
}

#[cfg(not(windows))]
pub async fn execute(args: UnsealArgs) -> Result<(), Box<dyn std::error::Error>> {
    let state = StateFile::load_default()?;
    let record = resolve::resolve(&state, &args.r#box)?;
    let attest_socket_path = crate::socket_paths::require_runtime_socket(
        record,
        crate::socket_paths::RuntimeSocket::Attest,
    )
    .map_err(|e| -> Box<dyn std::error::Error> { e.into() })?;
    let socket_path = &attest_socket_path;

    // Normalize policy name
    let policy = normalize_policy(&args.policy)?;

    let client = SealClient::new(socket_path);
    let plaintext = client
        .unseal(
            &args.blob,
            &args.context,
            &policy,
            AttestationPolicy::default(),
            args.allow_simulated,
        )
        .await?;

    if args.raw {
        use std::io::Write;
        std::io::stdout().write_all(&plaintext)?;
    } else {
        let data_str = String::from_utf8(plaintext).unwrap_or_else(|e| {
            use base64::Engine;
            format!(
                "(binary, base64): {}",
                base64::engine::general_purpose::STANDARD.encode(e.as_bytes())
            )
        });

        let output = UnsealOutput {
            box_name: record.name.clone(),
            data: data_str,
            context: args.context.clone(),
            policy: args.policy.clone(),
        };
        println!("{}", serde_json::to_string_pretty(&output)?);
    }

    Ok(())
}

/// Normalize CLI-friendly policy names to internal format.
#[cfg(any(not(windows), test))]
fn normalize_policy(policy: &str) -> Result<String, String> {
    match policy.to_lowercase().replace('-', "").as_str() {
        "measurementandchip" => Ok("MeasurementAndChip".to_string()),
        "measurementonly" => Ok("MeasurementOnly".to_string()),
        "chiponly" => Ok("ChipOnly".to_string()),
        _ => Err(format!(
            "Invalid sealing policy '{}'. Valid: measurement-and-chip, measurement-only, chip-only",
            policy
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_normalize_policy_all_variants() {
        assert_eq!(
            normalize_policy("measurement-and-chip").unwrap(),
            "MeasurementAndChip"
        );
        assert_eq!(
            normalize_policy("measurement-only").unwrap(),
            "MeasurementOnly"
        );
        assert_eq!(normalize_policy("chip-only").unwrap(), "ChipOnly");
    }

    #[test]
    fn test_normalize_policy_invalid() {
        assert!(normalize_policy("bad-policy").is_err());
    }
}
