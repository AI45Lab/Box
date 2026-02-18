//! Version-based rollback protection for sealed storage.
//!
//! Extends sealed storage with a monotonic version counter to prevent
//! replay attacks where an attacker replaces a newer sealed blob with
//! an older one.
//!
//! ## How It Works
//!
//! - Each sealed blob includes a `version` field (monotonically increasing)
//! - A `VersionStore` persists the latest known version per context
//! - On unseal, the blob's version is checked against the stored version
//! - If the blob's version is older than the stored version, unseal is rejected
//!
//! ## Storage
//!
//! Version state is stored in `~/.a3s/sealed-versions.json` (or a custom path).
//! The file is atomically updated on each seal operation.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use a3s_box_core::error::{BoxError, Result};
use serde::{Deserialize, Serialize};

/// Versioned sealed data — extends SealedData with rollback protection.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VersionedSealedData {
    /// Monotonically increasing version number
    pub version: u64,
    /// Context string (key derivation + version tracking)
    pub context: String,
    /// Sealing policy used
    pub policy: super::sealed::SealingPolicy,
    /// Sealed blob: nonce || ciphertext || tag
    #[serde(with = "base64_serde")]
    pub blob: Vec<u8>,
}

/// Persistent store for version counters per context.
pub struct VersionStore {
    path: PathBuf,
    versions: HashMap<String, u64>,
}

impl VersionStore {
    /// Load or create a version store at the given path.
    pub fn load(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| {
                BoxError::Other(format!(
                    "Failed to create version store directory {}: {}",
                    parent.display(),
                    e
                ))
            })?;
        }

        let versions = if path.exists() {
            let data = std::fs::read_to_string(path).map_err(|e| {
                BoxError::Other(format!(
                    "Failed to read version store {}: {}",
                    path.display(),
                    e
                ))
            })?;
            serde_json::from_str(&data).unwrap_or_default()
        } else {
            HashMap::new()
        };

        Ok(Self {
            path: path.to_path_buf(),
            versions,
        })
    }

    /// Load from the default path (~/.a3s/sealed-versions.json).
    pub fn load_default() -> Result<Self> {
        let home = dirs::home_dir()
            .map(|h| h.join(".a3s"))
            .unwrap_or_else(|| PathBuf::from(".a3s"));
        Self::load(&home.join("sealed-versions.json"))
    }

    /// Get the current version for a context (0 if never sealed).
    pub fn current_version(&self, context: &str) -> u64 {
        self.versions.get(context).copied().unwrap_or(0)
    }

    /// Advance the version for a context and persist.
    /// Returns the new version number.
    pub fn advance(&mut self, context: &str) -> Result<u64> {
        let next = self.current_version(context) + 1;
        self.versions.insert(context.to_string(), next);
        self.save()?;
        Ok(next)
    }

    /// Check if a versioned blob is current (not rolled back).
    ///
    /// Returns `Ok(())` if the blob's version >= stored version,
    /// or an error if the blob is older (rollback detected).
    pub fn check_version(&self, context: &str, blob_version: u64) -> Result<()> {
        let stored = self.current_version(context);
        if blob_version < stored {
            return Err(BoxError::AttestationError(format!(
                "Rollback detected for context '{}': blob version {} < stored version {}",
                context, blob_version, stored
            )));
        }
        Ok(())
    }

    /// Update the stored version after successful unseal.
    /// Only advances if the blob version is newer.
    pub fn update_version(&mut self, context: &str, blob_version: u64) -> Result<()> {
        let stored = self.current_version(context);
        if blob_version > stored {
            self.versions.insert(context.to_string(), blob_version);
            self.save()?;
        }
        Ok(())
    }

    /// List all tracked contexts and their versions.
    pub fn list(&self) -> &HashMap<String, u64> {
        &self.versions
    }

    /// Remove version tracking for a context.
    pub fn remove(&mut self, context: &str) -> Result<bool> {
        if self.versions.remove(context).is_some() {
            self.save()?;
            Ok(true)
        } else {
            Ok(false)
        }
    }

    /// Save the version store atomically.
    fn save(&self) -> Result<()> {
        let data = serde_json::to_string_pretty(&self.versions)
            .map_err(|e| BoxError::Other(format!("Failed to serialize version store: {}", e)))?;
        let tmp = self.path.with_extension("json.tmp");
        std::fs::write(&tmp, &data).map_err(|e| {
            BoxError::Other(format!(
                "Failed to write version store {}: {}",
                tmp.display(),
                e
            ))
        })?;
        std::fs::rename(&tmp, &self.path)
            .map_err(|e| BoxError::Other(format!("Failed to rename version store: {}", e)))?;
        Ok(())
    }
}

