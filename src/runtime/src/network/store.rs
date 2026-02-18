//! Persistent storage for network configurations.
//!
//! Networks are stored as JSON in `~/.a3s/networks.json` with atomic writes
//! (write to tmp file, then rename) to prevent corruption.

use a3s_box_core::error::{BoxError, Result};
use a3s_box_core::network::NetworkConfig;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Persistent store for network configurations.
#[derive(Debug)]
pub struct NetworkStore {
    /// Path to the JSON file.
    path: PathBuf,
}

/// Serializable wrapper for the networks file.
#[derive(Debug, serde::Serialize, serde::Deserialize, Default)]
struct NetworksFile {
    networks: HashMap<String, NetworkConfig>,
}

impl NetworkStore {
    /// Create a new store at the given path.
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    /// Create a store at the default location (`~/.a3s/networks.json`).
    pub fn default_path() -> Result<Self> {
        let home = dirs_path()?;
        Ok(Self::new(home.join("networks.json")))
    }

    /// Load all networks from disk.
    pub fn load(&self) -> Result<HashMap<String, NetworkConfig>> {
        if !self.path.exists() {
            return Ok(HashMap::new());
        }

        let data = std::fs::read_to_string(&self.path).map_err(|e| {
            BoxError::NetworkError(format!(
                "failed to read networks file {}: {}",
                self.path.display(),
                e
            ))
        })?;

        let file: NetworksFile = serde_json::from_str(&data)
            .map_err(|e| BoxError::NetworkError(format!("failed to parse networks file: {}", e)))?;

        Ok(file.networks)
    }

    /// Save all networks to disk (atomic write).
    pub fn save(&self, networks: &HashMap<String, NetworkConfig>) -> Result<()> {
        // Ensure parent directory exists
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| {
                BoxError::NetworkError(format!(
                    "failed to create directory {}: {}",
                    parent.display(),
                    e
                ))
            })?;
        }

        let file = NetworksFile {
            networks: networks.clone(),
        };

        let json = serde_json::to_string_pretty(&file)
            .map_err(|e| BoxError::NetworkError(format!("failed to serialize networks: {}", e)))?;

        // Atomic write: write to tmp, then rename
        let tmp_path = self.path.with_extension("json.tmp");
        std::fs::write(&tmp_path, &json).map_err(|e| {
            BoxError::NetworkError(format!(
                "failed to write tmp file {}: {}",
                tmp_path.display(),
                e
            ))
        })?;

        std::fs::rename(&tmp_path, &self.path).map_err(|e| {
            BoxError::NetworkError(format!(
                "failed to rename {} → {}: {}",
                tmp_path.display(),
                self.path.display(),
                e
            ))
        })?;

        Ok(())
    }

    /// Get a single network by name.
    pub fn get(&self, name: &str) -> Result<Option<NetworkConfig>> {
        let networks = self.load()?;
        Ok(networks.get(name).cloned())
    }

    /// Create a new network. Returns error if name already exists.
    pub fn create(&self, config: NetworkConfig) -> Result<()> {
        let mut networks = self.load()?;

        if networks.contains_key(&config.name) {
            return Err(BoxError::NetworkError(format!(
                "network '{}' already exists",
                config.name
            )));
        }

        networks.insert(config.name.clone(), config);
        self.save(&networks)
    }

    /// Remove a network by name. Returns the removed config or error if not found.
    pub fn remove(&self, name: &str) -> Result<NetworkConfig> {
        let mut networks = self.load()?;

        let config = networks
            .remove(name)
            .ok_or_else(|| BoxError::NetworkError(format!("network '{}' not found", name)))?;

        if !config.endpoints.is_empty() {
            return Err(BoxError::NetworkError(format!(
                "network '{}' has {} connected endpoint(s); disconnect them first or use --force",
                name,
                config.endpoints.len()
            )));
        }

        self.save(&networks)?;
        Ok(config)
    }

    /// List all network names.
    pub fn list(&self) -> Result<Vec<NetworkConfig>> {
        let networks = self.load()?;
        Ok(networks.into_values().collect())
    }

    /// Update a network in-place (used for connect/disconnect).
    pub fn update(&self, config: &NetworkConfig) -> Result<()> {
        let mut networks = self.load()?;

        if !networks.contains_key(&config.name) {
            return Err(BoxError::NetworkError(format!(
                "network '{}' not found",
                config.name
            )));
        }

        networks.insert(config.name.clone(), config.clone());
        self.save(&networks)
    }

    /// Get the store file path.
    pub fn path(&self) -> &Path {
        &self.path
    }
}

/// Get the A3S home directory (~/.a3s).
fn dirs_path() -> Result<PathBuf> {
    let home = std::env::var("HOME")
        .map_err(|_| BoxError::NetworkError("HOME environment variable not set".to_string()))?;
    Ok(PathBuf::from(home).join(".a3s"))
}

impl a3s_box_core::traits::NetworkStoreBackend for NetworkStore {
    fn get(&self, name: &str) -> Result<Option<NetworkConfig>> {
        self.get(name)
    }

