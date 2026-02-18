//! Sealed storage for TEE-bound encryption.
//!
//! Provides encryption/decryption of data bound to the TEE's identity
//! (measurement + chip_id). Only the same TEE running the same firmware
//! and guest image on the same physical chip can unseal the data.
//!
//! ## Key Derivation
//!
//! The sealing key is derived using HKDF-SHA256:
//! - IKM (Input Key Material): `measurement || chip_id` from the SNP report
//! - Salt: "a3s-sealed-storage-v1"
//! - Info: caller-provided context (e.g., "session-keys", "model-weights")
//!
//! ## Encryption
//!
//! AES-256-GCM with a random 96-bit nonce per seal operation.
//! The sealed blob format: `nonce (12 bytes) || ciphertext+tag`
//!
//! ## Sealing Policies
//!
//! - `MeasurementAndChip`: Binds to both measurement and chip (strictest)
//! - `MeasurementOnly`: Binds to measurement only (portable across chips)
//! - `ChipOnly`: Binds to chip only (survives firmware updates)

use a3s_box_core::error::{BoxError, Result};
use ring::aead::{self, Aad, BoundKey, Nonce, NonceSequence, NONCE_LEN};
use ring::hkdf;
use serde::{Deserialize, Serialize};

/// Salt for HKDF key derivation.
const HKDF_SALT: &[u8] = b"a3s-sealed-storage-v1";

/// Sealing policy determines what TEE identity fields bind the key.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum SealingPolicy {
    /// Bind to both measurement and chip_id (strictest).
    /// Data can only be unsealed by the exact same guest image
    /// on the exact same physical chip.
    #[default]
    MeasurementAndChip,

    /// Bind to measurement only (portable across chips).
    /// Data can be unsealed by the same guest image on any chip.
    MeasurementOnly,

    /// Bind to chip only (survives firmware/image updates).
    /// Data can be unsealed by any guest image on the same chip.
    ChipOnly,
}

/// Sealed data blob with metadata for unsealing.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SealedData {
    /// The sealing policy used.
    pub policy: SealingPolicy,
    /// Context string used for key derivation.
    pub context: String,
    /// Sealed blob: nonce (12 bytes) || ciphertext || tag (16 bytes).
    #[serde(with = "base64_serde")]
    pub blob: Vec<u8>,
}

// ============================================================================
// Seal / Unseal operations
// ============================================================================

/// Seal (encrypt) data bound to the TEE identity.
///
/// # Arguments
/// * `report` - Raw SNP report bytes (1184 bytes) containing measurement and chip_id
/// * `plaintext` - Data to encrypt
/// * `context` - Application-specific context for key derivation (e.g., "session-keys")
/// * `policy` - Sealing policy determining which TEE fields bind the key
///
/// # Returns
/// A `SealedData` blob that can only be unsealed with the same TEE identity.
pub fn seal(
    report: &[u8],
    plaintext: &[u8],
    context: &str,
    policy: SealingPolicy,
) -> Result<SealedData> {
    let key = derive_sealing_key(report, context, policy)?;

    // Generate random nonce
    let rng = ring::rand::SystemRandom::new();
    let mut nonce_bytes = [0u8; NONCE_LEN];
    ring::rand::SecureRandom::fill(&rng, &mut nonce_bytes)
        .map_err(|_| BoxError::AttestationError("Failed to generate random nonce".to_string()))?;

    // Encrypt with AES-256-GCM
    let mut in_out = plaintext.to_vec();
    let unbound_key = aead::UnboundKey::new(&aead::AES_256_GCM, &key)
        .map_err(|_| BoxError::AttestationError("Failed to create AES-256-GCM key".to_string()))?;

    let nonce_seq = SingleNonce::new(nonce_bytes);
    let mut sealing_key = aead::SealingKey::new(unbound_key, nonce_seq);

    sealing_key
        .seal_in_place_append_tag(Aad::from(context.as_bytes()), &mut in_out)
        .map_err(|_| BoxError::AttestationError("AES-256-GCM seal failed".to_string()))?;

    // Prepend nonce to ciphertext
    let mut blob = Vec::with_capacity(NONCE_LEN + in_out.len());
    blob.extend_from_slice(&nonce_bytes);
    blob.extend_from_slice(&in_out);

    Ok(SealedData {
        policy,
        context: context.to_string(),
        blob,
    })
}