/// Seal data with version-based rollback protection.
///
/// Advances the version counter for the context, then seals the data
/// with the version embedded in the blob.
pub fn seal_versioned(
    report: &[u8],
    plaintext: &[u8],
    context: &str,
    policy: super::sealed::SealingPolicy,
    version_store: &mut VersionStore,
) -> Result<VersionedSealedData> {
    let version = version_store.advance(context)?;
    let sealed = super::sealed::seal(report, plaintext, context, policy)?;

    Ok(VersionedSealedData {
        version,
        context: sealed.context,
        policy: sealed.policy,
        blob: sealed.blob,
    })
}

/// Unseal data with rollback protection check.
///
/// Verifies the blob's version against the stored version before unsealing.
/// After successful unseal, updates the stored version.
pub fn unseal_versioned(
    report: &[u8],
    versioned: &VersionedSealedData,
    version_store: &mut VersionStore,
) -> Result<Vec<u8>> {
    // Check for rollback
    version_store.check_version(&versioned.context, versioned.version)?;

    // Reconstruct SealedData for the underlying unseal
    let sealed = super::sealed::SealedData {
        policy: versioned.policy,
        context: versioned.context.clone(),
        blob: versioned.blob.clone(),
    };

    let plaintext = super::sealed::unseal(report, &sealed)?;

    // Update version after successful unseal
    version_store.update_version(&versioned.context, versioned.version)?;

    Ok(plaintext)
}

// Base64 serde helper (same as sealed.rs)
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

#[cfg(test)]
mod tests {
    use super::super::sealed::SealingPolicy;
    use super::*;
    use tempfile::TempDir;

    fn make_test_report() -> Vec<u8> {
        let mut report = vec![0u8; 1184];
        for i in 0..48 {
            report[0x90 + i] = (i as u8).wrapping_mul(0xA3);
        }
        for b in &mut report[0x1A0..0x1E0] {
            *b = 0xA3;
        }
        report
    }

    fn test_version_store(tmp: &TempDir) -> VersionStore {
        VersionStore::load(&tmp.path().join("versions.json")).unwrap()
    }

    #[test]
    fn test_version_store_new() {
        let tmp = TempDir::new().unwrap();
        let store = test_version_store(&tmp);
        assert_eq!(store.current_version("test"), 0);
        assert!(store.list().is_empty());
    }

    #[test]
    fn test_version_store_advance() {
        let tmp = TempDir::new().unwrap();
        let mut store = test_version_store(&tmp);

        assert_eq!(store.advance("ctx-a").unwrap(), 1);
        assert_eq!(store.advance("ctx-a").unwrap(), 2);
        assert_eq!(store.advance("ctx-b").unwrap(), 1);
        assert_eq!(store.current_version("ctx-a"), 2);
        assert_eq!(store.current_version("ctx-b"), 1);
    }

    #[test]
    fn test_version_store_persistence() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("versions.json");

        {
            let mut store = VersionStore::load(&path).unwrap();
            store.advance("ctx-1").unwrap();
            store.advance("ctx-1").unwrap();
            store.advance("ctx-2").unwrap();
        }

