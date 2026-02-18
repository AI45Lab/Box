//! Rootfs builder for guest VM.
//!
//! Creates a minimal rootfs containing:
//! - Basic directory structure
//! - Essential configuration files

use std::fs;
use std::path::PathBuf;

use a3s_box_core::error::{BoxError, Result};

use super::layout::GuestLayout;

/// Builder for creating guest rootfs.
pub struct RootfsBuilder {
    /// Target rootfs directory.
    rootfs_path: PathBuf,

    /// Guest layout configuration.
    layout: GuestLayout,
}

impl RootfsBuilder {
    /// Create a new rootfs builder.
    pub fn new(rootfs_path: impl Into<PathBuf>) -> Self {
        Self {
            rootfs_path: rootfs_path.into(),
            layout: GuestLayout::default(),
        }
    }

    /// Set a custom guest layout.
    pub fn with_layout(mut self, layout: GuestLayout) -> Self {
        self.layout = layout;
        self
    }

    /// Build the rootfs.
    pub fn build(&self) -> Result<()> {
        tracing::info!(
            rootfs = %self.rootfs_path.display(),
            "Building guest rootfs"
        );

        // Create base directory
        fs::create_dir_all(&self.rootfs_path).map_err(|e| {
            BoxError::Other(format!(
                "Failed to create rootfs directory {}: {}",
                self.rootfs_path.display(),
                e
            ))
        })?;

        // Create directory structure
        self.create_directories()?;

        // Create essential files
        self.create_essential_files()?;

        tracing::info!("Guest rootfs built successfully");
        Ok(())
    }

    /// Create the directory structure.
    fn create_directories(&self) -> Result<()> {
        for dir in self.layout.required_dirs() {
            let full_path = self.rootfs_path.join(dir.trim_start_matches('/'));
            fs::create_dir_all(&full_path).map_err(|e| {
                BoxError::Other(format!(
                    "Failed to create directory {}: {}",
                    full_path.display(),
                    e
                ))
            })?;
            tracing::debug!(dir = %full_path.display(), "Created directory");
        }
        Ok(())
    }

    /// Create essential configuration files.
    fn create_essential_files(&self) -> Result<()> {
        // /etc/passwd - minimal user database
        let passwd_content =
            "root:x:0:0:root:/root:/bin/sh\nnobody:x:65534:65534:nobody:/:/bin/false\n";
        self.write_file("etc/passwd", passwd_content)?;

        // /etc/group - minimal group database
        let group_content = "root:x:0:\nnogroup:x:65534:\n";
        self.write_file("etc/group", group_content)?;

        // /etc/hosts - basic hosts file
        let hosts_content = "127.0.0.1\tlocalhost\n::1\t\tlocalhost\n";
        self.write_file("etc/hosts", hosts_content)?;

        // /etc/resolv.conf - DNS configuration (can be overridden)
        let resolv_content = "nameserver 8.8.8.8\nnameserver 8.8.4.4\n";
        self.write_file("etc/resolv.conf", resolv_content)?;

        // /etc/nsswitch.conf - name service switch configuration
        let nsswitch_content = "passwd: files\ngroup: files\nhosts: files dns\n";
        self.write_file("etc/nsswitch.conf", nsswitch_content)?;

        Ok(())
    }

    /// Write a file to the rootfs.
    fn write_file(&self, relative_path: &str, content: &str) -> Result<()> {
        let full_path = self.rootfs_path.join(relative_path);

        // Ensure parent directory exists
        if let Some(parent) = full_path.parent() {
            fs::create_dir_all(parent).map_err(|e| {
                BoxError::Other(format!("Failed to create parent directory: {}", e))
            })?;
        }

        fs::write(&full_path, content).map_err(|e| {
            BoxError::Other(format!("Failed to write {}: {}", full_path.display(), e))
        })?;

        tracing::debug!(path = %full_path.display(), "Created file");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_rootfs_builder_creates_directories() {
        let temp_dir = TempDir::new().unwrap();
        let rootfs_path = temp_dir.path().join("rootfs");

        let builder = RootfsBuilder::new(&rootfs_path);
        builder.build().unwrap();

        // Check directories were created
        assert!(rootfs_path.join("dev").exists());
        assert!(rootfs_path.join("proc").exists());
        assert!(rootfs_path.join("sys").exists());
        assert!(rootfs_path.join("tmp").exists());
        assert!(rootfs_path.join("etc").exists());
        assert!(rootfs_path.join("workspace").exists());
    }

    #[test]
    fn test_rootfs_builder_creates_essential_files() {
        let temp_dir = TempDir::new().unwrap();
        let rootfs_path = temp_dir.path().join("rootfs");

        let builder = RootfsBuilder::new(&rootfs_path);
        builder.build().unwrap();

        // Check files were created
        assert!(rootfs_path.join("etc/passwd").exists());
        assert!(rootfs_path.join("etc/group").exists());
        assert!(rootfs_path.join("etc/hosts").exists());
        assert!(rootfs_path.join("etc/resolv.conf").exists());

        // Check content
        let passwd = fs::read_to_string(rootfs_path.join("etc/passwd")).unwrap();
        assert!(passwd.contains("root:x:0:0"));
    }

    #[test]
    fn test_rootfs_builder_essential_files_content() {
        let temp_dir = TempDir::new().unwrap();
        let rootfs_path = temp_dir.path().join("rootfs");

        RootfsBuilder::new(&rootfs_path).build().unwrap();

        // Verify /etc/passwd content
        let passwd = fs::read_to_string(rootfs_path.join("etc/passwd")).unwrap();
        assert!(passwd.contains("root:x:0:0:root:/root:/bin/sh"));
        assert!(passwd.contains("nobody:x:65534:65534"));

        // Verify /etc/group content
        let group = fs::read_to_string(rootfs_path.join("etc/group")).unwrap();
        assert!(group.contains("root:x:0:"));
        assert!(group.contains("nogroup:x:65534:"));

        // Verify /etc/hosts content
        let hosts = fs::read_to_string(rootfs_path.join("etc/hosts")).unwrap();
        assert!(hosts.contains("127.0.0.1"));
        assert!(hosts.contains("localhost"));

        // Verify /etc/resolv.conf content
        let resolv = fs::read_to_string(rootfs_path.join("etc/resolv.conf")).unwrap();
        assert!(resolv.contains("nameserver"));

        // Verify /etc/nsswitch.conf content
        let nsswitch = fs::read_to_string(rootfs_path.join("etc/nsswitch.conf")).unwrap();
        assert!(nsswitch.contains("passwd: files"));
        assert!(nsswitch.contains("hosts: files dns"));
    }

    #[test]
    fn test_rootfs_builder_with_custom_layout() {
        let temp_dir = TempDir::new().unwrap();
        let rootfs_path = temp_dir.path().join("rootfs");

        let custom_layout = GuestLayout {
            workspace_dir: "/work",
            tmp_dir: "/tmp",
            run_dir: "/run",
        };

        let builder = RootfsBuilder::new(&rootfs_path).with_layout(custom_layout);
        builder.build().unwrap();

        // Verify custom directories were created
        assert!(rootfs_path.join("work").exists());
    }

    #[test]
    fn test_rootfs_builder_idempotent() {
        let temp_dir = TempDir::new().unwrap();
        let rootfs_path = temp_dir.path().join("rootfs");

        let builder = RootfsBuilder::new(&rootfs_path);

        // Build twice should succeed
        builder.build().unwrap();
        builder.build().unwrap();

        // Verify everything still exists
        assert!(rootfs_path.join("etc/passwd").exists());
        assert!(rootfs_path.join("dev").exists());
    }
}
