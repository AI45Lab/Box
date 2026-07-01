//! Rootfs provider — abstracts how a rootfs directory is prepared for a box.
//!
//! Two built-in providers:
//! - `CopyProvider` — full recursive copy (works everywhere, current default)
//! - `OverlayProvider` — Linux overlayfs mount (near-instant, CoW)

use std::path::{Path, PathBuf};

use a3s_box_core::error::{BoxError, Result};

/// Abstracts how a rootfs directory is prepared for a box from a cached lower layer.
pub trait RootfsProvider: Send + Sync {
    /// Prepare a rootfs at `box_dir` from the cached read-only layer at `cache_dir`.
    /// Returns the path to use as `InstanceSpec.rootfs_path`.
    fn prepare(&self, box_dir: &Path, cache_dir: &Path) -> Result<PathBuf>;

    /// Cleanup after box stops.
    ///
    /// When `persistent` is true, the writable layer (overlay upper dir or copy
    /// rootfs) is preserved on disk so changes survive the next start.
    /// When false, the writable layer is wiped for a clean slate.
    fn cleanup(&self, box_dir: &Path, persistent: bool) -> Result<()>;

    /// Human-readable name for logging.
    fn name(&self) -> &'static str;
}

/// Full recursive copy provider — works on all platforms.
///
/// This is the original behavior: copies the entire cached rootfs into
/// `box_dir/rootfs/`. Safe but slow for large images.
pub struct CopyProvider;

impl RootfsProvider for CopyProvider {
    fn prepare(&self, box_dir: &Path, cache_dir: &Path) -> Result<PathBuf> {
        let rootfs = box_dir.join("rootfs");
        // Reuse existing rootfs when persistent and already populated
        if rootfs.exists() {
            tracing::info!(path = %rootfs.display(), "Reusing persistent rootfs");
            return Ok(rootfs);
        }
        crate::cache::layer_cache::copy_dir_recursive(cache_dir, &rootfs)?;
        Ok(rootfs)
    }

    fn cleanup(&self, box_dir: &Path, persistent: bool) -> Result<()> {
        if persistent {
            tracing::info!("Persistent box: keeping rootfs on disk");
            return Ok(());
        }
        let rootfs = box_dir.join("rootfs");
        if rootfs.exists() {
            std::fs::remove_dir_all(&rootfs).map_err(|e| {
                BoxError::BuildError(format!(
                    "Failed to remove rootfs {}: {}",
                    rootfs.display(),
                    e
                ))
            })?;
        }
        Ok(())
    }

    fn name(&self) -> &'static str {
        "copy"
    }
}

/// Overlayfs provider — near-instant CoW mounts (Linux only).
///
/// Layout:
/// ```text
/// cache_dir/           ← lower (read-only, shared across boxes)
/// box_dir/upper/       ← upper (per-box writes)
/// box_dir/work/        ← overlayfs workdir
/// box_dir/merged/      ← merged view → InstanceSpec.rootfs_path
/// ```
pub struct OverlayProvider;

impl RootfsProvider for OverlayProvider {
    fn prepare(&self, box_dir: &Path, cache_dir: &Path) -> Result<PathBuf> {
        let upper = box_dir.join("upper");
        let work = box_dir.join("work");
        let merged = box_dir.join("merged");

        for dir in [&upper, &work, &merged] {
            std::fs::create_dir_all(dir).map_err(|e| {
                BoxError::BuildError(format!(
                    "Failed to create overlay dir {}: {}",
                    dir.display(),
                    e
                ))
            })?;
        }

        // Idempotent: a restart re-runs prepare(); without this guard each call
        // stacks another overlay on `merged` (the leaked double/triple mounts).
        if super::is_mountpoint(&merged) {
            tracing::debug!(merged = %merged.display(), "Overlay already mounted; reusing");
            return Ok(merged);
        }

        super::overlay::overlay_mount(cache_dir, &upper, &work, &merged)?;

        tracing::info!(
            lower = %cache_dir.display(),
            merged = %merged.display(),
            "Overlay mount ready"
        );

        Ok(merged)
    }

    fn cleanup(&self, box_dir: &Path, persistent: bool) -> Result<()> {
        let merged = box_dir.join("merged");
        // Bounded unmount-retry rather than a single attempt: a transient EBUSY
        // must not leave the overlay mounted, or the remove_dir_all below would
        // recurse into the live mount and leak it. Mirrors the cleanup paths in
        // cleanup_stopped_box/cleanup_removed_box.
        super::unmount_box_overlay(&merged);

        if persistent {
            // Keep upper (writes) and remove only merged/work (not needed at rest)
            tracing::info!("Persistent box: keeping overlay upper layer on disk");
            for dir_name in &["merged", "work"] {
                let dir = box_dir.join(dir_name);
                if dir.exists() {
                    if let Err(e) = std::fs::remove_dir_all(&dir) {
                        tracing::warn!(path = %dir.display(), error = %e, "Failed to remove overlay dir");
                    }
                }
            }
            return Ok(());
        }

        for dir_name in &["upper", "work", "merged"] {
            let dir = box_dir.join(dir_name);
            if dir.exists() {
                if let Err(e) = std::fs::remove_dir_all(&dir) {
                    tracing::warn!(
                        path = %dir.display(),
                        error = %e,
                        "Failed to remove overlay dir"
                    );
                }
            }
        }

        Ok(())
    }

