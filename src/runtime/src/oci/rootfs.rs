//! OCI rootfs composition and building.
//!
//! Handles composing rootfs from multiple OCI images for agent and business code.

use a3s_box_core::error::{BoxError, Result};
use std::path::PathBuf;

use super::image::OciImage;
use super::layers::extract_layer;

/// Configuration for rootfs composition from OCI images.
#[derive(Debug, Clone)]
pub struct RootfsComposition {
    /// Agent OCI image path
    pub agent_image: PathBuf,

    /// Business code OCI image path (optional)
    pub business_image: Option<PathBuf>,

    /// Target directory for agent files
    pub agent_target: String,

    /// Target directory for business code files
    pub business_target: String,
}

impl Default for RootfsComposition {
    fn default() -> Self {
        Self {
            agent_image: PathBuf::new(),
            business_image: None,
            agent_target: "/agent".to_string(),
            business_target: "/workspace".to_string(),
        }
    }
}

/// Builder for creating rootfs from OCI images.
///
/// Supports composing rootfs from:
/// - Single agent OCI image
/// - Agent + business code OCI images
/// - Optional guest init binary for namespace isolation
///
/// Each image is extracted to its own target directory within the rootfs,
/// providing isolation between agent and business code environments.
pub struct OciRootfsBuilder {
    /// Target rootfs directory
    rootfs_path: PathBuf,

    /// Composition configuration
    composition: RootfsComposition,

    /// Path to guest init binary (optional)
    guest_init_path: Option<PathBuf>,

    /// Path to nsexec binary (optional)
    nsexec_path: Option<PathBuf>,
}

impl OciRootfsBuilder {
    /// Create a new OCI rootfs builder.
    ///
    /// # Arguments
    ///
    /// * `rootfs_path` - Target directory for the composed rootfs
    pub fn new(rootfs_path: impl Into<PathBuf>) -> Self {
        Self {
            rootfs_path: rootfs_path.into(),
            composition: RootfsComposition::default(),
            guest_init_path: None,
            nsexec_path: None,
        }
    }

    /// Set the agent OCI image path.
    pub fn with_agent_image(mut self, path: impl Into<PathBuf>) -> Self {
        self.composition.agent_image = path.into();
        self
    }

    /// Set the business code OCI image path.
    pub fn with_business_image(mut self, path: impl Into<PathBuf>) -> Self {
        self.composition.business_image = Some(path.into());
        self
    }

    /// Set the target directory for agent files within rootfs.
    pub fn with_agent_target(mut self, target: impl Into<String>) -> Self {
        self.composition.agent_target = target.into();
        self
    }

    /// Set the target directory for business code files within rootfs.
    pub fn with_business_target(mut self, target: impl Into<String>) -> Self {
        self.composition.business_target = target.into();
        self
    }

    /// Set the path to the guest init binary.
    ///
    /// If set, the guest init binary will be installed at `/sbin/init` in the rootfs.
    /// This enables namespace isolation for agent and business code.
    pub fn with_guest_init(mut self, path: impl Into<PathBuf>) -> Self {
        self.guest_init_path = Some(path.into());
        self
    }

    /// Set the path to the nsexec binary.
    ///
    /// If set, the nsexec binary will be installed at `/usr/bin/nsexec` in the rootfs.
    /// This allows the agent to execute business code in isolated namespaces.
    pub fn with_nsexec(mut self, path: impl Into<PathBuf>) -> Self {
        self.nsexec_path = Some(path.into());
        self
    }

