//! StateFile persistence layer.

use std::path::{Path, PathBuf};

use super::BoxRecord;
use crate::state::policy::{is_process_alive, should_restart};

/// Persistent state file backed by JSON.
pub struct StateFile {
    path: PathBuf,
    pub(super) records: Vec<BoxRecord>,
}

impl StateFile {
    /// Load state from disk. Creates an empty state if the file doesn't exist.
    pub fn load(path: &Path) -> Result<Self, std::io::Error> {
        if path.exists() {
            let data = std::fs::read_to_string(path)?;
            let records: Vec<BoxRecord> = serde_json::from_str(&data).unwrap_or_default();
            let mut sf = Self {
                path: path.to_path_buf(),
                records,
            };
            sf.reconcile();
            Ok(sf)
        } else {
            // Ensure parent directory exists
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            Ok(Self {
                path: path.to_path_buf(),
                records: Vec::new(),
            })
        }
    }

    /// Load from the default path (~/.a3s/boxes.json).
    pub fn load_default() -> Result<Self, std::io::Error> {
        let home = dirs::home_dir()
            .map(|h| h.join(".a3s"))
            .unwrap_or_else(|| PathBuf::from(".a3s"));
        Self::load(&home.join("boxes.json"))
    }

    /// Save state to disk atomically (write to .tmp, then rename).
    pub fn save(&self) -> Result<(), std::io::Error> {
        let data = serde_json::to_string_pretty(&self.records).map_err(std::io::Error::other)?;
        let tmp_path = self.path.with_extension("json.tmp");
        std::fs::write(&tmp_path, &data)?;
        std::fs::rename(&tmp_path, &self.path)?;
        Ok(())
    }

    /// Add a record and persist.
    pub fn add(&mut self, record: BoxRecord) -> Result<(), std::io::Error> {
        self.records.push(record);
        self.save()
    }

    /// Remove a record by ID and persist.
    pub fn remove(&mut self, id: &str) -> Result<bool, std::io::Error> {
        let len_before = self.records.len();
        self.records.retain(|r| r.id != id);
        if self.records.len() < len_before {
            self.save()?;
            Ok(true)
        } else {
            Ok(false)
        }
    }

    /// Find a record by exact ID.
    pub fn find_by_id(&self, id: &str) -> Option<&BoxRecord> {
        self.records.iter().find(|r| r.id == id)
    }

    /// Find a mutable record by exact ID.
    pub fn find_by_id_mut(&mut self, id: &str) -> Option<&mut BoxRecord> {
        self.records.iter_mut().find(|r| r.id == id)
    }

    /// Find a record by exact name.
    pub fn find_by_name(&self, name: &str) -> Option<&BoxRecord> {
        self.records.iter().find(|r| r.name == name)
    }

    /// Find records matching an ID prefix (must be unique).
    pub fn find_by_id_prefix(&self, prefix: &str) -> Vec<&BoxRecord> {
        self.records
            .iter()
            .filter(|r| r.id.starts_with(prefix) || r.short_id.starts_with(prefix))
            .collect()
    }

    /// List records, optionally filtering to running-only.
    pub fn list(&self, all: bool) -> Vec<&BoxRecord> {
        if all {
            self.records.iter().collect()
        } else {
            self.records
                .iter()
                .filter(|r| r.status == "running")
                .collect()
        }
    }

    /// All records (for iteration).
    pub fn records(&self) -> &[BoxRecord] {
        &self.records
    }

    /// Reconcile: check PID liveness for running boxes, mark dead ones.
    ///
    /// Returns a list of box IDs that should be restarted based on their
    /// restart policy. The caller is responsible for actually restarting them.
    fn reconcile(&mut self) -> Vec<String> {
        let mut changed = false;
        let mut restart_candidates = Vec::new();

        for record in &mut self.records {
            if record.status == "running" {
                if let Some(pid) = record.pid {
                    if !is_process_alive(pid) {
                        record.status = "dead".to_string();
                        record.pid = None;
                        changed = true;

                        if should_restart(record) {
                            restart_candidates.push(record.id.clone());
                        }
                    }
                } else {
                    // Running but no PID — mark as dead
                    record.status = "dead".to_string();
                    changed = true;

                    if should_restart(record) {
                        restart_candidates.push(record.id.clone());
                    }
                }
            }
        }
        if changed {
            let _ = self.save();
        }

        restart_candidates
    }

    /// Get box IDs that are pending restart (dead boxes with active restart policy).
    ///
    /// This can be called after load to check if any boxes need restarting.
    pub fn pending_restarts(&self) -> Vec<String> {
        self.records
            .iter()
            .filter(|r| r.status == "dead" && should_restart(r))
            .map(|r| r.id.clone())
            .collect()
    }

    /// Find all records matching a label key-value pair.
    pub fn find_by_label(&self, key: &str, value: &str) -> Vec<&BoxRecord> {
        self.records
            .iter()
            .filter(|r| r.labels.get(key).is_some_and(|v| v == value))
            .collect()
    }
}
