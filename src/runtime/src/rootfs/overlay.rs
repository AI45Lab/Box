//! Overlayfs mount/unmount operations.
//!
//! Provides host-side overlayfs mounts for CoW rootfs. On Linux 5.11+,
//! unprivileged overlayfs is available in user namespaces. Falls back to
//! `mount(2)` syscall or `mount` command.

use std::path::Path;

use a3s_box_core::error::{BoxError, Result};

/// Mount an overlayfs at `merged` with `lower` (read-only), `upper` (writes), `work`.
///
/// Tries in order:
/// 1. `mount(2)` syscall (requires CAP_SYS_ADMIN or unprivileged overlay)
/// 2. `mount` command as fallback
pub fn overlay_mount(lower: &Path, upper: &Path, work: &Path, merged: &Path) -> Result<()> {
    // overlayfs mount options are comma-delimited with no escaping, so a comma in
    // any path would be parsed as an option boundary and silently corrupt the
    // mount. Refuse instead. Box dirs are UUID-based today, so this never trips in
    // practice — it's a guard for any future user-controllable cache/box path.
    for path in [lower, upper, work] {
        if path.to_string_lossy().contains(',') {
            return Err(BoxError::BuildError(format!(
                "overlay path contains a comma, which overlayfs options cannot express: {}",
                path.display()
            )));
        }
    }

    let opts = format!(
        "lowerdir={},upperdir={},workdir={}",
        lower.display(),
        upper.display(),
        work.display()
    );

    // Try mount(2) syscall first
    #[cfg(target_os = "linux")]
    {
        use std::ffi::CString;

        let source = CString::new("overlay").unwrap();
        let target = CString::new(merged.to_string_lossy().as_ref())
            .map_err(|e| BoxError::BuildError(format!("Invalid merged path for mount: {}", e)))?;
        let fstype = CString::new("overlay").unwrap();
        let data = CString::new(opts.as_str())
            .map_err(|e| BoxError::BuildError(format!("Invalid overlay mount options: {}", e)))?;

        let ret = unsafe {
            libc::mount(
                source.as_ptr(),
                target.as_ptr(),
                fstype.as_ptr(),
                0,
                data.as_ptr() as *const libc::c_void,
            )
        };

        if ret == 0 {
            tracing::debug!(
                lower = %lower.display(),
                merged = %merged.display(),
                "Overlay mounted via mount(2)"
            );
            return Ok(());
        }

        let errno = std::io::Error::last_os_error();
        tracing::debug!(
            error = %errno,
            "mount(2) failed, trying mount command"
        );

        // Fallback: try `mount` command
        let status = std::process::Command::new("mount")
            .args(["-t", "overlay", "overlay", "-o", &opts])
            .arg(merged)
            .status()
            .map_err(|e| BoxError::BuildError(format!("Failed to run mount command: {}", e)))?;

        if status.success() {
            tracing::debug!(
                lower = %lower.display(),
                merged = %merged.display(),
                "Overlay mounted via mount command"
            );
            return Ok(());
        }

        Err(BoxError::BuildError(format!(
            "Failed to mount overlayfs at {}: mount(2) returned {} and mount command exited with {}",
            merged.display(),
            errno,
            status
        )))
    }

    #[cfg(not(target_os = "linux"))]
    {
        let _ = (lower, upper, work, merged, opts);
        Err(BoxError::BuildError(
            "Overlayfs is only supported on Linux".to_string(),
        ))
    }
}

