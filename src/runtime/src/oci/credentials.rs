//! Persistent credential store for container registries.
//!
//! Stores per-registry credentials at `~/.a3s/auth/credentials.json`.
//! Uses atomic writes (write tmp, rename) for safety.

use std::collections::HashMap;
use std::path::PathBuf;

use a3s_box_core::error::{BoxError, Result};
use serde::{Deserialize, Serialize};

/// Per-registry credential entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct CredentialEntry {
    username: String,
    password: String,
}

/// Persistent credential file format.
#[derive(Debug, Default, Serialize, Deserialize)]
struct CredentialFile {
    registries: HashMap<String, CredentialEntry>,
}

/// Persistent credential store for container registries.
///
/// Stores credentials at `~/.a3s/auth/credentials.json`.
pub struct CredentialStore {
    path: PathBuf,
}

impl CredentialStore {
    /// Create a credential store at the default path (`~/.a3s/auth/credentials.json`).
    pub fn default_path() -> Result<Self> {
        Ok(Self {
            path: a3s_box_core::dirs_home()
                .join("auth")
                .join("credentials.json"),
        })
    }

    /// Create a credential store at a custom path.
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }

    /// Store credentials for a registry. Overwrites existing entry.
    pub fn store(&self, registry: &str, username: &str, password: &str) -> Result<()> {
        self.with_write_lock(|file| {
            file.registries.insert(
                normalize_registry(registry),
                CredentialEntry {
                    username: username.to_string(),
                    password: password.to_string(),
                },
            );
            Ok(())
        })
    }

    /// Get credentials for a registry. Returns `(username, password)`.
    pub fn get(&self, registry: &str) -> Result<Option<(String, String)>> {
        let file = self.load()?;
        Ok(file
            .registries
            .get(&normalize_registry(registry))
            .map(|e| (e.username.clone(), e.password.clone())))
    }

    /// Remove credentials for a registry. Returns true if entry existed.
    pub fn remove(&self, registry: &str) -> Result<bool> {
        self.with_write_lock(|file| {
            Ok(file
                .registries
                .remove(&normalize_registry(registry))
                .is_some())
        })
    }

    /// List all registries with stored credentials.
    pub fn list_registries(&self) -> Result<Vec<String>> {
        let file = self.load()?;
        let mut registries: Vec<String> = file.registries.keys().cloned().collect();
        registries.sort();
        Ok(registries)
    }

    /// Load the credential file from disk. Returns empty if not found.
    fn load(&self) -> Result<CredentialFile> {
        if !self.path.exists() {
            return Ok(CredentialFile::default());
        }
        let data = std::fs::read_to_string(&self.path).map_err(|e| {
            BoxError::ConfigError(format!(
                "Failed to read credential store {}: {}",
                self.path.display(),
                e
            ))
        })?;
        serde_json::from_str(&data).map_err(|e| {
            BoxError::ConfigError(format!(
                "Failed to parse credential store {}: {}",
                self.path.display(),
                e
            ))
        })
    }

    /// Save the credential file to disk atomically (write tmp, rename).
    fn save(&self, file: &CredentialFile) -> Result<()> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| {
                BoxError::ConfigError(format!(
                    "Failed to create credential store directory {}: {}",
                    parent.display(),
                    e
                ))
            })?;
        }

        let tmp_path = self.path.with_extension("tmp");
        let data = serde_json::to_string_pretty(file)?;
        std::fs::write(&tmp_path, &data).map_err(|e| {
            BoxError::ConfigError(format!(
                "Failed to write credential store {}: {}",
                tmp_path.display(),
                e
            ))
        })?;
        std::fs::rename(&tmp_path, &self.path).map_err(|e| {
            BoxError::ConfigError(format!(
                "Failed to rename credential store {} -> {}: {}",
                tmp_path.display(),
                self.path.display(),
                e
            ))
        })?;
        Ok(())
    }

    /// Run `f` over the credential file under a cross-process advisory lock,
    /// re-loading fresh inside the lock and saving the result.
    ///
    /// `store`/`remove` funnel through here so two concurrent `a3s-box login`
    /// processes cannot lose each other's entry: the atomic tmp+rename in
    /// `save` only prevents a torn read, and `save` even uses a fixed `.tmp`
    /// name that concurrent writers would collide on. `save` stays lock-free —
    /// the guard is held here across the whole load → mutate → save and is
    /// non-reentrant.
    fn with_write_lock<F, R>(&self, f: F) -> Result<R>
    where
        F: FnOnce(&mut CredentialFile) -> Result<R>,
    {
        let _lock = crate::file_lock::FileLock::acquire(&self.path).map_err(|e| {
            BoxError::ConfigError(format!(
                "Failed to lock credential store {}: {e}",
                self.path.display()
            ))
        })?;
        let mut file = self.load()?;
        let r = f(&mut file)?;
        self.save(&file)?;
        Ok(r)
    }
}

/// Normalize registry names (e.g., "docker.io" and "index.docker.io" → "index.docker.io").
fn normalize_registry(registry: &str) -> String {
    let r = registry.trim().to_lowercase();
    if r == "docker.io" || r == "registry-1.docker.io" {
        "index.docker.io".to_string()
    } else {
        r
    }
}

