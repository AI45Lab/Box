//! Cross-process advisory lock for the box state file.

/// RAII exclusive advisory lock guarding `boxes.json` mutations.
///
/// Held for the duration of a [`StateFile::modify`](super::StateFile::modify)
/// (and each [`save`](super::StateFile::save)) so concurrent processes — the
/// `monitor` daemon, `compose`, per-box health checkers, and plain CLI
/// commands — cannot interleave a read-modify-write and clobber each other's
/// fields (`save` rewrites the whole record vector).
///
/// The lock lives on a sibling `boxes.json.lock` file, never on `boxes.json`
/// itself (whose atomic tmp+rename would swap the inode out from under a held
/// lock). `flock` is released automatically when the holder exits or crashes,
/// so a killed monitor/CLI never leaves a stale lock.
pub(crate) struct StateLock {
    #[cfg(unix)]
    _file: std::fs::File,
}

impl StateLock {
    /// Acquire the exclusive advisory lock, blocking until it is available.
    #[cfg(unix)]
    pub(crate) fn acquire() -> std::io::Result<Self> {
        let path = a3s_box_core::dirs_home().join("boxes.json.lock");
        Self::acquire_path(&path)
    }

    #[cfg(unix)]
    fn acquire_path(path: &std::path::Path) -> std::io::Result<Self> {
        use std::os::unix::io::AsRawFd;

        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)?;
        // Blocking exclusive advisory lock; released when `file` drops.
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(Self { _file: file })
    }

    /// Non-Unix fallback: the atomic tmp+rename in `save` still prevents torn
    /// reads; multi-writer concurrency is not a supported Windows scenario.
    #[cfg(not(unix))]
    pub(crate) fn acquire() -> std::io::Result<Self> {
        Ok(Self {})
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::time::Duration;

    #[test]
    fn acquire_path_creates_parent_and_releases_on_drop() {
        let tmp = tempfile::tempdir().unwrap();
        let lock_path = tmp.path().join("state").join("boxes.json.lock");

        let guard = StateLock::acquire_path(&lock_path).unwrap();
        assert!(lock_path.exists());
        drop(guard);

        // Re-acquiring after drop proves the fd-backed flock was released.
        let _guard = StateLock::acquire_path(&lock_path).unwrap();
    }

    #[test]
    fn exclusive_lock_blocks_other_file_descriptors_until_released() {
        let tmp = tempfile::tempdir().unwrap();
        let lock_path = tmp.path().join("boxes.json.lock");
        let guard = StateLock::acquire_path(&lock_path).unwrap();
        let thread_lock_path = lock_path.clone();
        let (tx, rx) = mpsc::channel();

        let waiter = std::thread::spawn(move || {
            let _guard = StateLock::acquire_path(&thread_lock_path).unwrap();
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
