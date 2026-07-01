//! Durable atomic file writes for persisted state.

use std::io::Write;
use std::path::Path;

/// Atomically and **durably** write `bytes` to `final_path`: write to the
/// caller-chosen `tmp_path` sibling, `fsync` it, rename over `final_path`, then
/// best-effort `fsync` the parent directory.
///
/// Plain `write` + `rename` gives atomicity against a torn read but NOT crash
/// durability: on power loss the rename's directory entry can be journaled while
/// the temp file's data blocks are still buffered (delayed allocation), leaving
/// `final_path` present but zero-length/truncated. That truncated file then
/// fails to parse and gets quarantined — orphaning everything it tracked, the
/// exact outcome the quarantine logic exists to prevent. `fsync`-before-rename
/// closes that window so a hard crash cannot corrupt the persisted state.
///
/// The caller supplies `tmp_path` so it can pick a collision-free name (e.g. a
/// per-process/per-call unique suffix) when concurrent writers are possible.
pub fn write_durable(tmp_path: &Path, final_path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    {
        let mut f = std::fs::File::create(tmp_path)?;
        f.write_all(bytes)?;
        f.sync_all()?;
    }
    std::fs::rename(tmp_path, final_path)?;
    // Best-effort parent-dir fsync so the rename itself is durable. Not every
    // filesystem requires or permits a directory fsync, so failures are ignored.
    if let Some(dir) = final_path.parent() {
        if let Ok(d) = std::fs::File::open(dir) {
            let _ = d.sync_all();
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_durable_round_trips_and_replaces_atomically() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        let tmp = dir.path().join("state.json.tmp");

        write_durable(&tmp, &path, b"hello").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"hello");
        assert!(
            !tmp.exists(),
            "temp file must be renamed away, not left behind"
        );

        // A subsequent write replaces the contents atomically.
        write_durable(&tmp, &path, b"world!!").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"world!!");
        assert!(!tmp.exists());
    }
}
