//! OCI rootfs builder.
//!
//! Extracts an OCI image into a guest rootfs directory.
//! Optionally installs the guest-init binary at /sbin/init.

use a3s_box_core::error::{BoxError, Result};
use std::path::PathBuf;

use super::image::OciImage;
use super::layers::extract_layer;

/// Builder for creating a guest rootfs from an OCI image.
///
/// The image is extracted directly at the rootfs root ("/"), preserving
/// absolute symlinks and dynamic linker paths from the original image.
pub struct OciRootfsBuilder {
    /// Target rootfs directory
    rootfs_path: PathBuf,

    /// Path to the OCI image directory
    image_path: PathBuf,

    /// Path to guest init binary (optional)
    guest_init_path: Option<PathBuf>,
}

impl OciRootfsBuilder {
    /// Create a new OCI rootfs builder.
    pub fn new(rootfs_path: impl Into<PathBuf>) -> Self {
        Self {
            rootfs_path: rootfs_path.into(),
            image_path: PathBuf::new(),
            guest_init_path: None,
        }
    }

    /// Set the OCI image path to extract.
    pub fn with_image(mut self, path: impl Into<PathBuf>) -> Self {
        self.image_path = path.into();
        self
    }

    /// Set the path to the guest init binary.
    ///
    /// If set, the guest init binary will be installed at `/sbin/init` in the
    /// rootfs, overriding any existing init from the OCI image.
    pub fn with_guest_init(mut self, path: impl Into<PathBuf>) -> Self {
        self.guest_init_path = Some(path.into());
        self
    }

    /// Build the rootfs by extracting the OCI image.
    ///
    /// # Process
    ///
    /// 1. Create base directory structure
    /// 2. Extract image layers to rootfs root
    /// 3. Install guest init binary (if provided)
    /// 4. Ensure essential system files exist
    pub fn build(&self) -> Result<()> {
        tracing::info!(
            rootfs = %self.rootfs_path.display(),
            "Building OCI rootfs"
        );

        if self.image_path.as_os_str().is_empty() {
            return Err(BoxError::Other("OCI image path not set".to_string()));
        }

        self.create_base_structure()?;
        self.extract_image()?;

        if self.guest_init_path.is_some() {
            self.install_guest_init()?;
        }

        self.create_essential_files()?;

        tracing::info!("OCI rootfs built successfully");
        Ok(())
    }

