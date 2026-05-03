//! Persistent credential store for container registries.
//!
//! Stores per-registry credentials at `~/.a3s/auth/credentials.json`.
//! Uses atomic writes (write tmp, rename) for safety.

use std::collections::HashMap;
use std::path::PathBuf;

use a3s_box_core::error::{BoxError, Result};
use base64::Engine;
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

#[derive(Debug, Default, Deserialize)]
struct DockerConfigFile {
    #[serde(default)]
    auths: HashMap<String, DockerAuthEntry>,
}

#[derive(Debug, Default, Deserialize)]
struct DockerAuthEntry {
    auth: Option<String>,
    username: Option<String>,
    password: Option<String>,
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
        let mut file = self.load()?;
        file.registries.insert(
            a3s_box_core::normalize_registry_server(registry),
            CredentialEntry {
                username: username.to_string(),
                password: password.to_string(),
            },
        );
        self.save(&file)
    }

    /// Get credentials for a registry. Returns `(username, password)`.
    pub fn get(&self, registry: &str) -> Result<Option<(String, String)>> {
        let file = self.load()?;
        Ok(file
            .registries
            .get(&a3s_box_core::normalize_registry_server(registry))
            .map(|e| (e.username.clone(), e.password.clone())))
    }

    /// Remove credentials for a registry. Returns true if entry existed.
    pub fn remove(&self, registry: &str) -> Result<bool> {
        let mut file = self.load()?;
        let removed = file
            .registries
            .remove(&a3s_box_core::normalize_registry_server(registry))
            .is_some();
        if removed {
            self.save(&file)?;
        }
        Ok(removed)
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
}

/// Read-only Docker CLI credential fallback.
///
/// Supports inline `auth` entries and `username`/`password` entries from
/// `$DOCKER_CONFIG/config.json` or `~/.docker/config.json`. External credential
/// helpers are intentionally not invoked.
pub struct DockerConfigCredentialStore {
    path: PathBuf,
}

impl DockerConfigCredentialStore {
    /// Create a Docker config credential reader at the default path.
    pub fn default_path() -> Result<Self> {
        let path = match std::env::var("DOCKER_CONFIG") {
            Ok(dir) => PathBuf::from(dir).join("config.json"),
            Err(_) => dirs::home_dir()
                .unwrap_or_else(|| PathBuf::from("."))
                .join(".docker")
                .join("config.json"),
        };
        Ok(Self { path })
    }

    /// Create a Docker config credential reader at a custom path.
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }

    /// Get Docker CLI credentials for a registry.
    pub fn get(&self, registry: &str) -> Result<Option<(String, String)>> {
        let file = self.load()?;
        let registry = a3s_box_core::normalize_registry_server(registry);

        for (key, entry) in file.auths {
            if a3s_box_core::normalize_registry_server(&key) == registry {
                return decode_docker_auth(entry);
            }
        }

        Ok(None)
    }

    fn load(&self) -> Result<DockerConfigFile> {
        if !self.path.exists() {
            return Ok(DockerConfigFile::default());
        }

        let data = std::fs::read_to_string(&self.path).map_err(|e| {
            BoxError::ConfigError(format!(
                "Failed to read Docker config {}: {}",
                self.path.display(),
                e
            ))
        })?;
        serde_json::from_str(&data).map_err(|e| {
            BoxError::ConfigError(format!(
                "Failed to parse Docker config {}: {}",
                self.path.display(),
                e
            ))
        })
    }
}

fn decode_docker_auth(entry: DockerAuthEntry) -> Result<Option<(String, String)>> {
    if let (Some(username), Some(password)) = (entry.username, entry.password) {
        if !username.is_empty() && !password.is_empty() {
            return Ok(Some((username, password)));
        }
    }

    let Some(auth) = entry.auth else {
        return Ok(None);
    };
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(auth.trim())
        .map_err(|e| BoxError::ConfigError(format!("Failed to decode Docker auth entry: {e}")))?;
    let decoded = String::from_utf8(decoded).map_err(|e| {
        BoxError::ConfigError(format!("Failed to decode Docker auth entry as UTF-8: {e}"))
    })?;
    let Some((username, password)) = decoded.split_once(':') else {
        return Ok(None);
    };
    if username.is_empty() || password.is_empty() {
        Ok(None)
    } else {
        Ok(Some((username.to_string(), password.to_string())))
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

    #[test]
    fn test_docker_config_auth_entry() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("config.json");
        std::fs::write(
            &path,
            r#"{
                "auths": {
                    "https://index.docker.io/v1/": {
                        "auth": "ZG9ja2VyLXVzZXI6ZG9ja2VyLXBhc3M="
                    }
                }
            }"#,
        )
        .unwrap();

        let store = DockerConfigCredentialStore::new(path);
        let creds = store.get("docker.io").unwrap();
        assert_eq!(
            creds,
            Some(("docker-user".to_string(), "docker-pass".to_string()))
        );
    }

    #[test]
    fn test_docker_config_username_password_entry() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("config.json");
        std::fs::write(
            &path,
            r#"{
                "auths": {
                    "registry.example.com": {
                        "username": "alice",
                        "password": "secret"
                    }
                }
            }"#,
        )
        .unwrap();

        let store = DockerConfigCredentialStore::new(path);
        let creds = store.get("registry.example.com").unwrap();
        assert_eq!(creds, Some(("alice".to_string(), "secret".to_string())));
    }
}
