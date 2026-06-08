//! Crash-recovery reaping of orphaned sandbox microVMs.
//!
//! A clean shutdown destroys each VM via its in-memory handle (overlay
//! unmount + box-dir removal). After a crash (`SIGKILL`, OOM, power loss) the
//! CRI process dies but its `a3s-box-shim` microVMs are reparented to `init`
//! and keep running, holding their overlay mounts and box directories. On the
//! next start the CRI has no handle to them, so without this they leak across
//! restarts. [`reap_orphaned_box`] reclaims one such box by id.

#[cfg(target_os = "linux")]
use std::path::Path;

/// Reap an orphaned sandbox microVM left by a previous (crashed) process:
/// kill its `a3s-box-shim`, unmount its overlay, and remove its box directory.
///
/// Idempotent and best-effort: a box with no leftovers (e.g. after a graceful
/// shutdown) is a no-op. Safe to call for every known sandbox id on startup.
#[cfg(target_os = "linux")]
pub fn reap_orphaned_box(box_id: &str) {
    reap_orphaned_box_in(&a3s_box_core::dirs_home(), box_id);
}

/// [`reap_orphaned_box`] against an explicit home directory (for testing).
#[cfg(target_os = "linux")]
fn reap_orphaned_box_in(home_dir: &Path, box_id: &str) {
    let box_dir = home_dir.join("boxes").join(box_id);
    if !box_dir.exists() {
        return;
    }

    let killed = kill_orphaned_shim(box_id);
    // Wait for the killed shim(s) to actually exit before touching the overlay:
    // they hold the merged rootfs, so unmounting/removing it while they are
    // still alive would race the VM's own files.
    wait_for_exit(&killed, std::time::Duration::from_secs(5));

    // Unmount the box overlay; MNT_DETACH (lazy) inside overlay_unmount handles
    // a mount that is somehow still busy.
    let merged = box_dir.join("merged");
    if merged.exists() {
        if let Err(error) = crate::rootfs::overlay::overlay_unmount(&merged) {
            tracing::warn!(
                box_id = %box_id,
                path = %merged.display(),
                error = %error,
                "Failed to unmount orphaned box overlay during crash recovery"
            );
        }
    }

    if let Err(error) = std::fs::remove_dir_all(&box_dir) {
        if error.kind() != std::io::ErrorKind::NotFound {
            tracing::warn!(
                box_id = %box_id,
                path = %box_dir.display(),
                error = %error,
                "Failed to remove orphaned box directory during crash recovery"
            );
        }
    }

    if !killed.is_empty() {
        tracing::info!(box_id = %box_id, "Reaped orphaned sandbox microVM after CRI restart");
    }
}

/// Poll until every pid in `pids` has exited, or `timeout` elapses.
#[cfg(target_os = "linux")]
fn wait_for_exit(pids: &[i32], timeout: std::time::Duration) {
    if pids.is_empty() {
        return;
    }
    // No `Instant::now` budget here (tests stub the clock); bound by iterations.
    let step = std::time::Duration::from_millis(50);
    let mut remaining = (timeout.as_millis() / step.as_millis().max(1)) as u32;
    while remaining > 0 {
        // `kill(pid, 0)` returns ESRCH once the pid is gone (and reaped).
        let any_alive = pids.iter().any(|&pid| unsafe { libc::kill(pid, 0) } == 0);
        if !any_alive {
            return;
        }
        std::thread::sleep(step);
        remaining -= 1;
    }
}

/// Non-Linux builds are development stubs (no microVMs to reap).
#[cfg(not(target_os = "linux"))]
pub fn reap_orphaned_box(_box_id: &str) {}

/// SIGKILL any `a3s-box-shim` process whose command line carries `box_id`.
///
/// The shim is launched as `a3s-box-shim --config '{"box_id":"<id>",...}'`, so
/// matching on both the binary name AND the (UUID) box id scopes the kill to
/// exactly this sandbox's microVM — it can never hit an unrelated process.
#[cfg(target_os = "linux")]
fn kill_orphaned_shim(box_id: &str) -> Vec<i32> {
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return Vec::new();
    };
    let mut killed = Vec::new();
    for entry in entries.flatten() {
        // Only numeric /proc/<pid> entries are processes.
        let name = entry.file_name();
        let Some(pid) = name.to_str().and_then(|s| s.parse::<i32>().ok()) else {
            continue;
        };
        let Ok(cmdline) = std::fs::read(Path::new("/proc").join(name).join("cmdline")) else {
            continue;
        };
        // cmdline is a NUL-separated argv; a plain substring check is enough.
        let cmdline = String::from_utf8_lossy(&cmdline);
        if cmdline.contains("a3s-box-shim") && cmdline.contains(box_id) {
            // SAFETY: kill(2) with a pid we just read from /proc; SIGKILL has no
            // memory effects. The double match (binary + UUID) bounds the target.
            unsafe {
                libc::kill(pid, libc::SIGKILL);
            }
            tracing::info!(box_id = %box_id, pid, "Killed orphaned shim during crash recovery");
            killed.push(pid);
        }
    }
    killed
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;

    #[test]
    fn test_reap_removes_box_dir() {
        // A box dir with no live shim / mount (e.g. left by a crash) is removed.
        let home = tempfile::tempdir().unwrap();
        let box_id = "reap-test-no-such-shim-uuid";
        let box_dir = home.path().join("boxes").join(box_id);
        std::fs::create_dir_all(box_dir.join("logs")).unwrap();
        std::fs::write(box_dir.join("logs/shim.stdout.log"), b"x").unwrap();
        assert!(box_dir.exists());

        reap_orphaned_box_in(home.path(), box_id);
        assert!(!box_dir.exists(), "orphaned box dir should be removed");
    }

    #[test]
    fn test_reap_absent_box_is_noop() {
        let home = tempfile::tempdir().unwrap();
        // No boxes/<id> dir at all — must not panic or error.
        reap_orphaned_box_in(home.path(), "absent-box-uuid");
    }
}