    /// Build the rootfs by extracting OCI images.
    ///
    /// # Process
    ///
    /// 1. Create base directory structure
    /// 2. Extract agent image layers to agent target directory
    /// 3. Extract business image layers to business target directory (if provided)
    /// 4. Install guest init binary (if provided)
    /// 5. Install nsexec binary (if provided)
    /// 6. Create essential system files
    ///
    /// # Errors
    ///
    /// Returns error if:
    /// - Agent image is not set or invalid
    /// - Layer extraction fails
    /// - Directory creation fails
    pub fn build(&self) -> Result<()> {
        tracing::info!(
            rootfs = %self.rootfs_path.display(),
            "Building OCI rootfs"
        );

        // Validate agent image is set
        if self.composition.agent_image.as_os_str().is_empty() {
            return Err(BoxError::Other("Agent OCI image path not set".to_string()));
        }

        // Create base directory structure
        self.create_base_structure()?;

        // Extract agent image
        self.extract_agent_image()?;

        // Extract business image if provided
        if self.composition.business_image.is_some() {
            self.extract_business_image()?;
        }

        // Install guest init if provided
        if self.guest_init_path.is_some() {
            self.install_guest_init()?;
        }

        // Install nsexec if provided
        if self.nsexec_path.is_some() {
            self.install_nsexec()?;
        }

        // Create essential system files
        self.create_essential_files()?;

        tracing::info!("OCI rootfs built successfully");
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
            self.composition.agent_target.trim_start_matches('/'),
            self.composition.business_target.trim_start_matches('/'),
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

    /// Extract agent OCI image layers.
    fn extract_agent_image(&self) -> Result<()> {
        let image = OciImage::from_path(&self.composition.agent_image)?;
        let target_dir = self
            .rootfs_path
            .join(self.composition.agent_target.trim_start_matches('/'));

        tracing::info!(
            image = %self.composition.agent_image.display(),
            target = %target_dir.display(),
            layers = image.layer_paths().len(),
            "Extracting agent image"
        );

        // Extract layers in order (bottom to top)
        for layer_path in image.layer_paths() {
            extract_layer(layer_path, &target_dir)?;
        }

        Ok(())
    }

    /// Extract business code OCI image layers.
    fn extract_business_image(&self) -> Result<()> {
        let business_path = self
            .composition
            .business_image
            .as_ref()
            .ok_or_else(|| BoxError::Other("Business image not set".to_string()))?;

        let image = OciImage::from_path(business_path)?;
        let target_dir = self
            .rootfs_path
            .join(self.composition.business_target.trim_start_matches('/'));

        tracing::info!(
            image = %business_path.display(),
            target = %target_dir.display(),
            layers = image.layer_paths().len(),
            "Extracting business image"
        );

        // Extract layers in order (bottom to top)
        for layer_path in image.layer_paths() {
            extract_layer(layer_path, &target_dir)?;
        }

        Ok(())
    }

    /// Install guest init binary to /sbin/init.
    fn install_guest_init(&self) -> Result<()> {
        let guest_init_src = self
            .guest_init_path
            .as_ref()
            .ok_or_else(|| BoxError::Other("Guest init path not set".to_string()))?;

        // Validate source exists
        if !guest_init_src.exists() {
            return Err(BoxError::Other(format!(
                "Guest init binary not found: {}",
                guest_init_src.display()
            )));
        }

        // Create /sbin directory
        let sbin_dir = self.rootfs_path.join("sbin");
        std::fs::create_dir_all(&sbin_dir)
            .map_err(|e| BoxError::Other(format!("Failed to create /sbin directory: {}", e)))?;

        // Copy guest init to /sbin/init
        // Remove any existing file/symlink first (e.g., busybox symlink in Alpine)
        let init_path = sbin_dir.join("init");
        if init_path.exists() || init_path.symlink_metadata().is_ok() {
            std::fs::remove_file(&init_path).map_err(|e| {
                BoxError::Other(format!(
                    "Failed to remove existing {}: {}",
                    init_path.display(),
                    e
                ))
            })?;
        }
        std::fs::copy(guest_init_src, &init_path).map_err(|e| {
            BoxError::Other(format!(
                "Failed to copy guest init to {}: {}",
                init_path.display(),
                e
            ))
        })?;

        // Make executable (chmod +x)
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&init_path)
                .map_err(|e| BoxError::Other(format!("Failed to get permissions: {}", e)))?
                .permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&init_path, perms)
                .map_err(|e| BoxError::Other(format!("Failed to set permissions: {}", e)))?;
        }

        tracing::info!(
            src = %guest_init_src.display(),
            dst = %init_path.display(),
            "Installed guest init"
        );

        Ok(())
    }

    /// Install nsexec binary to /usr/bin/nsexec.
    fn install_nsexec(&self) -> Result<()> {
        let nsexec_src = self
            .nsexec_path
            .as_ref()
            .ok_or_else(|| BoxError::Other("Nsexec path not set".to_string()))?;

        // Validate source exists
        if !nsexec_src.exists() {
            return Err(BoxError::Other(format!(
                "Nsexec binary not found: {}",
                nsexec_src.display()
            )));
        }

        // Create /usr/bin directory
        let usr_bin_dir = self.rootfs_path.join("usr/bin");
        std::fs::create_dir_all(&usr_bin_dir)
            .map_err(|e| BoxError::Other(format!("Failed to create /usr/bin directory: {}", e)))?;

        // Copy nsexec to /usr/bin/nsexec
        // Remove any existing file/symlink first
        let nsexec_path = usr_bin_dir.join("nsexec");
        if nsexec_path.exists() || nsexec_path.symlink_metadata().is_ok() {
            std::fs::remove_file(&nsexec_path).ok();
        }
        std::fs::copy(nsexec_src, &nsexec_path).map_err(|e| {
            BoxError::Other(format!(
                "Failed to copy nsexec to {}: {}",
                nsexec_path.display(),
                e
            ))
        })?;

        // Make executable (chmod +x)
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&nsexec_path)
                .map_err(|e| BoxError::Other(format!("Failed to get permissions: {}", e)))?
                .permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&nsexec_path, perms)
                .map_err(|e| BoxError::Other(format!("Failed to set permissions: {}", e)))?;
        }

        tracing::info!(
            src = %nsexec_src.display(),
            dst = %nsexec_path.display(),
            "Installed nsexec"
        );

        Ok(())
    }

    /// Create essential system files.
    ///
    /// For /etc/passwd and /etc/group: preserve existing entries from the OCI
    /// image (e.g., nginx user in nginx:alpine) and only ensure essential
    /// entries (root, nobody) exist.
    fn create_essential_files(&self) -> Result<()> {
        // /etc/passwd - ensure essential entries exist, preserve image entries
        self.ensure_passwd_entries(&[
            ("root", "root:x:0:0:root:/root:/bin/sh"),
            ("nobody", "nobody:x:65534:65534:nobody:/:/bin/false"),
        ])?;

        // /etc/group - ensure essential entries exist, preserve image entries
        self.ensure_group_entries(&[("root", "root:x:0:"), ("nogroup", "nogroup:x:65534:")])?;

        // /etc/hosts - basic hosts file
        let hosts_content = "127.0.0.1\tlocalhost\n::1\t\tlocalhost\n";
        self.write_file("etc/hosts", hosts_content)?;

        // /etc/resolv.conf - DNS configuration
        let resolv_content = "nameserver 8.8.8.8\nnameserver 8.8.4.4\n";
        self.write_file("etc/resolv.conf", resolv_content)?;

        // /etc/nsswitch.conf - name service switch configuration
        let nsswitch_content = "passwd: files\ngroup: files\nhosts: files dns\n";
        self.write_file("etc/nsswitch.conf", nsswitch_content)?;

        Ok(())
    }

    /// Ensure essential entries exist in /etc/passwd, preserving image entries.
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

    /// Ensure essential entries exist in /etc/group, preserving image entries.
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

    /// Write a file to the rootfs.
    fn write_file(&self, relative_path: &str, content: &str) -> Result<()> {
        let full_path = self.rootfs_path.join(relative_path);

        // Ensure parent directory exists
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

    /// Get the agent OCI image configuration.
    ///
    /// Useful for extracting entrypoint, environment, etc.
    pub fn agent_config(&self) -> Result<super::image::OciImageConfig> {
        let image = OciImage::from_path(&self.composition.agent_image)?;
        Ok(image.config().clone())
    }

    /// Get the business code OCI image configuration.
    pub fn business_config(&self) -> Result<Option<super::image::OciImageConfig>> {
        match &self.composition.business_image {
            Some(path) => {
                let image = OciImage::from_path(path)?;
                Ok(Some(image.config().clone()))
            }
            None => Ok(None),
        }
    }

    /// Check if guest init is configured.
    pub fn has_guest_init(&self) -> bool {
        self.guest_init_path.is_some()
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
        let agent_image = temp_dir.path().join("agent-image");

        // Create a minimal agent image
        create_test_oci_image(&agent_image);

        let builder = OciRootfsBuilder::new(&rootfs_path).with_agent_image(&agent_image);

        builder.build().unwrap();

        // Verify base directories were created
        assert!(rootfs_path.join("dev").exists());
        assert!(rootfs_path.join("proc").exists());
        assert!(rootfs_path.join("sys").exists());
        assert!(rootfs_path.join("tmp").exists());
        assert!(rootfs_path.join("etc").exists());
        assert!(rootfs_path.join("agent").exists());
        assert!(rootfs_path.join("workspace").exists());
    }

    #[test]
    fn test_oci_rootfs_builder_creates_essential_files() {
        let temp_dir = TempDir::new().unwrap();
        let rootfs_path = temp_dir.path().join("rootfs");
        let agent_image = temp_dir.path().join("agent-image");

        create_test_oci_image(&agent_image);

        let builder = OciRootfsBuilder::new(&rootfs_path).with_agent_image(&agent_image);

        builder.build().unwrap();

        // Verify essential files were created
        assert!(rootfs_path.join("etc/passwd").exists());
        assert!(rootfs_path.join("etc/group").exists());
        assert!(rootfs_path.join("etc/hosts").exists());
        assert!(rootfs_path.join("etc/resolv.conf").exists());

        // Verify content
        let passwd = fs::read_to_string(rootfs_path.join("etc/passwd")).unwrap();
        assert!(passwd.contains("root:x:0:0"));
    }

    #[test]
    fn test_oci_rootfs_builder_extracts_agent_image() {
        let temp_dir = TempDir::new().unwrap();
        let rootfs_path = temp_dir.path().join("rootfs");
        let agent_image = temp_dir.path().join("agent-image");

        // Create agent image with a test file
        create_test_oci_image_with_file(&agent_image, "agent.py", b"print('hello')");

        let builder = OciRootfsBuilder::new(&rootfs_path).with_agent_image(&agent_image);

        builder.build().unwrap();

        // Verify agent file was extracted
        let agent_file = rootfs_path.join("agent/agent.py");
        assert!(agent_file.exists());
        let content = fs::read_to_string(agent_file).unwrap();
        assert_eq!(content, "print('hello')");
    }

    #[test]
    fn test_oci_rootfs_builder_extracts_both_images() {
        let temp_dir = TempDir::new().unwrap();
        let rootfs_path = temp_dir.path().join("rootfs");
        let agent_image = temp_dir.path().join("agent-image");
        let business_image = temp_dir.path().join("business-image");

        // Create both images
        create_test_oci_image_with_file(&agent_image, "agent.py", b"agent code");
        create_test_oci_image_with_file(&business_image, "app.js", b"business code");

        let builder = OciRootfsBuilder::new(&rootfs_path)
            .with_agent_image(&agent_image)
            .with_business_image(&business_image);

        builder.build().unwrap();

        // Verify both files were extracted to correct locations
        assert!(rootfs_path.join("agent/agent.py").exists());
        assert!(rootfs_path.join("workspace/app.js").exists());

        let agent_content = fs::read_to_string(rootfs_path.join("agent/agent.py")).unwrap();
        assert_eq!(agent_content, "agent code");

        let business_content = fs::read_to_string(rootfs_path.join("workspace/app.js")).unwrap();
        assert_eq!(business_content, "business code");
    }

    #[test]
    fn test_oci_rootfs_builder_custom_targets() {
        let temp_dir = TempDir::new().unwrap();
        let rootfs_path = temp_dir.path().join("rootfs");
        let agent_image = temp_dir.path().join("agent-image");

        create_test_oci_image_with_file(&agent_image, "main.rs", b"fn main() {}");

        let builder = OciRootfsBuilder::new(&rootfs_path)
            .with_agent_image(&agent_image)
            .with_agent_target("/custom/agent")
            .with_business_target("/custom/app");

        builder.build().unwrap();

        // Verify custom directories were created
        assert!(rootfs_path.join("custom/agent").exists());
        assert!(rootfs_path.join("custom/app").exists());

        // Verify file was extracted to custom location
        assert!(rootfs_path.join("custom/agent/main.rs").exists());
    }

    #[test]
    fn test_oci_rootfs_builder_no_agent_image() {
        let temp_dir = TempDir::new().unwrap();
        let rootfs_path = temp_dir.path().join("rootfs");

        let builder = OciRootfsBuilder::new(&rootfs_path);

        let result = builder.build();

        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("Agent OCI image"));
    }

    // Helper function to create a minimal test OCI image
    fn create_test_oci_image(path: &Path) {
        create_test_oci_image_with_file(path, "test.txt", b"test content");
    }

    // Helper function to create a test OCI image with a specific file
    fn create_test_oci_image_with_file(path: &Path, filename: &str, content: &[u8]) {
        use flate2::write::GzEncoder;
        use flate2::Compression;
        use tar::Builder;

        // Create directory structure
        fs::create_dir_all(path.join("blobs/sha256")).unwrap();

        // Create oci-layout
        fs::write(path.join("oci-layout"), r#"{"imageLayoutVersion":"1.0.0"}"#).unwrap();

        // Create layer blob with the specified file
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

        // Create config blob
        let config_content = r#"{
            "architecture": "amd64",
            "os": "linux",
            "config": {
                "Entrypoint": ["/bin/agent"],
                "Cmd": ["--port", "8080"],
                "Env": ["PATH=/usr/bin"],
                "WorkingDir": "/workspace"
            },
            "rootfs": {
                "type": "layers",
                "diff_ids": ["sha256:layer123"]
            },
            "history": []
        }"#;
        let config_hash = "config456";
        fs::write(path.join("blobs/sha256").join(config_hash), config_content).unwrap();

        // Create manifest blob
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

        // Create index.json
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
