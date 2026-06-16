//! Shared helper for surviving a corrupt on-disk JSON store file.
//!
//! A persisted store (`index.json`, `networks.json`, `volumes.json`) must never
//! be able to brick the whole runtime: a truncated file after a crash, or a
//! schema change in a binary upgrade, should degrade to an empty store the next
//! pull/create repopulates — not a hard error that blocks CRI/CLI startup. This
//! mirrors the CLI's `boxes.json` `parse_or_quarantine` hardening.

use std::path::{Path, PathBuf};

/// Move a corrupt store file aside to a timestamped `*.corrupt-<unix-secs>`
/// sibling so the next save cannot overwrite it (the original is preserved for
/// recovery). Falls back to a copy if rename fails (e.g. cross-device). Returns
/// the backup path on success, `None` if even the copy failed.
pub(crate) fn quarantine_corrupt(path: &Path) -> Option<PathBuf> {
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

/// Render the backup path from [`quarantine_corrupt`] for a log message.
pub(crate) fn quarantine_label(path: &Path) -> String {
    quarantine_corrupt(path)
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "<backup failed>".to_string())
}