    fn name(&self) -> &'static str {
        "overlay"
    }
}

/// Auto-detect the best available rootfs provider for the current platform.
pub fn default_provider() -> Box<dyn RootfsProvider> {
    if super::overlay::is_overlay_supported() {
        tracing::info!("Using overlayfs rootfs provider");
        return Box::new(OverlayProvider);
    }

    tracing::info!("Overlayfs not available, using copy provider");
    Box::new(CopyProvider)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn make_sample_rootfs(dir: &Path) {
        std::fs::create_dir_all(dir.join("etc")).unwrap();
        std::fs::create_dir_all(dir.join("bin")).unwrap();
        std::fs::write(dir.join("etc/hostname"), "testbox").unwrap();
        std::fs::write(dir.join("bin/hello"), "#!/bin/sh\necho hi").unwrap();
    }

    #[test]
    fn test_copy_provider_prepare() {
        let tmp = TempDir::new().unwrap();
        let cache_dir = tmp.path().join("cache");
        let box_dir = tmp.path().join("box");
        std::fs::create_dir_all(&cache_dir).unwrap();
        std::fs::create_dir_all(&box_dir).unwrap();
        make_sample_rootfs(&cache_dir);

        let provider = CopyProvider;
        let rootfs = provider.prepare(&box_dir, &cache_dir).unwrap();

        assert_eq!(rootfs, box_dir.join("rootfs"));
        assert!(rootfs.join("etc/hostname").exists());
        assert_eq!(
            std::fs::read_to_string(rootfs.join("etc/hostname")).unwrap(),
            "testbox"
        );
        assert!(rootfs.join("bin/hello").exists());
    }

    #[test]
    fn test_copy_provider_cleanup() {
        let tmp = TempDir::new().unwrap();
        let cache_dir = tmp.path().join("cache");
        let box_dir = tmp.path().join("box");
        std::fs::create_dir_all(&cache_dir).unwrap();
        std::fs::create_dir_all(&box_dir).unwrap();
        make_sample_rootfs(&cache_dir);

        let provider = CopyProvider;
        let rootfs = provider.prepare(&box_dir, &cache_dir).unwrap();
        assert!(rootfs.exists());

        provider.cleanup(&box_dir, false).unwrap();
        assert!(!rootfs.exists());
    }

    #[test]
    fn test_copy_provider_cleanup_nonexistent() {
        let tmp = TempDir::new().unwrap();
        let provider = CopyProvider;
        // Should not error on missing dir
        provider.cleanup(tmp.path(), false).unwrap();
    }

    #[test]
    fn test_copy_provider_name() {
        assert_eq!(CopyProvider.name(), "copy");
    }

    #[test]
    fn test_overlay_provider_name() {
        assert_eq!(OverlayProvider.name(), "overlay");
    }

    #[test]
    fn test_default_provider_returns_something() {
        let provider = default_provider();
        // On any platform, we should get a provider
        assert!(!provider.name().is_empty());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn test_overlay_provider_prepare_and_cleanup() {
        if !super::super::overlay::is_overlay_supported() {
            // Skip if overlay not available (e.g., in container without privileges)
            return;
        }

        let tmp = TempDir::new().unwrap();
        let cache_dir = tmp.path().join("cache");
        let box_dir = tmp.path().join("box");
        std::fs::create_dir_all(&cache_dir).unwrap();
        std::fs::create_dir_all(&box_dir).unwrap();
        make_sample_rootfs(&cache_dir);

        let provider = OverlayProvider;
        let merged = provider.prepare(&box_dir, &cache_dir).unwrap();

        assert_eq!(merged, box_dir.join("merged"));
        assert!(merged.join("etc/hostname").exists());
        assert_eq!(
            std::fs::read_to_string(merged.join("etc/hostname")).unwrap(),
            "testbox"
        );

        // Write to merged — should go to upper
        std::fs::write(merged.join("etc/newfile"), "overlay write").unwrap();
        assert!(box_dir.join("upper/etc/newfile").exists());

        provider.cleanup(&box_dir, false).unwrap();
        assert!(!box_dir.join("merged").exists());
        assert!(!box_dir.join("upper").exists());
        assert!(!box_dir.join("work").exists());
    }
}
