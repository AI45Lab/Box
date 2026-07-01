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
        use std::os::unix::io::AsRawFd;

        let path = a3s_box_core::dirs_home().join("boxes.json.lock");
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)?;
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