        {
            let store = VersionStore::load(&path).unwrap();
            assert_eq!(store.current_version("ctx-1"), 2);
            assert_eq!(store.current_version("ctx-2"), 1);
        }
    }

    #[test]
    fn test_version_store_check_version_ok() {
        let tmp = TempDir::new().unwrap();
        let mut store = test_version_store(&tmp);
        store.advance("ctx").unwrap(); // version = 1

        // Blob version >= stored version → OK
        assert!(store.check_version("ctx", 1).is_ok());
        assert!(store.check_version("ctx", 2).is_ok());
    }

    #[test]
    fn test_version_store_check_version_rollback() {
        let tmp = TempDir::new().unwrap();
        let mut store = test_version_store(&tmp);
        store.advance("ctx").unwrap(); // version = 1
        store.advance("ctx").unwrap(); // version = 2

        // Blob version < stored version → rollback
        let result = store.check_version("ctx", 1);
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("Rollback detected"));
    }

    #[test]
    fn test_version_store_update_version() {
        let tmp = TempDir::new().unwrap();
        let mut store = test_version_store(&tmp);

        store.update_version("ctx", 5).unwrap();
        assert_eq!(store.current_version("ctx"), 5);

        // Lower version doesn't update
        store.update_version("ctx", 3).unwrap();
        assert_eq!(store.current_version("ctx"), 5);
    }

    #[test]
    fn test_version_store_remove() {
        let tmp = TempDir::new().unwrap();
        let mut store = test_version_store(&tmp);
        store.advance("ctx").unwrap();

        assert!(store.remove("ctx").unwrap());
        assert_eq!(store.current_version("ctx"), 0);
        assert!(!store.remove("ctx").unwrap());
    }

    #[test]
    fn test_seal_versioned_roundtrip() {
        let tmp = TempDir::new().unwrap();
        let mut store = test_version_store(&tmp);
        let report = make_test_report();

        let sealed = seal_versioned(
            &report,
            b"secret data",
            "my-context",
            SealingPolicy::default(),
            &mut store,
        )
        .unwrap();

        assert_eq!(sealed.version, 1);
        assert_eq!(sealed.context, "my-context");

        let plaintext = unseal_versioned(&report, &sealed, &mut store).unwrap();
        assert_eq!(plaintext, b"secret data");
    }

    #[test]
    fn test_seal_versioned_increments() {
        let tmp = TempDir::new().unwrap();
        let mut store = test_version_store(&tmp);
        let report = make_test_report();

        let s1 =
            seal_versioned(&report, b"v1", "ctx", SealingPolicy::default(), &mut store).unwrap();
        let s2 =
            seal_versioned(&report, b"v2", "ctx", SealingPolicy::default(), &mut store).unwrap();

        assert_eq!(s1.version, 1);
        assert_eq!(s2.version, 2);
    }

    #[test]
    fn test_unseal_versioned_rollback_rejected() {
        let tmp = TempDir::new().unwrap();
        let mut store = test_version_store(&tmp);
        let report = make_test_report();

        let old =
            seal_versioned(&report, b"old", "ctx", SealingPolicy::default(), &mut store).unwrap();
        let _new =
            seal_versioned(&report, b"new", "ctx", SealingPolicy::default(), &mut store).unwrap();

        // Try to unseal the old blob — should fail (rollback)
        let result = unseal_versioned(&report, &old, &mut store);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("Rollback"));
    }

    #[test]
    fn test_unseal_versioned_same_version_ok() {
        let tmp = TempDir::new().unwrap();
        let mut store = test_version_store(&tmp);
        let report = make_test_report();

        let sealed = seal_versioned(
            &report,
            b"data",
            "ctx",
            SealingPolicy::default(),
            &mut store,
        )
        .unwrap();

        // Unseal same version twice — should work
        let p1 = unseal_versioned(&report, &sealed, &mut store).unwrap();
        let p2 = unseal_versioned(&report, &sealed, &mut store).unwrap();
        assert_eq!(p1, b"data");
        assert_eq!(p2, b"data");
    }

    #[test]
    fn test_versioned_sealed_data_serde() {
        let tmp = TempDir::new().unwrap();
        let mut store = test_version_store(&tmp);
        let report = make_test_report();

        let sealed = seal_versioned(
            &report,
            b"serde-test",
            "ctx",
            SealingPolicy::default(),
            &mut store,
        )
        .unwrap();
        let json = serde_json::to_string(&sealed).unwrap();
        let parsed: VersionedSealedData = serde_json::from_str(&json).unwrap();

        assert_eq!(parsed.version, sealed.version);
        assert_eq!(parsed.context, sealed.context);
        assert_eq!(parsed.blob, sealed.blob);

        let plaintext = unseal_versioned(&report, &parsed, &mut store).unwrap();
        assert_eq!(plaintext, b"serde-test");
    }

    #[test]
    fn test_independent_contexts() {
        let tmp = TempDir::new().unwrap();
        let mut store = test_version_store(&tmp);
        let report = make_test_report();

        let s1 =
            seal_versioned(&report, b"a", "ctx-a", SealingPolicy::default(), &mut store).unwrap();
        let s2 =
            seal_versioned(&report, b"b", "ctx-b", SealingPolicy::default(), &mut store).unwrap();

        // Each context has independent versioning
        assert_eq!(s1.version, 1);
        assert_eq!(s2.version, 1);

        // Both can be unsealed
        assert_eq!(unseal_versioned(&report, &s1, &mut store).unwrap(), b"a");
        assert_eq!(unseal_versioned(&report, &s2, &mut store).unwrap(), b"b");
    }
}
