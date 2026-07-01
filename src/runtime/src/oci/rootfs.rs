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

    /// Override for `/etc/resolv.conf` content (e.g. the pod's DNS config).
    /// When `None`, a default resolv.conf is written.
    resolv_conf: Option<String>,
}

impl OciRootfsBuilder {
    /// Create a new OCI rootfs builder.
    pub fn new(rootfs_path: impl Into<PathBuf>) -> Self {
        Self {
            rootfs_path: rootfs_path.into(),
            image_path: PathBuf::new(),
            guest_init_path: None,
            resolv_conf: None,
        }
    }

    /// Override the `/etc/resolv.conf` written into the rootfs.
    ///
    /// Used to apply a pod's CRI `DNSConfig`. An empty string is ignored so the
    /// default resolv.conf is written instead.
    pub fn with_resolv_conf(mut self, content: impl Into<String>) -> Self {
        let content = content.into();
        if !content.is_empty() {
            self.resolv_conf = Some(content);
        }
        self
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
            return Err(BoxError::OciImageError(
                "OCI image path not set".to_string(),
            ));
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

    /// Install or refresh only the guest-init binary in an existing rootfs.
    pub fn install_guest_init_only(&self) -> Result<()> {
        if self.guest_init_path.is_some() {
            self.install_guest_init()?;
        }
        Ok(())
    }

    /// Create the base directory structure.
    fn create_base_structure(&self) -> Result<()> {
        let dirs = [
            "dev",
            "proc",
            "sys",
            "tmp",
            "run",
            "etc",
            "var",
            "var/tmp",
            "var/log",
            "workspace",
        ];

        for dir in dirs {
            let full_path = self.rootfs_path.join(dir);
            std::fs::create_dir_all(&full_path).map_err(|e| {
                BoxError::BuildError(format!(
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
            .ok_or_else(|| BoxError::BuildError("Guest init path not set".to_string()))?;

        if !src.exists() {
            return Err(BoxError::BuildError(format!(
                "Guest init binary not found: {}",
                src.display()
            )));
        }

        #[cfg(target_os = "windows")]
        let install_dir = {
            let sbin_link = self.rootfs_path.join("sbin");
            match std::fs::symlink_metadata(&sbin_link) {
                Ok(meta) if meta.is_dir() => sbin_link.clone(),
                Ok(meta) if meta.file_type().is_symlink() => {
                    let target = std::fs::read_link(&sbin_link).map_err(|err| {
                        BoxError::BuildError(format!(
                            "Failed to resolve /sbin symlink {}: {}",
                            sbin_link.display(),
                            err
                        ))
                    })?;
                    if target.is_absolute() {
                        target
                    } else {
                        self.rootfs_path.join(target)
                    }
                }
                Ok(_) => {
                    return Err(BoxError::BuildError(format!(
                        "Cannot install guest init because {} exists and is not a directory or symlink",
                        sbin_link.display()
                    )));
                }
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                    let usr_sbin = self.rootfs_path.join("usr").join("sbin");
                    if usr_sbin.is_dir() {
                        usr_sbin
                    } else {
                        std::fs::create_dir_all(&sbin_link).map_err(|e| {
                            BoxError::BuildError(format!("Failed to create /sbin directory: {}", e))
                        })?;
                        sbin_link.clone()
                    }
                }
                Err(err) => {
                    return Err(BoxError::BuildError(format!(
                        "Failed to inspect /sbin path {}: {}",
                        sbin_link.display(),
                        err
                    )));
                }
            }
        };

        #[cfg(not(target_os = "windows"))]
        let install_dir = {
            let sbin_dir = self.rootfs_path.join("sbin");
            std::fs::create_dir_all(&sbin_dir).map_err(|e| {
                BoxError::BuildError(format!("Failed to create /sbin directory: {}", e))
            })?;
            sbin_dir
        };

        std::fs::create_dir_all(&install_dir).map_err(|e| {
            BoxError::BuildError(format!(
                "Failed to create guest init install directory {}: {}",
                install_dir.display(),
                e
            ))
        })?;

        let dest = install_dir.join("init");
        // Remove any existing init (e.g., busybox symlink in Alpine)
        if dest.exists() || dest.symlink_metadata().is_ok() {
            std::fs::remove_file(&dest).map_err(|e| {
                BoxError::BuildError(format!(
                    "Failed to remove existing {}: {}",
                    dest.display(),
                    e
                ))
            })?;
        }
        std::fs::copy(src, &dest).map_err(|e| {
            BoxError::BuildError(format!(
                "Failed to copy guest init to {}: {}",
                dest.display(),
                e
            ))
        })?;

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&dest)
                .map_err(|e| BoxError::BuildError(format!("Failed to get permissions: {}", e)))?
                .permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&dest, perms)
                .map_err(|e| BoxError::BuildError(format!("Failed to set permissions: {}", e)))?;
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
        let resolv_conf = self
            .resolv_conf
            .as_deref()
            .unwrap_or("nameserver 8.8.8.8\nnameserver 8.8.4.4\n");
        self.write_file("etc/resolv.conf", resolv_conf)?;
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
                BoxError::BuildError(format!("Failed to create parent directory: {}", e))
            })?;
        }

        std::fs::write(&full_path, content).map_err(|e| {
            BoxError::BuildError(format!("Failed to write {}: {}", full_path.display(), e))
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
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("image path not set"));
    }

    #[test]
    fn test_oci_rootfs_builder_uses_custom_resolv_conf_and_default_when_empty() {
        let temp_dir = TempDir::new().unwrap();
        let custom_rootfs = temp_dir.path().join("custom-rootfs");
        let default_rootfs = temp_dir.path().join("default-rootfs");
        let image = temp_dir.path().join("image");
        create_test_oci_image(&image);

        OciRootfsBuilder::new(&custom_rootfs)
            .with_image(&image)
            .with_resolv_conf("nameserver 10.0.0.10\nsearch svc.cluster.local\n")
            .build()
            .unwrap();

        assert_eq!(
            fs::read_to_string(custom_rootfs.join("etc/resolv.conf")).unwrap(),
            "nameserver 10.0.0.10\nsearch svc.cluster.local\n"
        );

        OciRootfsBuilder::new(&default_rootfs)
            .with_image(&image)
            .with_resolv_conf("")
            .build()
            .unwrap();

        let resolv_conf = fs::read_to_string(default_rootfs.join("etc/resolv.conf")).unwrap();
        assert!(resolv_conf.contains("nameserver 8.8.8.8"));
        assert!(resolv_conf.contains("nameserver 8.8.4.4"));
    }

    #[test]
    fn test_oci_rootfs_builder_preserves_existing_passwd_and_group_entries() {
        let temp_dir = TempDir::new().unwrap();
        let rootfs_path = temp_dir.path().join("rootfs");
        let image = temp_dir.path().join("image");

        create_test_oci_image_with_files(
            &image,
            &[
                (
                    "etc/passwd",
                    b"root:x:0:0:Root User:/root:/bin/bash\napp:x:1000:1000:App:/app:/bin/sh"
                        .as_slice(),
                ),
                ("etc/group", b"root:x:0:\napp:x:1000:".as_slice()),
            ],
        );

        OciRootfsBuilder::new(&rootfs_path)
            .with_image(&image)
            .build()
            .unwrap();

        let passwd = fs::read_to_string(rootfs_path.join("etc/passwd")).unwrap();
        assert_eq!(entry_count(&passwd, "root"), 1);
        assert!(passwd.contains("root:x:0:0:Root User:/root:/bin/bash\n"));
        assert!(passwd.contains("app:x:1000:1000:App:/app:/bin/sh\n"));
        assert!(passwd.contains("nobody:x:65534:65534:nobody:/:/bin/false\n"));

        let group = fs::read_to_string(rootfs_path.join("etc/group")).unwrap();
        assert_eq!(entry_count(&group, "root"), 1);
        assert!(group.contains("root:x:0:\n"));
        assert!(group.contains("app:x:1000:\n"));
        assert!(group.contains("nogroup:x:65534:\n"));
    }

    #[test]
    fn test_install_guest_init_only_noop_without_guest_init() {
        let temp_dir = TempDir::new().unwrap();
        let rootfs_path = temp_dir.path().join("rootfs");

        OciRootfsBuilder::new(&rootfs_path)
            .install_guest_init_only()
            .unwrap();

        assert!(!rootfs_path.exists());
    }

    #[test]
    fn test_install_guest_init_only_replaces_existing_init_and_sets_permissions() {
        let temp_dir = TempDir::new().unwrap();
        let rootfs_path = temp_dir.path().join("rootfs");
        let guest_init = temp_dir.path().join("guest-init");

        fs::create_dir_all(rootfs_path.join("sbin")).unwrap();
        fs::write(rootfs_path.join("sbin/init"), b"old init").unwrap();
        fs::write(&guest_init, b"new guest init").unwrap();

        OciRootfsBuilder::new(&rootfs_path)
            .with_guest_init(&guest_init)
            .install_guest_init_only()
            .unwrap();

        let installed = rootfs_path.join("sbin/init");
        assert_eq!(fs::read(&installed).unwrap(), b"new guest init");

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&installed).unwrap().permissions().mode() & 0o777,
                0o755
            );
        }
    }

    #[test]
    fn test_install_guest_init_only_errors_when_source_missing() {
        let temp_dir = TempDir::new().unwrap();
        let rootfs_path = temp_dir.path().join("rootfs");
        let missing_guest_init = temp_dir.path().join("missing-guest-init");

        let err = OciRootfsBuilder::new(&rootfs_path)
            .with_guest_init(&missing_guest_init)
            .install_guest_init_only()
            .unwrap_err();

        assert!(err.to_string().contains("Guest init binary not found"));
    }

    #[test]
    fn test_oci_rootfs_builder_image_config_reads_oci_config() {
        let temp_dir = TempDir::new().unwrap();
        let image = temp_dir.path().join("image");
        create_test_oci_image(&image);

        let config = OciRootfsBuilder::new(temp_dir.path().join("rootfs"))
            .with_image(&image)
            .image_config()
            .unwrap();

        assert_eq!(
            config.entrypoint,
            Some(vec!["/usr/local/bin/app".to_string()])
        );
        assert_eq!(config.cmd, None);
        assert_eq!(
            config.env,
            vec![(
                "PATH".to_string(),
                "/usr/local/bin:/usr/bin:/bin".to_string()
            )]
        );
        assert_eq!(config.working_dir, Some("/app".to_string()));
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn test_install_guest_init_prefers_usr_sbin_when_sbin_missing() {
        let temp_dir = TempDir::new().unwrap();
        let rootfs_path = temp_dir.path().join("rootfs");
        let guest_init = temp_dir.path().join("guest-init");

        fs::create_dir_all(rootfs_path.join("usr").join("sbin")).unwrap();
        fs::write(&guest_init, b"guest-init").unwrap();

        let builder = OciRootfsBuilder {
            rootfs_path: rootfs_path.clone(),
            image_path: PathBuf::new(),
            guest_init_path: Some(guest_init),
        };

        builder.install_guest_init().unwrap();

        assert!(rootfs_path.join("usr").join("sbin").join("init").exists());
        assert!(!rootfs_path.join("sbin").exists());
    }

    // Helper: create a minimal test OCI image
    fn create_test_oci_image(path: &Path) {
        create_test_oci_image_with_file(path, "test.txt", b"test content");
    }

    fn create_test_oci_image_with_file(path: &Path, filename: &str, content: &[u8]) {
        create_test_oci_image_with_files(path, &[(filename, content)]);
    }

    fn entry_count(content: &str, name: &str) -> usize {
        content
            .lines()
            .filter(|line| line.split(':').next() == Some(name))
            .count()
    }

    fn create_test_oci_image_with_files(path: &Path, files: &[(&str, &[u8])]) {
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

            for (filename, content) in files {
                let mut header = tar::Header::new_gnu();
                header.set_size(content.len() as u64);
                header.set_mode(0o644);
                // uid/gid must be set or a root-side ownership-preserving extraction
                // can't parse the (blank) uid field. Real OCI layers always set them.
                header.set_uid(0);
                header.set_gid(0);
                header.set_cksum();

                builder
                    .append_data(&mut header, *filename, *content)
                    .unwrap();
            }
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
