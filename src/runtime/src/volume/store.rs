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
        let home = a3s_box_core::dirs_home();
        Ok(Self::new(home.join("volumes.json"), home.join("volumes")))
    }

    /// Load all volumes from disk.
    pub fn load(&self) -> Result<HashMap<String, VolumeConfig>> {
        if !self.path.exists() {
            return Ok(HashMap::new());
        }

        let data = std::fs::read_to_string(&self.path).map_err(|e| {
            BoxError::ConfigError(format!(
                "failed to read volumes file {}: {}",
                self.path.display(),
                e
            ))
        })?;

        let file: VolumesFile = serde_json::from_str(&data).map_err(|e| {
            BoxError::SerializationError(format!("failed to parse volumes file: {}", e))
        })?;

        Ok(file.volumes)
    }

    /// Save all volumes to disk (atomic write).
    pub fn save(&self, volumes: &HashMap<String, VolumeConfig>) -> Result<()> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| {
                BoxError::ConfigError(format!(
                    "failed to create directory {}: {}",
                    parent.display(),
                    e
                ))
            })?;
        }

        let file = VolumesFile {
            volumes: volumes.clone(),
        };

        let json = serde_json::to_string_pretty(&file).map_err(|e| {
            BoxError::SerializationError(format!("failed to serialize volumes: {}", e))
        })?;

        let tmp_path = self.path.with_extension("json.tmp");
        std::fs::write(&tmp_path, &json).map_err(|e| {
            BoxError::ConfigError(format!(
                "failed to write tmp file {}: {}",
                tmp_path.display(),
                e
            ))
        })?;

        std::fs::rename(&tmp_path, &self.path).map_err(|e| {
            BoxError::ConfigError(format!(
                "failed to rename {} → {}: {}",
                tmp_path.display(),
                self.path.display(),
                e
            ))
        })?;

        Ok(())
    }

    /// Run `f` over the volume map under a cross-process advisory lock,
    /// re-loading fresh from disk inside the lock and saving the result.
    ///
    /// `create`/`remove`/`update`/`modify`/`get_or_create` all funnel through
    /// here so concurrent `a3s-box` processes cannot lose each other's writes.
    /// The atomic tmp+rename in `save` only prevents a *torn* read — two
    /// processes that both load, mutate a different entry, and save would still
    /// clobber one update (and, for attach/detach, silently drop a volume's
    /// `in_use_by` entry, letting `prune`/`remove` delete data a live box still
    /// has mounted). `save` itself stays lock-free: the guard is held here for
    /// the whole load → mutate → save, and the lock is non-reentrant.
    fn with_write_lock<F, R>(&self, f: F) -> Result<R>
    where
        F: FnOnce(&mut HashMap<String, VolumeConfig>) -> Result<R>,
    {
        let _lock = crate::file_lock::FileLock::acquire(&self.path).map_err(|e| {
            BoxError::ConfigError(format!(
                "failed to lock volumes file {}: {e}",
                self.path.display()
            ))
        })?;
        let mut volumes = self.load()?;
        let r = f(&mut volumes)?;
        self.save(&volumes)?;
        Ok(r)
    }

    /// Get a single volume by name.
    pub fn get(&self, name: &str) -> Result<Option<VolumeConfig>> {
        let volumes = self.load()?;
        Ok(volumes.get(name).cloned())
    }

    /// Create a new named volume. Returns the host mount point path.
    ///
    /// Creates the volume data directory under `~/.a3s/volumes/<name>/`.
    /// Errors if the name already exists (use [`Self::get_or_create`] for the
    /// idempotent auto-create on the `run -v name:/path` path).
    pub fn create(&self, config: VolumeConfig) -> Result<VolumeConfig> {
        self.with_write_lock(|volumes| {
            if volumes.contains_key(&config.name) {
                return Err(BoxError::ConfigError(format!(
                    "volume '{}' already exists",
                    config.name
                )));
            }
            self.materialize(config, volumes)
        })
    }

    /// Return the existing volume, or create it if absent — atomic under the
    /// cross-process lock. Two concurrent first-time `run -v name:/path` then
    /// share one volume instead of one racing to an "already exists" error.
    pub fn get_or_create(&self, config: VolumeConfig) -> Result<VolumeConfig> {
        self.with_write_lock(|volumes| {
            if let Some(existing) = volumes.get(&config.name) {
                return Ok(existing.clone());
            }
            self.materialize(config, volumes)
        })
    }

    /// Create the volume's data directory, set its mount point, and insert it
    /// into `volumes`. Caller must already hold the write lock.
    fn materialize(
        &self,
        mut config: VolumeConfig,
        volumes: &mut HashMap<String, VolumeConfig>,
    ) -> Result<VolumeConfig> {
        let vol_dir = self.volumes_dir.join(&config.name);
        std::fs::create_dir_all(&vol_dir).map_err(|e| {
            BoxError::ConfigError(format!(
                "failed to create volume directory {}: {}",
                vol_dir.display(),
                e
            ))
        })?;
        config.mount_point = vol_dir.to_string_lossy().into_owned();
        volumes.insert(config.name.clone(), config.clone());
        Ok(config)
    }

    /// Remove a volume by name. Returns error if in use.
    pub fn remove(&self, name: &str, force: bool) -> Result<VolumeConfig> {
        let config = self.with_write_lock(|volumes| {
            let config = volumes
                .remove(name)
                .ok_or_else(|| BoxError::ConfigError(format!("volume '{}' not found", name)))?;

            if config.is_in_use() && !force {
                // Put it back
                volumes.insert(name.to_string(), config.clone());
                return Err(BoxError::ConfigError(format!(
                    "volume '{}' is in use by {} box(es); use --force to remove",
                    name,
                    config.in_use_by.len()
                )));
            }
            Ok(config)
        })?;

        // Remove the data directory outside the lock; it is keyed by name and
        // the removal is idempotent.
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

    /// Replace a volume's config wholesale under the cross-process lock.
    ///
    /// For attach/detach prefer [`Self::modify`]: a `get` → mutate → `update`
    /// reads outside the lock and would lose a concurrent update made between
    /// the two calls.
    pub fn update(&self, config: &VolumeConfig) -> Result<()> {
        self.with_write_lock(|volumes| {
            if !volumes.contains_key(&config.name) {
                return Err(BoxError::ConfigError(format!(
                    "volume '{}' not found",
                    config.name
                )));
            }
            volumes.insert(config.name.clone(), config.clone());
            Ok(())
        })
    }

    /// Atomically mutate one volume's config under the cross-process lock.
    ///
    /// Re-reads the current entry inside the lock so concurrent attach/detach
    /// accumulate correctly — the canonical fix for the split `get` → mutate →
    /// `update` race that could drop a volume's `in_use_by` entry. Returns
    /// `false` if the volume does not exist.
    pub fn modify<F>(&self, name: &str, f: F) -> Result<bool>
    where
        F: FnOnce(&mut VolumeConfig),
    {
        self.with_write_lock(|volumes| match volumes.get_mut(name) {
            Some(config) => {
                f(config);
                Ok(true)
            }
            None => Ok(false),
        })
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

    #[test]
    fn test_get_or_create_is_idempotent() {
        let (_dir, store) = temp_store();
        let first = store
            .get_or_create(VolumeConfig::new("shared", ""))
            .unwrap();
        let second = store
            .get_or_create(VolumeConfig::new("shared", ""))
            .unwrap();
        assert_eq!(first.mount_point, second.mount_point);
        assert_eq!(store.list().unwrap().len(), 1);
    }

    #[test]
    fn test_modify_missing_returns_false() {
        let (_dir, store) = temp_store();
        assert!(!store.modify("nope", |c| c.attach("box-1")).unwrap());
    }

    // The advisory lock is per-open-file-description, so separate
    // FileLock::acquire calls serialize even across threads in one process —
    // which is exactly what lets this exercise the lost-update fix in-process.
    #[test]
    fn concurrent_attaches_accumulate_without_lost_update() {
        use std::sync::Arc;
        use std::thread;

        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(VolumeStore::new(
            dir.path().join("volumes.json"),
            dir.path().join("volumes"),
        ));
        store.create(VolumeConfig::new("shared", "")).unwrap();

        let n = 16;
        let handles: Vec<_> = (0..n)
            .map(|i| {
                let store = Arc::clone(&store);
                thread::spawn(move || {
                    store
                        .modify("shared", |c| c.attach(&format!("box-{i}")))
                        .unwrap();
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }

        let cfg = store.get("shared").unwrap().unwrap();
        assert_eq!(
            cfg.in_use_by.len(),
            n,
            "every concurrent attach must persist (no lost update): {:?}",
            cfg.in_use_by
        );
        for i in 0..n {
            assert!(cfg.in_use_by.contains(&format!("box-{i}")));
        }
    }

    #[test]
    fn concurrent_creates_persist_every_volume() {
        use std::sync::Arc;
        use std::thread;

        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(VolumeStore::new(
            dir.path().join("volumes.json"),
            dir.path().join("volumes"),
        ));

        let n = 16;
        let handles: Vec<_> = (0..n)
            .map(|i| {
                let store = Arc::clone(&store);
                thread::spawn(move || {
                    store
                        .create(VolumeConfig::new(&format!("vol-{i}"), ""))
                        .unwrap();
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }

        assert_eq!(
            store.list().unwrap().len(),
            n,
            "every concurrent create must persist (no lost update)"
        );
    }
}
