//! Filesystem mount management for virtio-fs

use a3s_box_core::error::{BoxError, Result};
use std::path::{Path, PathBuf};

/// Mount point configuration
#[derive(Debug, Clone)]
pub struct MountPoint {
    /// Host path
    pub host_path: PathBuf,

    /// Guest path
    pub guest_path: PathBuf,

    /// Read-only
    pub readonly: bool,
}

/// Filesystem manager
pub struct FsManager {
    /// Mount points
    mounts: Vec<MountPoint>,
}

impl FsManager {
    /// Create a new filesystem manager
    pub fn new() -> Self {
        Self { mounts: Vec::new() }
    }

    /// Add a mount point
    pub fn add_mount(
        &mut self,
        host_path: impl AsRef<Path>,
        guest_path: impl AsRef<Path>,
        readonly: bool,
    ) {
        self.mounts.push(MountPoint {
            host_path: host_path.as_ref().to_path_buf(),
            guest_path: guest_path.as_ref().to_path_buf(),
            readonly,
        });
    }

    /// Setup default mounts for A3S Box
    pub fn setup_default_mounts(
        &mut self,
        workspace: impl AsRef<Path>,
        cache: impl AsRef<Path>,
    ) -> Result<()> {
        // Workspace mount (read-write)
        self.add_mount(workspace, "/workspace", false);

        // Cache mount (read-write, persistent)
        self.add_mount(cache, "/cache", false);

        Ok(())
    }

    /// Get all mount points
    pub fn mounts(&self) -> &[MountPoint] {
        &self.mounts
    }

    /// Validate and log all configured mounts.
    ///
    /// The actual virtio-fs device setup is handled by the shim via `InstanceSpec`.
    /// This method validates that mount source paths exist on the host.
    pub async fn apply_mounts(&self) -> Result<()> {
        for mount in &self.mounts {
            if !mount.host_path.exists() {
                return Err(BoxError::ConfigError(format!(
                    "Mount source does not exist: {}",
                    mount.host_path.display()
                )));
            }

            tracing::info!(
                host = %mount.host_path.display(),
                guest = %mount.guest_path.display(),
                readonly = mount.readonly,
                "Configured virtio-fs mount"
            );
        }

        Ok(())
    }
}

impl Default for FsManager {
    fn default() -> Self {
        Self::new()
    }
}

/// Ensure cache directory exists
pub async fn ensure_cache_dir() -> Result<PathBuf> {
    let cache_dir = dirs::cache_dir()
        .ok_or_else(|| BoxError::ConfigError("Cannot determine cache directory".to_string()))?
        .join("a3s-box");

    tokio::fs::create_dir_all(&cache_dir).await?;

    Ok(cache_dir)
}

// External dependency for cache directory
mod dirs {
    use std::path::PathBuf;

    pub fn cache_dir() -> Option<PathBuf> {
        #[cfg(target_os = "macos")]
        {
            std::env::var_os("HOME").map(|home| PathBuf::from(home).join("Library/Caches"))
        }

        #[cfg(target_os = "linux")]
        {
            std::env::var_os("XDG_CACHE_HOME")
                .map(PathBuf::from)
                .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".cache")))
        }

        #[cfg(target_os = "windows")]
        {
            std::env::var_os("LOCALAPPDATA").map(PathBuf::from)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_fs_manager_new_empty() {
        let mgr = FsManager::new();
        assert!(mgr.mounts().is_empty());
    }

    #[test]
    fn test_fs_manager_default() {
        let mgr = FsManager::default();
        assert!(mgr.mounts().is_empty());
    }

    #[test]
    fn test_add_mount() {
        let mut mgr = FsManager::new();
        mgr.add_mount("/host/path", "/guest/path", false);

        assert_eq!(mgr.mounts().len(), 1);
        assert_eq!(mgr.mounts()[0].host_path, PathBuf::from("/host/path"));
        assert_eq!(mgr.mounts()[0].guest_path, PathBuf::from("/guest/path"));
        assert!(!mgr.mounts()[0].readonly);
    }

    #[test]
    fn test_add_mount_readonly() {
        let mut mgr = FsManager::new();
        mgr.add_mount("/data", "/mnt/data", true);

        assert!(mgr.mounts()[0].readonly);
    }

    #[test]
    fn test_add_multiple_mounts() {
        let mut mgr = FsManager::new();
        mgr.add_mount("/a", "/ga", false);
        mgr.add_mount("/b", "/gb", true);
        mgr.add_mount("/c", "/gc", false);

        assert_eq!(mgr.mounts().len(), 3);
    }

    #[test]
    fn test_setup_default_mounts() {
        let tmp = TempDir::new().unwrap();
        let workspace = tmp.path().join("workspace");
        let cache = tmp.path().join("cache");

        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::create_dir_all(&cache).unwrap();

        let mut mgr = FsManager::new();
        mgr.setup_default_mounts(&workspace, &cache).unwrap();

        // workspace + cache = 2
        assert_eq!(mgr.mounts().len(), 2);

        // Workspace is read-write
        assert!(!mgr.mounts()[0].readonly);
        assert_eq!(mgr.mounts()[0].guest_path, PathBuf::from("/workspace"));

        // Cache is read-write
        assert!(!mgr.mounts()[1].readonly);
        assert_eq!(mgr.mounts()[1].guest_path, PathBuf::from("/cache"));
    }

    #[tokio::test]
    async fn test_apply_mounts_valid() {
        let tmp = TempDir::new().unwrap();
        let host_dir = tmp.path().join("data");
        std::fs::create_dir_all(&host_dir).unwrap();

        let mut mgr = FsManager::new();
        mgr.add_mount(&host_dir, "/guest/data", false);

        assert!(mgr.apply_mounts().await.is_ok());
    }

    #[tokio::test]
    async fn test_apply_mounts_missing_host_path() {
        let mut mgr = FsManager::new();
        mgr.add_mount("/nonexistent/path/12345", "/guest", false);

        let result = mgr.apply_mounts().await;
        assert!(result.is_err());
        let err = format!("{}", result.unwrap_err());
        assert!(err.contains("Mount source does not exist"));
    }

    #[tokio::test]
    async fn test_ensure_cache_dir() {
        let dir = ensure_cache_dir().await.unwrap();
        assert!(dir.exists());
        assert!(dir.to_string_lossy().contains("a3s-box"));
    }

    #[test]
    fn test_mount_point_debug() {
        let mp = MountPoint {
            host_path: PathBuf::from("/host"),
            guest_path: PathBuf::from("/guest"),
            readonly: true,
        };
        let debug = format!("{:?}", mp);
        assert!(debug.contains("host"));
        assert!(debug.contains("guest"));
        assert!(debug.contains("true"));
    }
}