/// Unseal (decrypt) data using the TEE identity.
///
/// # Arguments
/// * `report` - Raw SNP report bytes (must match the TEE that sealed the data)
/// * `sealed` - The sealed data blob
///
/// # Returns
/// The original plaintext, or an error if the TEE identity doesn't match.
pub fn unseal(report: &[u8], sealed: &SealedData) -> Result<Vec<u8>> {
    if sealed.blob.len() < NONCE_LEN + aead::AES_256_GCM.tag_len() {
        return Err(BoxError::AttestationError(
            "Sealed blob too short".to_string(),
        ));
    }

    let key = derive_sealing_key(report, &sealed.context, sealed.policy)?;

    // Split nonce and ciphertext
    let nonce_bytes: [u8; NONCE_LEN] = sealed.blob[..NONCE_LEN]
        .try_into()
        .map_err(|_| BoxError::AttestationError("Invalid nonce in sealed blob".to_string()))?;

    let mut in_out = sealed.blob[NONCE_LEN..].to_vec();

    let unbound_key = aead::UnboundKey::new(&aead::AES_256_GCM, &key)
        .map_err(|_| BoxError::AttestationError("Failed to create AES-256-GCM key".to_string()))?;

    let nonce_seq = SingleNonce::new(nonce_bytes);
    let mut opening_key = aead::OpeningKey::new(unbound_key, nonce_seq);

    let plaintext = opening_key
        .open_in_place(Aad::from(sealed.context.as_bytes()), &mut in_out)
        .map_err(|_| {
            BoxError::AttestationError(
                "Unseal failed: TEE identity mismatch or data corrupted".to_string(),
            )
        })?;

    Ok(plaintext.to_vec())
}

// ============================================================================
// Key derivation
// ============================================================================

/// Derive a 256-bit sealing key from the SNP report using HKDF-SHA256.
fn derive_sealing_key(report: &[u8], context: &str, policy: SealingPolicy) -> Result<[u8; 32]> {
    // Extract measurement (0x90, 48 bytes) and chip_id (0x1A0, 64 bytes)
    if report.len() < 0x1E0 {
        return Err(BoxError::AttestationError(
            "Report too short to extract sealing identity".to_string(),
        ));
    }

    let measurement = &report[0x90..0xC0]; // 48 bytes
    let chip_id = &report[0x1A0..0x1E0]; // 64 bytes

    // Build IKM based on policy
    let ikm = match policy {
        SealingPolicy::MeasurementAndChip => {
            let mut v = Vec::with_capacity(112);
            v.extend_from_slice(measurement);
            v.extend_from_slice(chip_id);
            v
        }
        SealingPolicy::MeasurementOnly => measurement.to_vec(),
        SealingPolicy::ChipOnly => chip_id.to_vec(),
    };

    // HKDF extract + expand
    let salt = hkdf::Salt::new(hkdf::HKDF_SHA256, HKDF_SALT);
    let prk = salt.extract(&ikm);
    let info = [context.as_bytes()];
    let okm = prk
        .expand(&info, HkdfLen(32))
        .map_err(|_| BoxError::AttestationError("HKDF expand failed".to_string()))?;

    let mut key = [0u8; 32];
    okm.fill(&mut key)
        .map_err(|_| BoxError::AttestationError("HKDF fill failed".to_string()))?;

    Ok(key)
}

// ============================================================================
// ring helper types
// ============================================================================

/// A NonceSequence that yields a single nonce then fails.
struct SingleNonce {
    nonce: Option<[u8; NONCE_LEN]>,
}

