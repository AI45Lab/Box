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
        let home = a3s_box_core::dirs_home();
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

        // A corrupt/old-schema networks file must not brick the runtime: quarantine
        // it and start from an empty set (create repopulates) rather than failing
        // every network operation. Mirrors the boxes.json hardening.
        let file: NetworksFile = match serde_json::from_str(&data) {
            Ok(f) => f,
            Err(e) => {
                let preserved = crate::store_io::quarantine_label(&self.path);
                tracing::warn!(
                    "networks file {} is corrupt ({e}); preserved a copy at {preserved} \
                     and started from an empty network set",
                    self.path.display(),
                );
                return Ok(HashMap::new());
            }
        };

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

    /// Run `f` against the full networks map under the **cross-process write
    /// lock** (load-fresh → mutate → save). Use this to make a
    /// get-modify-update sequence atomic — e.g. allocate-an-IP-then-connect —
    /// so concurrent boots cannot assign duplicate IPs/MACs or silently lose
    /// each other's endpoints. The map is saved only if `f` returns `Ok`.
    ///
    /// The lock is held across the whole load/mutate/save; `save` is itself
    /// lock-free, so there is no re-entrant `flock` (which would self-deadlock).
    pub fn with_write_lock<F, R, E>(&self, f: F) -> std::result::Result<R, E>
    where
        F: FnOnce(&mut HashMap<String, NetworkConfig>) -> std::result::Result<R, E>,
        E: From<BoxError>,
    {
        let _lock = crate::file_lock::FileLock::acquire(&self.path).map_err(|e| {
            E::from(BoxError::NetworkError(format!(
                "failed to lock networks file {}: {e}",
                self.path.display()
            )))
        })?;
        let mut networks = self.load().map_err(E::from)?;
        let r = f(&mut networks)?;
        self.save(&networks).map_err(E::from)?;
        Ok(r)
    }

    /// Create a new network. Returns error if name already exists.
    pub fn create(&self, config: NetworkConfig) -> Result<()> {
        self.with_write_lock(|networks| {
            if networks.contains_key(&config.name) {
                return Err(BoxError::NetworkError(format!(
                    "network '{}' already exists",
                    config.name
                )));
            }
            networks.insert(config.name.clone(), config);
            Ok(())
        })
    }

    /// Remove a network by name. Returns the removed config or error if not found.
    pub fn remove(&self, name: &str) -> Result<NetworkConfig> {
        self.with_write_lock(|networks| {
            let config = networks
                .remove(name)
                .ok_or_else(|| BoxError::NetworkError(format!("network '{}' not found", name)))?;

            if !config.endpoints.is_empty() {
                // Returning Err skips the save, so the in-memory removal is not
                // persisted — the network stays intact.
                return Err(BoxError::NetworkError(format!(
                    "network '{}' has {} connected endpoint(s); disconnect them first or use --force",
                    name,
                    config.endpoints.len()
                )));
            }
            Ok(config)
        })
    }

    /// List all network names.
    pub fn list(&self) -> Result<Vec<NetworkConfig>> {
        let networks = self.load()?;
        Ok(networks.into_values().collect())
    }

    /// Update a network in-place (used for connect/disconnect).
    ///
    /// Prefer [`with_write_lock`](Self::with_write_lock) for a
    /// get-modify-update sequence: `update` re-loads under the lock, but a
    /// caller that read the network *before* calling `update` decided its
    /// mutation on a possibly-stale snapshot.
    pub fn update(&self, config: &NetworkConfig) -> Result<()> {
        self.with_write_lock(|networks| {
            if !networks.contains_key(&config.name) {
                return Err(BoxError::NetworkError(format!(
                    "network '{}' not found",
                    config.name
                )));
            }
            networks.insert(config.name.clone(), config.clone());
            Ok(())
        })
    }

    /// Get the store file path.
    pub fn path(&self) -> &Path {
        &self.path
    }
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

    #[test]
    fn concurrent_connects_allocate_distinct_ips() {
        use std::collections::HashSet;
        use std::sync::Arc;

        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(NetworkStore::new(dir.path().join("networks.json")));
        store
            .create(NetworkConfig::new("dev", "10.88.0.0/24").unwrap())
            .unwrap();

        // Many threads connect concurrently. with_write_lock must serialize the
        // load → allocate → save so every box gets a distinct IP and no endpoint
        // is lost — the bug allocated duplicate IPs and dropped endpoints.
        let handles: Vec<_> = (0..16)
            .map(|i| {
                let store = Arc::clone(&store);
                std::thread::spawn(move || {
                    store
                        .with_write_lock(|nets| {
                            nets.get_mut("dev")
                                .unwrap()
                                .connect(&format!("box-{i}"), &format!("name-{i}"))
                                .map_err(BoxError::NetworkError)
                        })
                        .unwrap()
                        .ip_address
                })
            })
            .collect();

        let ips: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        let unique: HashSet<_> = ips.iter().collect();
        assert_eq!(
            unique.len(),
            ips.len(),
            "concurrent connects must allocate distinct IPs (got {ips:?})"
        );
        assert_eq!(
            store.get("dev").unwrap().unwrap().endpoints.len(),
            16,
            "every concurrent endpoint must be persisted (no lost writes)"
        );
    }

    #[test]
    fn corrupt_networks_file_is_quarantined_not_fatal() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("networks.json");
        std::fs::write(&path, "{ not valid json").unwrap();
        let store = NetworkStore::new(path.clone());

        // load() must succeed (empty) instead of erroring every network op.
        assert!(store.load().unwrap().is_empty());
        // The corrupt file is preserved as a timestamped sibling, not lost.
        let quarantined = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .any(|e| {
                e.file_name()
                    .to_string_lossy()
                    .contains("networks.json.corrupt-")
            });
        assert!(quarantined, "corrupt networks.json must be quarantined");
    }
}