    /// Create the base directory structure.
    fn create_base_structure(&self) -> Result<()> {
        let dirs = [
            "dev", "proc", "sys", "tmp", "run", "etc", "var", "var/tmp", "var/log", "workspace",
        ];

        for dir in dirs {
            let full_path = self.rootfs_path.join(dir);
            std::fs::create_dir_all(&full_path).map_err(|e| {
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

    /// Extract OCI image layers to the rootfs root.
    fn extract_image(&self) -> Result<()> {
        let image = OciImage::from_path(&self.image_path)?;

        tracing::info!(
            image = %self.image_path.display(),
            rootfs = %self.rootfs_path.display(),
            layers = image.layer_paths().len(),
            "Extracting OCI image"
        );

        for layer_path in image.layer_paths() {
            extract_layer(layer_path, &self.rootfs_path)?;
        }

        Ok(())
    }

    /// Install guest init binary to /sbin/init.
    fn install_guest_init(&self) -> Result<()> {
        let src = self
            .guest_init_path
            .as_ref()
            .ok_or_else(|| BoxError::Other("Guest init path not set".to_string()))?;

        if !src.exists() {
            return Err(BoxError::Other(format!(
                "Guest init binary not found: {}",
                src.display()
            )));
        }

        let sbin_dir = self.rootfs_path.join("sbin");
        std::fs::create_dir_all(&sbin_dir)
            .map_err(|e| BoxError::Other(format!("Failed to create /sbin directory: {}", e)))?;

        let dest = sbin_dir.join("init");
        // Remove any existing init (e.g., busybox symlink in Alpine)
        if dest.exists() || dest.symlink_metadata().is_ok() {
            std::fs::remove_file(&dest).map_err(|e| {
                BoxError::Other(format!(
                    "Failed to remove existing {}: {}",
                    dest.display(),
                    e
                ))
            })?;
        }
        std::fs::copy(src, &dest).map_err(|e| {
            BoxError::Other(format!(
                "Failed to copy guest init to {}: {}",
                dest.display(),
                e
            ))
        })?;

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&dest)
                .map_err(|e| BoxError::Other(format!("Failed to get permissions: {}", e)))?
                .permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&dest, perms)
                .map_err(|e| BoxError::Other(format!("Failed to set permissions: {}", e)))?;
        }

        tracing::info!(
            src = %src.display(),
            dst = %dest.display(),
            "Installed guest init"
        );

        Ok(())
    }

    /// Ensure essential system files exist, preserving OCI image entries.
    fn create_essential_files(&self) -> Result<()> {
        self.ensure_passwd_entries(&[
            ("root", "root:x:0:0:root:/root:/bin/sh"),
            ("nobody", "nobody:x:65534:65534:nobody:/:/bin/false"),
        ])?;

        self.ensure_group_entries(&[("root", "root:x:0:"), ("nogroup", "nogroup:x:65534:")])?;

        self.write_file("etc/hosts", "127.0.0.1\tlocalhost\n::1\t\tlocalhost\n")?;
        self.write_file("etc/resolv.conf", "nameserver 8.8.8.8\nnameserver 8.8.4.4\n")?;
        self.write_file(
            "etc/nsswitch.conf",
            "passwd: files\ngroup: files\nhosts: files dns\n",
        )?;

        Ok(())
    }

    fn ensure_passwd_entries(&self, required: &[(&str, &str)]) -> Result<()> {
        let passwd_path = self.rootfs_path.join("etc/passwd");
        let existing = std::fs::read_to_string(&passwd_path).unwrap_or_default();

        let mut content = existing.clone();
        if !content.is_empty() && !content.ends_with('\n') {
            content.push('\n');
        }

        for (username, entry) in required {
            let has_user = existing
                .lines()
                .any(|line| line.split(':').next() == Some(username));
            if !has_user {
                content.push_str(entry);
                content.push('\n');
            }
        }

        self.write_file("etc/passwd", &content)
    }

    fn ensure_group_entries(&self, required: &[(&str, &str)]) -> Result<()> {
        let group_path = self.rootfs_path.join("etc/group");
        let existing = std::fs::read_to_string(&group_path).unwrap_or_default();

        let mut content = existing.clone();
        if !content.is_empty() && !content.ends_with('\n') {
            content.push('\n');
        }

        for (groupname, entry) in required {
            let has_group = existing
                .lines()
                .any(|line| line.split(':').next() == Some(groupname));
            if !has_group {
                content.push_str(entry);
                content.push('\n');
            }
        }

        self.write_file("etc/group", &content)
    }

    fn write_file(&self, relative_path: &str, content: &str) -> Result<()> {
        let full_path = self.rootfs_path.join(relative_path);

        if let Some(parent) = full_path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| {
                BoxError::Other(format!("Failed to create parent directory: {}", e))
            })?;
        }

        std::fs::write(&full_path, content).map_err(|e| {
            BoxError::Other(format!("Failed to write {}: {}", full_path.display(), e))
        })?;

