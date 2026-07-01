//! Cross-process advisory file lock for load-modify-save persistence.
//!
//! Several JSON stores (`networks.json`, the OCI `index.json`) are mutated by a
//! read-modify-write: load the whole map, change one entry, write it back. Two
//! processes doing this concurrently lose each other's writes (and, for the
//! network store, allocate duplicate IPs). An atomic tmp+rename only prevents a
//! *torn* read — it does nothing for a lost update. This lock serializes the
//! whole load → mutate → save across processes.

use std::path::Path;

/// RAII exclusive advisory lock keyed on `<target>.lock`.
///
/// The lock lives on a **sibling** `<target>.lock` file, never on `target`
/// itself (whose atomic tmp+rename would swap the inode out from under a held
/// lock). `flock` is released automatically when the holder drops or crashes,
/// so a killed process never leaves a stale lock.
///
/// **Non-reentrant:** do NOT acquire it twice for the same file within one
/// process/task — a second `flock` on a fresh fd blocks on the first and
/// self-deadlocks. Hold a single guard across the entire load → mutate → save
/// (the store's internal `save` must be lock-free for this reason).
pub(crate) struct FileLock {
    #[cfg(unix)]
    _file: std::fs::File,
}

impl FileLock {
    /// Acquire the exclusive advisory lock for `target`, blocking until free.
    #[cfg(unix)]
    pub(crate) fn acquire(target: &Path) -> std::io::Result<Self> {
        use std::os::unix::io::AsRawFd;

        let mut lock_path = target.as_os_str().to_os_string();
        lock_path.push(".lock");
        let lock_path = std::path::PathBuf::from(lock_path);
        if let Some(parent) = lock_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&lock_path)?;
        // Blocking exclusive advisory lock; released when `file` drops.
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(Self { _file: file })
    }

    /// Non-Unix fallback: the atomic tmp+rename in each store's `save` still
    /// prevents torn reads; multi-writer concurrency is not a supported
    /// Windows scenario.
    #[cfg(not(unix))]
    pub(crate) fn acquire(_target: &Path) -> std::io::Result<Self> {
        Ok(Self {})
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::time::Duration;

    #[test]
    fn acquire_creates_sibling_lock_file_and_releases_on_drop() {
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("networks.json");
        let lock_path = tmp.path().join("networks.json.lock");

        let guard = FileLock::acquire(&target).unwrap();
        assert!(lock_path.exists());
        drop(guard);

        // Re-acquiring after drop proves the fd-backed flock was released.
        let _guard = FileLock::acquire(&target).unwrap();
    }

    #[test]
    fn exclusive_lock_blocks_other_file_descriptors_until_released() {
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("index.json");
        let guard = FileLock::acquire(&target).unwrap();
        let thread_target = target.clone();
        let (tx, rx) = mpsc::channel();

        let waiter = std::thread::spawn(move || {
            let _guard = FileLock::acquire(&thread_target).unwrap();
            tx.send(()).unwrap();
        });

        assert!(
            rx.recv_timeout(Duration::from_millis(100)).is_err(),
            "second lock acquisition should block while the first guard is alive"
        );

        drop(guard);
        rx.recv_timeout(Duration::from_secs(2))
            .expect("second lock acquisition should proceed after drop");
        waiter.join().unwrap();
    }
}
