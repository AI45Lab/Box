//! `a3s-box seal` command — Encrypt data bound to a TEE's identity.
//!
//! Connects to a running box's RA-TLS attestation server, verifies the TEE,
//! then encrypts data using a key derived from the TEE's measurement and chip_id.
//! The sealed blob can only be decrypted by the same TEE.

use a3s_box_runtime::{tee::AttestationPolicy, SealClient};
use clap::Args;

use crate::resolve;
use crate::state::StateFile;

#[derive(Args)]
pub struct SealArgs {
    /// Box name or ID
    pub r#box: String,

    /// Data to seal (plaintext string)
    #[arg(long)]
    pub data: String,

    /// Application-specific context for key derivation (e.g., "model-weights", "api-keys")
    #[arg(long, default_value = "default")]
    pub context: String,

    /// Sealing policy: measurement-and-chip, measurement-only, chip-only
    #[arg(long, default_value = "measurement-and-chip")]
    pub policy: String,

    /// Accept simulated (non-hardware) TEE reports for development/testing
    #[arg(long)]
    pub allow_simulated: bool,

    /// Read data from a file instead of --data
    #[arg(long, conflicts_with = "data")]
    pub file: Option<String>,
}

/// JSON output for the seal command.
#[derive(serde::Serialize)]
struct SealOutput {
    box_name: String,
    blob: String,
    context: String,
    policy: String,
}

pub async fn execute(args: SealArgs) -> Result<(), Box<dyn std::error::Error>> {
    let state = StateFile::load_default()?;
    let record = resolve::resolve(&state, &args.r#box)?;

    if record.status != "running" {
        return Err(format!("Box {} is not running", record.name).into());
    }

    // Derive the attestation socket path from box_dir.
    let attest_socket_path = record.box_dir.join("sockets").join("attest.sock");
    let socket_path = &attest_socket_path;
    if !socket_path.exists() {
        return Err(format!(
            "Attestation socket not found for box {} at {}",
            record.name,
            socket_path.display()
        )
        .into());
    }

    // Read data from file or --data
    let data = match &args.file {
        Some(path) => {
            std::fs::read(path).map_err(|e| format!("Failed to read file '{}': {}", path, e))?
        }
        None => args.data.as_bytes().to_vec(),
    };

    // Normalize policy name
    let policy = normalize_policy(&args.policy)?;

    let client = SealClient::new(socket_path);
    let result = client
        .seal(
            &data,
            &args.context,
            &policy,
            AttestationPolicy::default(),
            args.allow_simulated,
        )
        .await?;

    let output = SealOutput {
        box_name: record.name.clone(),
        blob: result.blob,
        context: result.context,
        policy: result.policy,
    };

    println!("{}", serde_json::to_string_pretty(&output)?);
    Ok(())
}

/// Normalize CLI-friendly policy names to internal format.
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
    fn test_normalize_policy_measurement_and_chip() {
        assert_eq!(
            normalize_policy("measurement-and-chip").unwrap(),
            "MeasurementAndChip"
        );
    }

    #[test]
    fn test_normalize_policy_measurement_only() {
        assert_eq!(
            normalize_policy("measurement-only").unwrap(),
            "MeasurementOnly"
        );
    }

    #[test]
    fn test_normalize_policy_chip_only() {
        assert_eq!(normalize_policy("chip-only").unwrap(), "ChipOnly");
    }

    #[test]
    fn test_normalize_policy_case_insensitive() {
        assert_eq!(
            normalize_policy("Measurement-And-Chip").unwrap(),
            "MeasurementAndChip"
        );
        assert_eq!(normalize_policy("CHIP-ONLY").unwrap(), "ChipOnly");
    }

    #[test]
    fn test_normalize_policy_invalid() {
        assert!(normalize_policy("invalid").is_err());
        assert!(normalize_policy("").is_err());
    }
}