        tracing::debug!(path = %full_path.display(), "Created file");
        Ok(())
    }

    /// Get the OCI image configuration.
    ///
    /// Useful for extracting entrypoint, environment, working directory, etc.
    pub fn image_config(&self) -> Result<super::image::OciImageConfig> {
        let image = OciImage::from_path(&self.image_path)?;
        Ok(image.config().clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::Path;
    use tempfile::TempDir;

    #[test]
    fn test_oci_rootfs_builder_creates_base_structure() {
        let temp_dir = TempDir::new().unwrap();
        let rootfs_path = temp_dir.path().join("rootfs");
        let image = temp_dir.path().join("image");

        create_test_oci_image(&image);

        OciRootfsBuilder::new(&rootfs_path)
            .with_image(&image)
            .build()
            .unwrap();

        assert!(rootfs_path.join("dev").exists());
        assert!(rootfs_path.join("proc").exists());
        assert!(rootfs_path.join("sys").exists());
        assert!(rootfs_path.join("tmp").exists());
        assert!(rootfs_path.join("etc").exists());
        assert!(rootfs_path.join("workspace").exists());
    }

    #[test]
    fn test_oci_rootfs_builder_creates_essential_files() {
        let temp_dir = TempDir::new().unwrap();
        let rootfs_path = temp_dir.path().join("rootfs");
        let image = temp_dir.path().join("image");

        create_test_oci_image(&image);

        OciRootfsBuilder::new(&rootfs_path)
            .with_image(&image)
            .build()
            .unwrap();

        assert!(rootfs_path.join("etc/passwd").exists());
        assert!(rootfs_path.join("etc/group").exists());
        assert!(rootfs_path.join("etc/hosts").exists());
        assert!(rootfs_path.join("etc/resolv.conf").exists());

        let passwd = fs::read_to_string(rootfs_path.join("etc/passwd")).unwrap();
        assert!(passwd.contains("root:x:0:0"));
    }

    #[test]
    fn test_oci_rootfs_builder_extracts_image_at_root() {
        let temp_dir = TempDir::new().unwrap();
        let rootfs_path = temp_dir.path().join("rootfs");
        let image = temp_dir.path().join("image");

        create_test_oci_image_with_file(&image, "app/main.py", b"print('hello')");

        OciRootfsBuilder::new(&rootfs_path)
            .with_image(&image)
            .build()
            .unwrap();

        // File extracted at rootfs root, not under /agent
        let extracted = rootfs_path.join("app/main.py");
        assert!(extracted.exists());
        let content = fs::read_to_string(extracted).unwrap();
        assert_eq!(content, "print('hello')");
    }

    #[test]
    fn test_oci_rootfs_builder_no_image_set() {
        let temp_dir = TempDir::new().unwrap();
        let rootfs_path = temp_dir.path().join("rootfs");

        let result = OciRootfsBuilder::new(&rootfs_path).build();

        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("image path not set"));
    }

    // Helper: create a minimal test OCI image
    fn create_test_oci_image(path: &Path) {
        create_test_oci_image_with_file(path, "test.txt", b"test content");
    }

    fn create_test_oci_image_with_file(path: &Path, filename: &str, content: &[u8]) {
        use flate2::write::GzEncoder;
        use flate2::Compression;
        use tar::Builder;

        fs::create_dir_all(path.join("blobs/sha256")).unwrap();
        fs::write(path.join("oci-layout"), r#"{"imageLayoutVersion":"1.0.0"}"#).unwrap();

        let layer_hash = "layer123";
        let layer_path = path.join("blobs/sha256").join(layer_hash);
        {
            let file = fs::File::create(&layer_path).unwrap();
            let encoder = GzEncoder::new(file, Compression::default());
            let mut builder = Builder::new(encoder);

            let mut header = tar::Header::new_gnu();
            header.set_size(content.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();

            builder.append_data(&mut header, filename, content).unwrap();
            builder.finish().unwrap();
        }

        let config_content = r#"{
            "architecture": "amd64",
            "os": "linux",
            "config": {
                "Entrypoint": ["/usr/local/bin/app"],
                "Cmd": null,
                "Env": ["PATH=/usr/local/bin:/usr/bin:/bin"],
                "WorkingDir": "/app"
            },
            "rootfs": {
                "type": "layers",
                "diff_ids": ["sha256:layer123"]
            },
            "history": []
        }"#;
        let config_hash = "config456";
        fs::write(path.join("blobs/sha256").join(config_hash), config_content).unwrap();

        let manifest_content = format!(
            r#"{{
            "schemaVersion": 2,
            "mediaType": "application/vnd.oci.image.manifest.v1+json",
            "config": {{
                "mediaType": "application/vnd.oci.image.config.v1+json",
                "digest": "sha256:{}",
                "size": {}
            }},
            "layers": [
                {{
                    "mediaType": "application/vnd.oci.image.layer.v1.tar+gzip",
                    "digest": "sha256:{}",
                    "size": 100
                }}
            ]
        }}"#,
            config_hash,
            config_content.len(),
            layer_hash
        );
        let manifest_hash = "manifest789";
        fs::write(
            path.join("blobs/sha256").join(manifest_hash),
            &manifest_content,
        )
        .unwrap();

        let index_content = format!(
            r#"{{
            "schemaVersion": 2,
            "mediaType": "application/vnd.oci.image.index.v1+json",
            "manifests": [
                {{
                    "mediaType": "application/vnd.oci.image.manifest.v1+json",
                    "digest": "sha256:{}",
                    "size": {}
                }}
            ]
        }}"#,
            manifest_hash,
            manifest_content.len()
        );
        fs::write(path.join("index.json"), index_content).unwrap();
    }
}