/// Unmount an overlayfs at `merged`.
pub fn overlay_unmount(merged: &Path) -> Result<()> {
    #[cfg(target_os = "linux")]
    {
        use std::ffi::CString;

        let target = CString::new(merged.to_string_lossy().as_ref())
            .map_err(|e| BoxError::BuildError(format!("Invalid path for umount: {}", e)))?;

        let ret = unsafe { libc::umount2(target.as_ptr(), libc::MNT_DETACH) };

        if ret == 0 {
            tracing::debug!(path = %merged.display(), "Overlay unmounted");
            return Ok(());
        }

        let errno = std::io::Error::last_os_error();

        // Fallback: try `umount` command
        let status = std::process::Command::new("umount")
            .arg("-l") // lazy unmount
            .arg(merged)
            .status()
            .map_err(|e| BoxError::BuildError(format!("Failed to run umount command: {}", e)))?;

        if status.success() {
            tracing::debug!(path = %merged.display(), "Overlay unmounted via umount command");
            return Ok(());
        }

        Err(BoxError::BuildError(format!(
            "Failed to unmount overlayfs at {}: umount2 returned {}, umount command exited with {}",
            merged.display(),
            errno,
            status
        )))
    }

    #[cfg(not(target_os = "linux"))]
    {
        let _ = merged;
        Ok(())
    }
}

/// Check if overlayfs is supported on this system.
///
/// Always returns `false` on non-Linux platforms (compile-time).
#[cfg(target_os = "linux")]
pub(crate) fn is_overlay_supported() -> bool {
    // Check /proc/filesystems for overlay support
    if let Ok(fs_list) = std::fs::read_to_string("/proc/filesystems") {
        if !fs_list.contains("overlay") {
            tracing::debug!("Overlay not listed in /proc/filesystems");
            return false;
        }
    } else {
        return false;
    }

    // Try a test mount in a tempdir to verify we have permission
    let tmp = match tempfile::TempDir::new() {
        Ok(t) => t,
        Err(_) => return false,
    };

    let lower = tmp.path().join("lower");
    let upper = tmp.path().join("upper");
    let work = tmp.path().join("work");
    let merged = tmp.path().join("merged");

    for dir in [&lower, &upper, &work, &merged] {
        if std::fs::create_dir_all(dir).is_err() {
            return false;
        }
    }

    let ok = overlay_mount(&lower, &upper, &work, &merged).is_ok();
    if ok {
        let _ = overlay_unmount(&merged);
    }
    ok
}

/// Check if overlayfs is supported on this system.
///
/// Always returns `false` on non-Linux platforms.
#[cfg(not(target_os = "linux"))]
pub(crate) fn is_overlay_supported() -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_overlay_supported_returns_bool() {
        // Just verify it doesn't panic
        let _supported = is_overlay_supported();
    }

    #[cfg(not(target_os = "linux"))]
    #[test]
    fn test_overlay_not_supported_on_non_linux() {
        assert!(!is_overlay_supported());
    }

    #[cfg(not(target_os = "linux"))]
    #[test]
    fn test_overlay_mount_fails_on_non_linux() {
        let tmp = tempfile::TempDir::new().unwrap();
        let result = overlay_mount(
            &tmp.path().join("l"),
            &tmp.path().join("u"),
            &tmp.path().join("w"),
            &tmp.path().join("m"),
        );
        assert!(result.is_err());
    }

    #[cfg(not(target_os = "linux"))]
    #[test]
    fn test_overlay_unmount_noop_on_non_linux() {
        let tmp = tempfile::TempDir::new().unwrap();
        assert!(overlay_unmount(tmp.path()).is_ok());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn test_overlay_mount_and_unmount() {
        if !is_overlay_supported() {
            // Skip in environments without overlay support
            return;
        }

        let tmp = tempfile::TempDir::new().unwrap();
        let lower = tmp.path().join("lower");
        let upper = tmp.path().join("upper");
        let work = tmp.path().join("work");
        let merged = tmp.path().join("merged");

        for dir in [&lower, &upper, &work, &merged] {
            std::fs::create_dir_all(dir).unwrap();
        }

        // Create a file in lower
        std::fs::write(lower.join("hello.txt"), "from lower").unwrap();

        // Mount
        overlay_mount(&lower, &upper, &work, &merged).unwrap();

        // Verify lower file visible in merged
        assert_eq!(
            std::fs::read_to_string(merged.join("hello.txt")).unwrap(),
            "from lower"
        );

        // Write to merged — should go to upper
        std::fs::write(merged.join("new.txt"), "from upper").unwrap();
        assert!(upper.join("new.txt").exists());

        // Unmount
        overlay_unmount(&merged).unwrap();
    }
}