impl a3s_box_core::traits::CredentialProvider for CredentialStore {
    fn get(&self, registry: &str) -> Result<Option<(String, String)>> {
        self.get(registry)
    }

    fn store(&self, registry: &str, username: &str, password: &str) -> Result<()> {
        self.store(registry, username, password)
    }

    fn remove(&self, registry: &str) -> Result<bool> {
        self.remove(registry)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn test_store(dir: &TempDir) -> CredentialStore {
        CredentialStore::new(dir.path().join("credentials.json"))
    }

    // The advisory lock is per-open-file-description, so separate
    // FileLock::acquire calls serialize even across threads in one process —
    // which lets this exercise the lost-update fix in-process.
    #[test]
    fn concurrent_logins_to_distinct_registries_no_lost_update() {
        use std::sync::Arc;
        use std::thread;

        let dir = TempDir::new().unwrap();
        let store = Arc::new(test_store(&dir));

        let n = 16;
        let handles: Vec<_> = (0..n)
            .map(|i| {
                let store = Arc::clone(&store);
                thread::spawn(move || {
                    store
                        .store(&format!("reg{i}.example.com"), "user", "pass")
                        .unwrap();
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }

        assert_eq!(
            store.list_registries().unwrap().len(),
            n,
            "every concurrent login must persist (no lost update)"
        );
    }

    #[test]
    fn test_store_and_get() {
        let dir = TempDir::new().unwrap();
        let store = test_store(&dir);

        store.store("ghcr.io", "user1", "pass1").unwrap();
        let creds = store.get("ghcr.io").unwrap();
        assert_eq!(creds, Some(("user1".to_string(), "pass1".to_string())));
    }

    #[test]
    fn test_get_nonexistent() {
        let dir = TempDir::new().unwrap();
        let store = test_store(&dir);

        let creds = store.get("ghcr.io").unwrap();
        assert_eq!(creds, None);
    }

    #[test]
    fn test_overwrite_existing() {
        let dir = TempDir::new().unwrap();
        let store = test_store(&dir);

        store.store("ghcr.io", "user1", "pass1").unwrap();
        store.store("ghcr.io", "user2", "pass2").unwrap();
        let creds = store.get("ghcr.io").unwrap();
        assert_eq!(creds, Some(("user2".to_string(), "pass2".to_string())));
    }

    #[test]
    fn test_remove() {
        let dir = TempDir::new().unwrap();
        let store = test_store(&dir);

        store.store("ghcr.io", "user1", "pass1").unwrap();
        assert!(store.remove("ghcr.io").unwrap());
        assert_eq!(store.get("ghcr.io").unwrap(), None);
    }

    #[test]
    fn test_remove_nonexistent() {
        let dir = TempDir::new().unwrap();
        let store = test_store(&dir);

        assert!(!store.remove("ghcr.io").unwrap());
    }

    #[test]
    fn test_list_registries() {
        let dir = TempDir::new().unwrap();
        let store = test_store(&dir);

        store.store("ghcr.io", "u1", "p1").unwrap();
        store.store("quay.io", "u2", "p2").unwrap();
        let registries = store.list_registries().unwrap();
        assert_eq!(registries, vec!["ghcr.io", "quay.io"]);
    }

    #[test]
    fn test_list_empty() {
        let dir = TempDir::new().unwrap();
        let store = test_store(&dir);

        let registries = store.list_registries().unwrap();
        assert!(registries.is_empty());
    }

    #[test]
    fn test_docker_io_normalization() {
        let dir = TempDir::new().unwrap();
        let store = test_store(&dir);

        store.store("docker.io", "user", "pass").unwrap();
        // All Docker Hub aliases should resolve to the same entry
        let creds = store.get("index.docker.io").unwrap();
        assert_eq!(creds, Some(("user".to_string(), "pass".to_string())));

        let creds = store.get("registry-1.docker.io").unwrap();
        assert_eq!(creds, Some(("user".to_string(), "pass".to_string())));
    }

    #[test]
    fn test_persistence() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("credentials.json");

        // Store with one instance
        let store1 = CredentialStore::new(path.clone());
        store1.store("ghcr.io", "user", "pass").unwrap();

        // Read with a new instance
        let store2 = CredentialStore::new(path);
        let creds = store2.get("ghcr.io").unwrap();
        assert_eq!(creds, Some(("user".to_string(), "pass".to_string())));
    }

    #[test]
    fn test_multiple_registries() {
        let dir = TempDir::new().unwrap();
        let store = test_store(&dir);

        store.store("ghcr.io", "u1", "p1").unwrap();
        store.store("quay.io", "u2", "p2").unwrap();
        store.store("ecr.aws", "u3", "p3").unwrap();

        assert_eq!(
            store.get("ghcr.io").unwrap(),
            Some(("u1".to_string(), "p1".to_string()))
        );
        assert_eq!(
            store.get("quay.io").unwrap(),
            Some(("u2".to_string(), "p2".to_string()))
        );
        assert_eq!(
            store.get("ecr.aws").unwrap(),
            Some(("u3".to_string(), "p3".to_string()))
        );
    }
}