    fn create(&self, config: NetworkConfig) -> Result<()> {
        self.create(config)
    }

    fn remove(&self, name: &str) -> Result<NetworkConfig> {
        self.remove(name)
    }

    fn list(&self) -> Result<Vec<NetworkConfig>> {
        self.list()
    }

    fn update(&self, config: &NetworkConfig) -> Result<()> {
        self.update(config)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_store() -> (tempfile::TempDir, NetworkStore) {
        let dir = tempfile::tempdir().unwrap();
        let store = NetworkStore::new(dir.path().join("networks.json"));
        (dir, store)
    }

    #[test]
    fn test_load_empty() {
        let (_dir, store) = temp_store();
        let networks = store.load().unwrap();
        assert!(networks.is_empty());
    }

    #[test]
    fn test_create_and_load() {
        let (_dir, store) = temp_store();
        let net = NetworkConfig::new("mynet", "10.88.0.0/24").unwrap();
        store.create(net).unwrap();

        let networks = store.load().unwrap();
        assert_eq!(networks.len(), 1);
        assert!(networks.contains_key("mynet"));
    }

    #[test]
    fn test_create_duplicate() {
        let (_dir, store) = temp_store();
        let net1 = NetworkConfig::new("mynet", "10.88.0.0/24").unwrap();
        let net2 = NetworkConfig::new("mynet", "10.89.0.0/24").unwrap();

        store.create(net1).unwrap();
        assert!(store.create(net2).is_err());
    }

    #[test]
    fn test_get_existing() {
        let (_dir, store) = temp_store();
        let net = NetworkConfig::new("mynet", "10.88.0.0/24").unwrap();
        store.create(net).unwrap();

        let found = store.get("mynet").unwrap();
        assert!(found.is_some());
        assert_eq!(found.unwrap().name, "mynet");
    }

    #[test]
    fn test_get_nonexistent() {
        let (_dir, store) = temp_store();
        let found = store.get("nope").unwrap();
        assert!(found.is_none());
    }

    #[test]
    fn test_remove() {
        let (_dir, store) = temp_store();
        let net = NetworkConfig::new("mynet", "10.88.0.0/24").unwrap();
        store.create(net).unwrap();

        let removed = store.remove("mynet").unwrap();
        assert_eq!(removed.name, "mynet");

        let networks = store.load().unwrap();
        assert!(networks.is_empty());
    }

    #[test]
    fn test_remove_nonexistent() {
        let (_dir, store) = temp_store();
        assert!(store.remove("nope").is_err());
    }

    #[test]
    fn test_remove_with_endpoints() {
        let (_dir, store) = temp_store();
        let mut net = NetworkConfig::new("mynet", "10.88.0.0/24").unwrap();
        net.connect("box-1", "web").unwrap();
        store.create(net).unwrap();

        // Should fail because endpoints are connected
        assert!(store.remove("mynet").is_err());
    }

    #[test]
    fn test_list() {
        let (_dir, store) = temp_store();
        store
            .create(NetworkConfig::new("net1", "10.88.0.0/24").unwrap())
            .unwrap();
        store
            .create(NetworkConfig::new("net2", "10.89.0.0/24").unwrap())
            .unwrap();

        let list = store.list().unwrap();
        assert_eq!(list.len(), 2);
    }

    #[test]
    fn test_update() {
        let (_dir, store) = temp_store();
        let net = NetworkConfig::new("mynet", "10.88.0.0/24").unwrap();
        store.create(net).unwrap();

        // Connect a box
        let mut net = store.get("mynet").unwrap().unwrap();
        net.connect("box-1", "web").unwrap();
        store.update(&net).unwrap();

        // Verify persistence
        let loaded = store.get("mynet").unwrap().unwrap();
        assert_eq!(loaded.endpoints.len(), 1);
    }

    #[test]
    fn test_update_nonexistent() {
        let (_dir, store) = temp_store();
        let net = NetworkConfig::new("nope", "10.88.0.0/24").unwrap();
        assert!(store.update(&net).is_err());
    }

    #[test]
    fn test_atomic_write() {
        let (_dir, store) = temp_store();
        let net = NetworkConfig::new("mynet", "10.88.0.0/24").unwrap();
        store.create(net).unwrap();

        // Verify the file exists and is valid JSON
        let data = std::fs::read_to_string(store.path()).unwrap();
        let _: serde_json::Value = serde_json::from_str(&data).unwrap();

        // Verify no tmp file left behind
        let tmp = store.path().with_extension("json.tmp");
        assert!(!tmp.exists());
    }

    #[test]
    fn test_creates_parent_directory() {
        let dir = tempfile::tempdir().unwrap();
        let store = NetworkStore::new(dir.path().join("subdir").join("networks.json"));

        let net = NetworkConfig::new("mynet", "10.88.0.0/24").unwrap();
        store.create(net).unwrap();

        assert!(store.path().exists());
    }
}
