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

/// Copy a store file aside to a timestamped `*.corrupt-<unix-secs>` sibling
/// WITHOUT removing the original.
///
/// Unlike [`quarantine_corrupt`] (whole-file unreadable → move aside), this is
/// for the *per-entry* skip path: the file still parses and its surviving
/// entries stay live, but the next save rewrites it with only the survivors and
/// would erase the un-deserializable entries (e.g. a schema mismatch after an
/// upgrade) with no backup. Copying — not moving — preserves those entries for
/// recovery while leaving the live catalog untouched. Returns the backup path,
/// `None` if the copy failed.
pub(crate) fn quarantine_copy(path: &Path) -> Option<PathBuf> {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let backup = path.with_extension(format!("json.corrupt-{secs}"));
    std::fs::copy(path, &backup).ok().map(|_| backup)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quarantine_copy_preserves_original_and_backs_up() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("index.json");
        std::fs::write(&path, b"original contents").unwrap();

        let backup = quarantine_copy(&path).expect("copy should succeed");

        // The live file is untouched (per-entry skip keeps its survivors).
        assert!(path.exists(), "original must remain in place");
        assert_eq!(std::fs::read(&path).unwrap(), b"original contents");
        // A recovery copy exists with the same bytes.
        assert!(backup.exists(), "backup copy must exist");
        assert_eq!(std::fs::read(&backup).unwrap(), b"original contents");
        assert!(backup.to_string_lossy().contains(".corrupt-"));
    }

    #[test]
    fn quarantine_corrupt_moves_original_aside() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("index.json");
        std::fs::write(&path, b"corrupt").unwrap();

        let backup = quarantine_corrupt(&path).expect("move should succeed");

        // Whole-file path removes the original so the next save can't reuse it.
        assert!(!path.exists(), "original must be moved aside");
        assert!(backup.exists());
        assert_eq!(std::fs::read(&backup).unwrap(), b"corrupt");
    }
}