impl SingleNonce {
    fn new(nonce: [u8; NONCE_LEN]) -> Self {
        Self { nonce: Some(nonce) }
    }
}

impl NonceSequence for SingleNonce {
    fn advance(&mut self) -> std::result::Result<Nonce, ring::error::Unspecified> {
        self.nonce
            .take()
            .map(Nonce::assume_unique_for_key)
            .ok_or(ring::error::Unspecified)
    }
}

/// HKDF output length wrapper for ring.
struct HkdfLen(usize);

impl hkdf::KeyType for HkdfLen {
    fn len(&self) -> usize {
        self.0
    }
}

// ============================================================================
// Base64 serde helper
// ============================================================================

mod base64_serde {
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(bytes: &Vec<u8>, s: S) -> std::result::Result<S::Ok, S::Error> {
        use base64::Engine;
        s.serialize_str(&base64::engine::general_purpose::STANDARD.encode(bytes))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> std::result::Result<Vec<u8>, D::Error> {
        use base64::Engine;
        let s = String::deserialize(d)?;
        base64::engine::general_purpose::STANDARD
            .decode(&s)
            .map_err(serde::de::Error::custom)
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a fake 1184-byte report with known measurement and chip_id.
    fn make_test_report() -> Vec<u8> {
        let mut report = vec![0u8; 1184];
        // measurement at 0x90 (48 bytes)
        for i in 0..48 {
            report[0x90 + i] = (i as u8).wrapping_mul(0xA3);
        }
        // chip_id at 0x1A0 (64 bytes)
        for b in &mut report[0x1A0..0x1E0] {
            *b = 0xA3;
        }
        report
    }

    #[test]
    fn test_seal_unseal_roundtrip() {
        let report = make_test_report();
        let plaintext = b"secret data for TEE";
        let sealed = seal(&report, plaintext, "test-context", SealingPolicy::default()).unwrap();
        let unsealed = unseal(&report, &sealed).unwrap();
        assert_eq!(unsealed, plaintext);
    }

    #[test]
    fn test_seal_unseal_measurement_only() {
        let report = make_test_report();
        let plaintext = b"measurement-bound secret";
        let sealed = seal(&report, plaintext, "ctx", SealingPolicy::MeasurementOnly).unwrap();
        let unsealed = unseal(&report, &sealed).unwrap();
        assert_eq!(unsealed, plaintext);
    }

    #[test]
    fn test_seal_unseal_chip_only() {
        let report = make_test_report();
        let plaintext = b"chip-bound secret";
        let sealed = seal(&report, plaintext, "ctx", SealingPolicy::ChipOnly).unwrap();
        let unsealed = unseal(&report, &sealed).unwrap();
        assert_eq!(unsealed, plaintext);
    }

    #[test]
    fn test_unseal_wrong_measurement_fails() {
        let report = make_test_report();
        let plaintext = b"secret";
        let sealed = seal(&report, plaintext, "ctx", SealingPolicy::MeasurementOnly).unwrap();

        // Different measurement
        let mut wrong_report = report.clone();
        wrong_report[0x90] = 0xFF;
        let result = unseal(&wrong_report, &sealed);
        assert!(result.is_err());
    }

    #[test]
    fn test_unseal_wrong_chip_fails() {
        let report = make_test_report();
        let plaintext = b"secret";
        let sealed = seal(&report, plaintext, "ctx", SealingPolicy::ChipOnly).unwrap();

        // Different chip_id
        let mut wrong_report = report.clone();
        wrong_report[0x1A0] = 0xFF;
        let result = unseal(&wrong_report, &sealed);
        assert!(result.is_err());
    }

    #[test]
    fn test_unseal_wrong_context_fails() {
        let report = make_test_report();
        let plaintext = b"secret";
        let sealed = seal(&report, plaintext, "context-a", SealingPolicy::default()).unwrap();

        // Try to unseal with different context
        let mut tampered = sealed.clone();
        tampered.context = "context-b".to_string();
        let result = unseal(&report, &tampered);
        assert!(result.is_err());
    }

    #[test]
    fn test_unseal_tampered_blob_fails() {
        let report = make_test_report();
        let plaintext = b"secret";
        let sealed = seal(&report, plaintext, "ctx", SealingPolicy::default()).unwrap();

        // Tamper with ciphertext
        let mut tampered = sealed.clone();
        if let Some(byte) = tampered.blob.get_mut(NONCE_LEN + 1) {
            *byte ^= 0xFF;
        }
        let result = unseal(&report, &tampered);
        assert!(result.is_err());
    }

    #[test]
    fn test_seal_empty_plaintext() {
        let report = make_test_report();
        let sealed = seal(&report, b"", "ctx", SealingPolicy::default()).unwrap();
        let unsealed = unseal(&report, &sealed).unwrap();
        assert!(unsealed.is_empty());
    }

    #[test]
    fn test_seal_large_plaintext() {
        let report = make_test_report();
        let plaintext = vec![0xAB; 1024 * 1024]; // 1 MiB
        let sealed = seal(&report, &plaintext, "ctx", SealingPolicy::default()).unwrap();
        let unsealed = unseal(&report, &sealed).unwrap();
        assert_eq!(unsealed, plaintext);
    }

    #[test]
    fn test_sealed_blob_size() {
        let report = make_test_report();
        let plaintext = b"hello";
        let sealed = seal(&report, plaintext, "ctx", SealingPolicy::default()).unwrap();
        // blob = nonce (12) + ciphertext (5) + tag (16) = 33
        assert_eq!(
            sealed.blob.len(),
            NONCE_LEN + plaintext.len() + aead::AES_256_GCM.tag_len()
        );
    }

    #[test]
    fn test_report_too_short() {
        let short_report = vec![0u8; 100];
        let result = seal(&short_report, b"data", "ctx", SealingPolicy::default());
        assert!(result.is_err());
    }

    #[test]
    fn test_sealed_data_serialization() {
        let report = make_test_report();
        let sealed = seal(&report, b"secret", "ctx", SealingPolicy::default()).unwrap();
        let json = serde_json::to_string(&sealed).unwrap();
        let deserialized: SealedData = serde_json::from_str(&json).unwrap();
        let unsealed = unseal(&report, &deserialized).unwrap();
        assert_eq!(unsealed, b"secret");
    }

    #[test]
    fn test_sealing_policy_default() {
        assert_eq!(SealingPolicy::default(), SealingPolicy::MeasurementAndChip);
    }

    #[test]
    fn test_different_nonces_per_seal() {
        let report = make_test_report();
        let s1 = seal(&report, b"same", "ctx", SealingPolicy::default()).unwrap();
        let s2 = seal(&report, b"same", "ctx", SealingPolicy::default()).unwrap();
        // Different nonces → different blobs
        assert_ne!(s1.blob, s2.blob);
        // But both unseal to the same plaintext
        assert_eq!(unseal(&report, &s1).unwrap(), b"same");
        assert_eq!(unseal(&report, &s2).unwrap(), b"same");
    }

    #[test]
    fn test_chip_only_survives_measurement_change() {
        let report = make_test_report();
        let sealed = seal(&report, b"secret", "ctx", SealingPolicy::ChipOnly).unwrap();

        // Change measurement but keep chip_id
        let mut updated_report = report.clone();
        updated_report[0x90] = 0xFF;
        let unsealed = unseal(&updated_report, &sealed).unwrap();
        assert_eq!(unsealed, b"secret");
    }

    #[test]
    fn test_measurement_only_survives_chip_change() {
        let report = make_test_report();
        let sealed = seal(&report, b"secret", "ctx", SealingPolicy::MeasurementOnly).unwrap();

        // Change chip_id but keep measurement
        let mut other_chip = report.clone();
        other_chip[0x1A0] = 0xFF;
        let unsealed = unseal(&other_chip, &sealed).unwrap();
        assert_eq!(unsealed, b"secret");
    }
}
