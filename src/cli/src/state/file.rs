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
            let records = Self::parse_or_quarantine(path, &data);
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
        let home = a3s_box_core::dirs_home();
        Self::load(&home.join("boxes.json"))
    }

    /// Load the default state WITHOUT the reconcile sweep (PID-liveness checks +
    /// cleanup over every record). The append hot path (box registration) only adds
    /// a record, so reconciling every *other* box under the global lock is pure
    /// overhead — and under a high-concurrency fork burst it makes registration
    /// O(N²) serialized syscalls. Reconcile still runs on every `list`/status load
    /// and in the monitor, so liveness/exit-code/restart handling is not lost.
    fn load_default_raw() -> Result<Self, std::io::Error> {
        let home = a3s_box_core::dirs_home();
        let path = home.join("boxes.json");
        if path.exists() {
            let data = std::fs::read_to_string(&path)?;
            let records = Self::parse_or_quarantine(&path, &data);
            Ok(Self { path, records })
        } else {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            Ok(Self {
                path,
                records: Vec::new(),
            })
        }
    }

    /// Parse box records from an existing state file's contents.
    ///
    /// On a parse failure the corrupt file is NOT silently discarded. Discarding
    /// it would let the next `write_to_disk` overwrite `boxes.json` with `[]`,
    /// orphaning every running VM/overlay with no error and no recoverable
    /// record. Instead the corrupt file is quarantined to a timestamped sibling
    /// (`boxes.json.corrupt-<unix-secs>`) and a loud warning is emitted, so the
    /// data is preserved for recovery (`a3s-box reap`, or manual repair) while
    /// the process starts from a clean empty state rather than crashing.
    fn parse_or_quarantine(path: &Path, data: &str) -> Vec<BoxRecord> {
        match serde_json::from_str::<Vec<BoxRecord>>(data) {
            Ok(records) => records,
            Err(err) => {
                let preserved = Self::quarantine_corrupt_file(path)
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|| "<backup failed>".to_string());
                eprintln!(
                    "a3s-box: WARNING: state file {} is corrupt ({err}); preserved a \
                     copy at {preserved} and started from empty state. Running boxes are \
                     no longer tracked — recover from the backup or run `a3s-box reap`.",
                    path.display(),
                );
                Vec::new()
            }
        }
    }

    /// Move a corrupt state file aside to a timestamped sibling so it is not
    /// overwritten by the next save. Falls back to a copy if rename fails
    /// (e.g. cross-device). Returns the backup path on success.
    fn quarantine_corrupt_file(path: &Path) -> Option<PathBuf> {
        let secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let backup = path.with_extension(format!("json.corrupt-{secs}"));
        if std::fs::rename(path, &backup).is_ok() {
            return Some(backup);
        }
        match std::fs::copy(path, &backup) {
            Ok(_) => Some(backup),
            Err(_) => None,
        }
    }

    /// Save state to disk atomically under the cross-process state lock.
    pub fn save(&self) -> Result<(), std::io::Error> {
        let _lock = super::lock::StateLock::acquire()?;
        self.write_to_disk()
    }

    /// Atomic write (tmp + rename) WITHOUT taking the state lock. Callers that
    /// already hold the lock (`save`, `modify`, and `reconcile` which runs
    /// inside `load`) use this to avoid re-locking (`flock` is not reentrant).
    fn write_to_disk(&self) -> Result<(), std::io::Error> {
        let data = serde_json::to_string_pretty(&self.records).map_err(std::io::Error::other)?;
        let tmp_path = self.path.with_extension("json.tmp");
        std::fs::write(&tmp_path, &data)?;
        std::fs::rename(&tmp_path, &self.path)?;
        Ok(())
    }

    /// Atomically apply `f` to the on-disk state under the exclusive
    /// cross-process lock: load fresh → mutate → save, all while the lock is
    /// held. This is the race-free read-modify-write primitive — every writer
    /// should mutate through it (or, for async work, snapshot inputs before the
    /// await and call `modify` afterward to re-apply only its owned fields), so
    /// the monitor/compose/health/CLI cannot clobber each other.
    ///
    /// `f` MUST be synchronous and MUST NOT `.await` (holding an OS lock across
    /// a task yield would serialize or deadlock the async runtime).
    pub fn modify<R, E>(f: impl FnOnce(&mut StateFile) -> Result<R, E>) -> Result<R, E>
    where
        E: From<std::io::Error>,
    {
        let _lock = super::lock::StateLock::acquire()?;
        let mut sf = Self::load_default()?;
        let out = f(&mut sf)?;
        sf.write_to_disk()?;
        Ok(out)
    }

    /// Append a record atomically under the state lock (load fresh → push →
    /// save). Use this instead of `load_default()? + add()` so concurrent
    /// appends/removals cannot lose records. Loads WITHOUT the reconcile sweep —
    /// appending a box must not pay an O(N) PID-liveness/cleanup pass over every
    /// other box (the high-concurrency fork bottleneck).
    pub fn add_record(record: BoxRecord) -> Result<(), std::io::Error> {
        let _lock = super::lock::StateLock::acquire()?;
        let mut sf = Self::load_default_raw()?;
        sf.records.push(record);
        sf.write_to_disk()
    }

    /// Remove a record by id atomically under the state lock. Returns whether a
    /// record was removed.
    pub fn remove_record(id: &str) -> Result<bool, std::io::Error> {
        Self::modify(|sf| {
            let before = sf.records.len();
            sf.records.retain(|r| r.id != id);
            Ok::<bool, std::io::Error>(sf.records.len() < before)
        })
    }

    /// Add a record and persist.
    pub fn add(&mut self, record: BoxRecord) -> Result<(), std::io::Error> {
        self.records.push(record);
        self.save()
    }

    /// Drop a record from this in-memory handle WITHOUT persisting.
    ///
    /// Used by callers that already removed the record from disk atomically via
    /// [`remove_record`](Self::remove_record); this keeps their in-memory view
    /// consistent without a second `save` that would clobber concurrent writers.
    pub(crate) fn forget(&mut self, id: &str) {
        self.records.retain(|r| r.id != id);
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

    /// Reconcile: check PID liveness for active boxes, mark dead ones.
    ///
    /// Returns a list of box IDs that should be restarted based on their
    /// restart policy. The caller is responsible for actually restarting them.
    fn reconcile(&mut self) -> Vec<String> {
        let mut changed = false;
        let mut restart_candidates = Vec::new();
        let mut auto_remove_records = Vec::new();
        let mut stopped_resource_records = Vec::new();

        for record in &mut self.records {
            if !matches!(record.status.as_str(), "running" | "paused") {
                continue;
            }

            let has_live_pid = record.pid.is_some_and(is_process_alive);
            if !has_live_pid {
                // guest-init writes the container exit code into the overlay
                // rootfs (`/.a3s_exit_code`) on exit; it surfaces on the host at
                // <box_dir>/upper/.a3s_exit_code. Capture it here so a detached
                // box's `wait`/`inspect` report the real code — libkrun's
                // start_enter takeover means we can't waitpid the VM, so liveness
                // polling alone would otherwise always yield exit 0.
                if record.exit_code.is_none() {
                    if let Ok(contents) =
                        std::fs::read_to_string(record.box_dir.join("upper").join(".a3s_exit_code"))
                    {
                        if let Ok(code) = contents.trim().parse::<i32>() {
                            record.exit_code = Some(code);
                        }
                    }
                }
                record.status = "dead".to_string();
                record.pid = None;
                record.health_status = "none".to_string();
                record.health_retries = 0;
                changed = true;

                if record.auto_remove {
                    auto_remove_records.push(record.clone());
                    continue;
                }

                stopped_resource_records.push(record.clone());

                if should_restart(record) {
                    restart_candidates.push(record.id.clone());
                }
            }
        }

        for record in &stopped_resource_records {
            crate::cleanup::cleanup_stopped_box(record);
        }

        if !auto_remove_records.is_empty() {
            for record in &auto_remove_records {
                crate::cleanup::cleanup_removed_box(record);
            }
            self.records
                .retain(|record| !auto_remove_records.iter().any(|r| r.id == record.id));
            changed = true;
        }

        if changed {
            // reconcile runs inside `load`, which `modify` calls while holding
            // the state lock; use the unlocked write to avoid re-locking.
            let _ = self.write_to_disk();
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
