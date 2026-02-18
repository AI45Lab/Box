//! Persistent storage for volume configurations.
//!
//! Volumes are stored as JSON in `~/.a3s/volumes.json` with atomic writes
//! (write to tmp file, then rename) to prevent corruption.
//! Volume data is stored under `~/.a3s/volumes/<name>/`.

use a3s_box_core::error::{BoxError, Result};
use a3s_box_core::volume::VolumeConfig;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Persistent store for volume configurations.
#[derive(Debug)]
pub struct VolumeStore {
    /// Path to the JSON file.
    path: PathBuf,
    /// Base directory for volume data (~/.a3s/volumes/).
    volumes_dir: PathBuf,
}

/// Serializable wrapper for the volumes file.
#[derive(Debug, serde::Serialize, serde::Deserialize, Default)]
struct VolumesFile {
    volumes: HashMap<String, VolumeConfig>,
}

impl VolumeStore {
    /// Create a new store at the given path.
    pub fn new(path: impl Into<PathBuf>, volumes_dir: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            volumes_dir: volumes_dir.into(),
        }
    }

    /// Create a store at the default location (`~/.a3s/volumes.json`).
    pub fn default_path() -> Result<Self> {
        let home = dirs_path()?;
        Ok(Self::new(home.join("volumes.json"), home.join("volumes")))
    }

    /// Load all volumes from disk.
    pub fn load(&self) -> Result<HashMap<String, VolumeConfig>> {
        if !self.path.exists() {
            return Ok(HashMap::new());
        }

        let data = std::fs::read_to_string(&self.path).map_err(|e| {
            BoxError::Other(format!(
                "failed to read volumes file {}: {}",
                self.path.display(),
                e
            ))
        })?;

        let file: VolumesFile = serde_json::from_str(&data)
            .map_err(|e| BoxError::Other(format!("failed to parse volumes file: {}", e)))?;

        Ok(file.volumes)
    }

    /// Save all volumes to disk (atomic write).
    pub fn save(&self, volumes: &HashMap<String, VolumeConfig>) -> Result<()> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| {
                BoxError::Other(format!(
                    "failed to create directory {}: {}",
                    parent.display(),
                    e
                ))
            })?;
        }

        let file = VolumesFile {
            volumes: volumes.clone(),
        };

        let json = serde_json::to_string_pretty(&file)
            .map_err(|e| BoxError::Other(format!("failed to serialize volumes: {}", e)))?;

        let tmp_path = self.path.with_extension("json.tmp");
        std::fs::write(&tmp_path, &json).map_err(|e| {
            BoxError::Other(format!(
                "failed to write tmp file {}: {}",
                tmp_path.display(),
                e
            ))
        })?;

        std::fs::rename(&tmp_path, &self.path).map_err(|e| {
            BoxError::Other(format!(
                "failed to rename {} → {}: {}",
                tmp_path.display(),
                self.path.display(),
                e
            ))
        })?;

        Ok(())
    }

    /// Get a single volume by name.
    pub fn get(&self, name: &str) -> Result<Option<VolumeConfig>> {
        let volumes = self.load()?;
        Ok(volumes.get(name).cloned())
    }

    /// Create a new named volume. Returns the host mount point path.
    ///
    /// Creates the volume data directory under `~/.a3s/volumes/<name>/`.
    pub fn create(&self, mut config: VolumeConfig) -> Result<VolumeConfig> {
        let mut volumes = self.load()?;

        if volumes.contains_key(&config.name) {
            return Err(BoxError::Other(format!(
                "volume '{}' already exists",
                config.name
            )));
        }

        // Create volume data directory
        let vol_dir = self.volumes_dir.join(&config.name);
        std::fs::create_dir_all(&vol_dir).map_err(|e| {
            BoxError::Other(format!(
                "failed to create volume directory {}: {}",
                vol_dir.display(),
                e
            ))
        })?;

        config.mount_point = vol_dir.to_string_lossy().to_string();

        volumes.insert(config.name.clone(), config.clone());
        self.save(&volumes)?;
        Ok(config)
    }

    /// Remove a volume by name. Returns error if in use.
    pub fn remove(&self, name: &str, force: bool) -> Result<VolumeConfig> {
        let mut volumes = self.load()?;

        let config = volumes
            .remove(name)
            .ok_or_else(|| BoxError::Other(format!("volume '{}' not found", name)))?;

        if config.is_in_use() && !force {
            // Put it back
            volumes.insert(name.to_string(), config.clone());
            return Err(BoxError::Other(format!(
                "volume '{}' is in use by {} box(es); use --force to remove",
                name,
                config.in_use_by.len()
            )));
        }

        self.save(&volumes)?;

        // Remove volume data directory
        let vol_dir = self.volumes_dir.join(name);
        if vol_dir.exists() {
            std::fs::remove_dir_all(&vol_dir).ok();
        }

        Ok(config)
    }

    /// List all volumes.
    pub fn list(&self) -> Result<Vec<VolumeConfig>> {
        let volumes = self.load()?;
        Ok(volumes.into_values().collect())
    }

    /// Update a volume in-place (used for attach/detach).
    pub fn update(&self, config: &VolumeConfig) -> Result<()> {
        let mut volumes = self.load()?;

        if !volumes.contains_key(&config.name) {
            return Err(BoxError::Other(format!(
                "volume '{}' not found",
                config.name
            )));
        }

        volumes.insert(config.name.clone(), config.clone());
        self.save(&volumes)
    }

    /// Remove all volumes that are not in use. Returns names of removed volumes.
    pub fn prune(&self) -> Result<Vec<String>> {
        let volumes = self.load()?;
        let mut pruned = Vec::new();

        for (name, config) in &volumes {
            if !config.is_in_use() {
                pruned.push(name.clone());
            }
        }

        for name in &pruned {
            self.remove(name, false).ok();
        }

        Ok(pruned)
    }

    /// Get the volume data directory for a named volume.
    pub fn volume_dir(&self, name: &str) -> PathBuf {
        self.volumes_dir.join(name)
    }

    /// Get the store file path.
    pub fn path(&self) -> &Path {
        &self.path
    }
}

/// Get the A3S home directory (~/.a3s).
fn dirs_path() -> Result<PathBuf> {
    let home = std::env::var("HOME")
        .map_err(|_| BoxError::Other("HOME environment variable not set".to_string()))?;
    Ok(PathBuf::from(home).join(".a3s"))
}

impl a3s_box_core::traits::VolumeStoreBackend for VolumeStore {
    fn get(&self, name: &str) -> Result<Option<VolumeConfig>> {
        self.get(name)
    }

    fn create(&self, config: VolumeConfig) -> Result<VolumeConfig> {
        self.create(config)
    }

    fn remove(&self, name: &str, force: bool) -> Result<VolumeConfig> {
        self.remove(name, force)
    }

    fn list(&self) -> Result<Vec<VolumeConfig>> {
        self.list()
    }

    fn update(&self, config: &VolumeConfig) -> Result<()> {
        self.update(config)
    }

    fn prune(&self) -> Result<Vec<String>> {
        self.prune()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_store() -> (tempfile::TempDir, VolumeStore) {
        let dir = tempfile::tempdir().unwrap();
        let store = VolumeStore::new(dir.path().join("volumes.json"), dir.path().join("volumes"));
        (dir, store)
    }

    #[test]
    fn test_load_empty() {
        let (_dir, store) = temp_store();
        let volumes = store.load().unwrap();
        assert!(volumes.is_empty());
    }

    #[test]
    fn test_create_and_load() {
        let (_dir, store) = temp_store();
        let vol = VolumeConfig::new("mydata", "");
        store.create(vol).unwrap();

        let volumes = store.load().unwrap();
        assert_eq!(volumes.len(), 1);
        assert!(volumes.contains_key("mydata"));
    }

    #[test]
    fn test_create_sets_mount_point() {
        let (_dir, store) = temp_store();
        let vol = VolumeConfig::new("mydata", "");
        let created = store.create(vol).unwrap();

        assert!(created.mount_point.contains("mydata"));
        assert!(PathBuf::from(&created.mount_point).exists());
    }

    #[test]
    fn test_create_duplicate() {
        let (_dir, store) = temp_store();
        let v1 = VolumeConfig::new("mydata", "");
        let v2 = VolumeConfig::new("mydata", "");

        store.create(v1).unwrap();
        assert!(store.create(v2).is_err());
    }

    #[test]
    fn test_get_existing() {
        let (_dir, store) = temp_store();
        store.create(VolumeConfig::new("mydata", "")).unwrap();

        let found = store.get("mydata").unwrap();
        assert!(found.is_some());
        assert_eq!(found.unwrap().name, "mydata");
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
        store.create(VolumeConfig::new("mydata", "")).unwrap();

        let removed = store.remove("mydata", false).unwrap();
        assert_eq!(removed.name, "mydata");

        let volumes = store.load().unwrap();
        assert!(volumes.is_empty());
    }

    #[test]
    fn test_remove_nonexistent() {
        let (_dir, store) = temp_store();
        assert!(store.remove("nope", false).is_err());
    }

    #[test]
    fn test_remove_in_use_fails() {
        let (_dir, store) = temp_store();
        let mut vol = VolumeConfig::new("mydata", "");
        vol.attach("box-1");
        // Manually insert since create() doesn't set in_use_by
        let created = store.create(VolumeConfig::new("mydata", "")).unwrap();
        let mut updated = created;
        updated.attach("box-1");
        store.update(&updated).unwrap();

        assert!(store.remove("mydata", false).is_err());
    }

    #[test]
    fn test_remove_in_use_force() {
        let (_dir, store) = temp_store();
        let created = store.create(VolumeConfig::new("mydata", "")).unwrap();
        let mut updated = created;
        updated.attach("box-1");
        store.update(&updated).unwrap();

        let removed = store.remove("mydata", true).unwrap();
        assert_eq!(removed.name, "mydata");
    }

    #[test]
    fn test_list() {
        let (_dir, store) = temp_store();
        store.create(VolumeConfig::new("vol1", "")).unwrap();
        store.create(VolumeConfig::new("vol2", "")).unwrap();

        let list = store.list().unwrap();
        assert_eq!(list.len(), 2);
    }

    #[test]
    fn test_update() {
        let (_dir, store) = temp_store();
        let created = store.create(VolumeConfig::new("mydata", "")).unwrap();

        let mut updated = created;
        updated.attach("box-1");
        store.update(&updated).unwrap();

        let loaded = store.get("mydata").unwrap().unwrap();
        assert_eq!(loaded.in_use_by, vec!["box-1"]);
    }

    #[test]
    fn test_update_nonexistent() {
        let (_dir, store) = temp_store();
        let vol = VolumeConfig::new("nope", "/tmp");
        assert!(store.update(&vol).is_err());
    }

    #[test]
    fn test_prune() {
        let (_dir, store) = temp_store();
        store.create(VolumeConfig::new("unused1", "")).unwrap();
        store.create(VolumeConfig::new("unused2", "")).unwrap();

        let created = store.create(VolumeConfig::new("in_use", "")).unwrap();
        let mut updated = created;
        updated.attach("box-1");
        store.update(&updated).unwrap();

        let pruned = store.prune().unwrap();
        assert_eq!(pruned.len(), 2);
        assert!(pruned.contains(&"unused1".to_string()));
        assert!(pruned.contains(&"unused2".to_string()));

        // in_use should remain
        let remaining = store.list().unwrap();
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].name, "in_use");
    }

    #[test]
    fn test_atomic_write() {
        let (_dir, store) = temp_store();
        store.create(VolumeConfig::new("mydata", "")).unwrap();

        let data = std::fs::read_to_string(store.path()).unwrap();
        let _: serde_json::Value = serde_json::from_str(&data).unwrap();

        let tmp = store.path().with_extension("json.tmp");
        assert!(!tmp.exists());
    }

    #[test]
    fn test_creates_parent_directory() {
        let dir = tempfile::tempdir().unwrap();
        let store = VolumeStore::new(
            dir.path().join("subdir").join("volumes.json"),
            dir.path().join("subdir").join("volumes"),
        );

        store.create(VolumeConfig::new("mydata", "")).unwrap();
        assert!(store.path().exists());
    }

    #[test]
    fn test_remove_cleans_up_directory() {
        let (_dir, store) = temp_store();
        let created = store.create(VolumeConfig::new("mydata", "")).unwrap();
        let vol_dir = PathBuf::from(&created.mount_point);
        assert!(vol_dir.exists());

        store.remove("mydata", false).unwrap();
        assert!(!vol_dir.exists());
    }
}
