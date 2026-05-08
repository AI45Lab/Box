//! OCI image parsing and representation.
//!
//! Handles parsing of OCI image layout including manifest and configuration.

use a3s_box_core::error::{BoxError, Result};
use oci_spec::image::{ImageConfiguration, ImageIndex, ImageManifest};
use std::path::{Path, PathBuf};

/// Health check configuration from OCI image config.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OciHealthCheck {
    pub test: Vec<String>,
    pub interval: Option<u64>,
    pub timeout: Option<u64>,
    pub retries: Option<u32>,
    pub start_period: Option<u64>,
}

/// Represents an OCI image loaded from disk.
#[derive(Debug)]
pub struct OciImage {
    /// Root directory of the OCI image layout
    root_dir: PathBuf,

    /// Manifest digest (e.g. "sha256:abc123...")
    manifest_digest: String,

    /// Image configuration
    config: OciImageConfig,

    /// Paths to layer blobs (in order, bottom to top)
    layer_paths: Vec<PathBuf>,
}

/// Parsed OCI image configuration with entrypoint and environment.
#[derive(Debug, Clone)]
pub struct OciImageConfig {
    /// Entrypoint command
    pub entrypoint: Option<Vec<String>>,

    /// Default command arguments
    pub cmd: Option<Vec<String>>,

    /// Environment variables
    pub env: Vec<(String, String)>,

    /// Working directory
    pub working_dir: Option<String>,

    /// User to run as
    pub user: Option<String>,

    /// Exposed ports
    pub exposed_ports: Vec<String>,

    /// Labels
    pub labels: std::collections::HashMap<String, String>,

    /// Volumes declared in the image (OCI VOLUME directive)
    pub volumes: Vec<String>,

    /// Stop signal
    pub stop_signal: Option<String>,

    /// Health check configuration
    pub health_check: Option<OciHealthCheck>,

    /// ONBUILD triggers
    pub onbuild: Vec<String>,
}

impl OciImage {
    /// Load an OCI image from a directory.
    ///
    /// The directory must contain a valid OCI image layout:
    /// - oci-layout file
    /// - index.json
    /// - blobs/sha256/ directory with manifest, config, and layers
    ///
    /// # Arguments
    ///
    /// * `path` - Path to the OCI image directory
    ///
    /// # Errors
    ///
    /// Returns error if:
    /// - Directory doesn't exist
    /// - OCI layout is invalid
    /// - Manifest or config cannot be parsed
    pub fn from_path(path: impl AsRef<Path>) -> Result<Self> {
        let root_dir = path.as_ref().to_path_buf();

        // Validate OCI layout
        Self::validate_oci_layout(&root_dir)?;

        // Load index.json
        let index = Self::load_index(&root_dir)?;

        // Get manifest digest from index
        let manifest_digest = index
            .manifests()
            .first()
            .ok_or_else(|| BoxError::OciImageError("No manifests in index.json".to_string()))?
            .digest()
            .to_string();

        // Load manifest
        let manifest = Self::load_manifest(&root_dir, &manifest_digest)?;

        // Load config
        let config_digest = manifest.config().digest().to_string();
        let config = Self::load_config(&root_dir, &config_digest)?;

        // Get layer paths
        let layer_paths = manifest
            .layers()
            .iter()
            .map(|layer| Self::blob_path(&root_dir, layer.digest()))
            .collect();

        Ok(Self {
            root_dir,
            manifest_digest,
            config,
            layer_paths,
        })
    }

    /// Get the image configuration.
    pub fn config(&self) -> &OciImageConfig {
        &self.config
    }

    /// Get paths to all layer blobs (in order, bottom to top).
    pub fn layer_paths(&self) -> &[PathBuf] {
        &self.layer_paths
    }

    /// Get the root directory of the OCI image.
    pub fn root_dir(&self) -> &Path {
        &self.root_dir
    }

    /// Get the manifest digest (e.g. `"sha256:abc123..."`).
    pub fn manifest_digest(&self) -> &str {
        &self.manifest_digest
    }

    /// Get the entrypoint command.
    ///
    /// Returns the entrypoint from config, or None if not set.
    pub fn entrypoint(&self) -> Option<&[String]> {
        self.config.entrypoint.as_deref()
    }

    /// Get the default command arguments.
    pub fn cmd(&self) -> Option<&[String]> {
        self.config.cmd.as_deref()
    }

    /// Get environment variables.
    pub fn env(&self) -> &[(String, String)] {
        &self.config.env
    }

    /// Get the working directory.
    pub fn working_dir(&self) -> Option<&str> {
        self.config.working_dir.as_deref()
    }

    /// Get a label value by key.
    pub fn label(&self, key: &str) -> Option<&str> {
        self.config.labels.get(key).map(|s| s.as_str())
    }

    /// Validate that the directory contains a valid OCI layout.
    fn validate_oci_layout(root_dir: &Path) -> Result<()> {
        // Check oci-layout file exists
        let oci_layout_path = root_dir.join("oci-layout");
        if !oci_layout_path.exists() {
            return Err(BoxError::OciImageError(format!(
                "Not a valid OCI layout: missing oci-layout file in {}",
                root_dir.display()
            )));
        }

        // Check index.json exists
        let index_path = root_dir.join("index.json");
        if !index_path.exists() {
            return Err(BoxError::OciImageError(format!(
                "Not a valid OCI layout: missing index.json in {}",
                root_dir.display()
            )));
        }

        // Check blobs directory exists
        let blobs_dir = root_dir.join("blobs");
        if !blobs_dir.exists() {
            return Err(BoxError::OciImageError(format!(
                "Not a valid OCI layout: missing blobs directory in {}",
                root_dir.display()
            )));
        }

        Ok(())
    }

    /// Load the image index from index.json.
    fn load_index(root_dir: &Path) -> Result<ImageIndex> {
        let index_path = root_dir.join("index.json");
        let content = std::fs::read_to_string(&index_path).map_err(|e| {
            BoxError::OciImageError(format!(
                "Failed to read index.json at {}: {}",
                index_path.display(),
                e
            ))
        })?;

        serde_json::from_str(&content)
            .map_err(|e| BoxError::OciImageError(format!("Failed to parse index.json: {}", e)))
    }

    /// Load the image manifest from blobs.
    fn load_manifest(root_dir: &Path, digest: &str) -> Result<ImageManifest> {
        let blob_path = Self::blob_path(root_dir, digest);
        let content = std::fs::read_to_string(&blob_path).map_err(|e| {
            BoxError::OciImageError(format!(
                "Failed to read manifest at {}: {}",
                blob_path.display(),
                e
            ))
        })?;

        serde_json::from_str(&content)
            .map_err(|e| BoxError::OciImageError(format!("Failed to parse manifest: {}", e)))
    }

    /// Load the image configuration from blobs.
    fn load_config(root_dir: &Path, digest: &str) -> Result<OciImageConfig> {
        let blob_path = Self::blob_path(root_dir, digest);
        let content = std::fs::read_to_string(&blob_path).map_err(|e| {
            BoxError::OciImageError(format!(
                "Failed to read config at {}: {}",
                blob_path.display(),
                e
            ))
        })?;

        let oci_config: ImageConfiguration = serde_json::from_str(&content)
            .map_err(|e| BoxError::OciImageError(format!("Failed to parse config: {}", e)))?;

        let raw_config: serde_json::Value = serde_json::from_str(&content)
            .map_err(|e| BoxError::OciImageError(format!("Failed to parse config JSON: {}", e)))?;

        // oci-spec 0.6 does not model OnBuild or Healthcheck, so parse those
        // Docker-compatible image fields directly from raw JSON.
        let onbuild: Vec<String> = raw_config
            .get("config")
            .and_then(|c| c.get("OnBuild"))
            .cloned()
            .and_then(|v| serde_json::from_value(v).ok())
            .unwrap_or_default();
        let health_check = Self::parse_health_check_from_raw(&raw_config);

        let mut config = OciImageConfig::from_oci_config(&oci_config, onbuild);
        config.health_check = health_check;
        Ok(config)
    }

    /// Get the path to a blob by digest.
    fn blob_path(root_dir: &Path, digest: &str) -> PathBuf {
        // Digest format: "sha256:abc123..."
        let parts: Vec<&str> = digest.split(':').collect();
        let (algorithm, hash) = if parts.len() == 2 {
            (parts[0], parts[1])
        } else {
            ("sha256", digest)
        };

        root_dir.join("blobs").join(algorithm).join(hash)
    }

    /// Parse Docker-compatible Healthcheck metadata from raw image config JSON.
    fn parse_health_check_from_raw(raw_config: &serde_json::Value) -> Option<OciHealthCheck> {
        let health = raw_config
            .get("config")
            .and_then(|c| c.get("Healthcheck").or_else(|| c.get("healthcheck")))?;

        let test = health.get("Test").or_else(|| health.get("test"))?;
        let test: Vec<String> = serde_json::from_value(test.clone()).ok()?;
        if test.is_empty() {
            return None;
        }

        if test
            .first()
            .is_some_and(|marker| marker.eq_ignore_ascii_case("NONE"))
        {
            return None;
        }

        Some(OciHealthCheck {
            test,
            interval: health
                .get("Interval")
                .or_else(|| health.get("interval"))
                .and_then(duration_seconds_from_json),
            timeout: health
                .get("Timeout")
                .or_else(|| health.get("timeout"))
                .and_then(duration_seconds_from_json),
            retries: health
                .get("Retries")
                .or_else(|| health.get("retries"))
                .and_then(u32_from_json)
                .filter(|value| *value > 0),
            start_period: health
                .get("StartPeriod")
                .or_else(|| health.get("start_period"))
                .and_then(duration_seconds_from_json),
        })
    }
}

fn duration_seconds_from_json(value: &serde_json::Value) -> Option<u64> {
    let nanos = u64_from_json(value)?;
    if nanos == 0 {
        return None;
    }
    Some(nanos.div_ceil(1_000_000_000).max(1))
}

fn u64_from_json(value: &serde_json::Value) -> Option<u64> {
    value
        .as_u64()
        .or_else(|| value.as_str().and_then(|s| s.parse::<u64>().ok()))
}

fn u32_from_json(value: &serde_json::Value) -> Option<u32> {
    u64_from_json(value).and_then(|value| u32::try_from(value).ok())
}

impl OciImageConfig {
    /// Create from OCI spec ImageConfiguration.
    fn from_oci_config(oci_config: &ImageConfiguration, onbuild: Vec<String>) -> Self {
        let config = oci_config.config();

        let entrypoint = config.as_ref().and_then(|c| c.entrypoint().clone());
        let cmd = config.as_ref().and_then(|c| c.cmd().clone());
        let working_dir = config.as_ref().and_then(|c| c.working_dir().clone());
        let user = config.as_ref().and_then(|c| c.user().clone());

        // Parse environment variables
        let env = config
            .as_ref()
            .and_then(|c| c.env().as_ref())
            .map(|env_list| {
                env_list
                    .iter()
                    .filter_map(|e| {
                        let parts: Vec<&str> = e.splitn(2, '=').collect();
                        if parts.len() == 2 {
                            Some((parts[0].to_string(), parts[1].to_string()))
                        } else {
                            None
                        }
                    })
                    .collect()
            })
            .unwrap_or_default();

        // Parse exposed ports
        let exposed_ports = config
            .as_ref()
            .and_then(|c| c.exposed_ports().as_ref())
            .map(|ports| ports.to_vec())
            .unwrap_or_default();

        // Parse labels
        let labels = config
            .as_ref()
            .and_then(|c| c.labels().clone())
            .unwrap_or_default();

        // Parse volumes (OCI VOLUME directive)
        let volumes = config
            .as_ref()
            .and_then(|c| c.volumes().as_ref())
            .map(|vols| vols.to_vec())
            .unwrap_or_default();

        // Parse stop signal
        let stop_signal = config.as_ref().and_then(|c| c.stop_signal().clone());

        // Healthcheck is filled by load_config from raw JSON because oci-spec
        // 0.6 does not expose the Docker-compatible field.
        let health_check = None;

        Self {
            entrypoint,
            cmd,
            env,
            working_dir,
            user,
            exposed_ports,
            labels,
            volumes,
            stop_signal,
            health_check,
            onbuild,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn test_validate_oci_layout_missing_oci_layout_file() {
        let temp_dir = TempDir::new().unwrap();

        let result = OciImage::validate_oci_layout(temp_dir.path());

        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("oci-layout"));
    }

    #[test]
    fn test_validate_oci_layout_missing_index_json() {
        let temp_dir = TempDir::new().unwrap();

        // Create oci-layout file
        fs::write(
            temp_dir.path().join("oci-layout"),
            r#"{"imageLayoutVersion":"1.0.0"}"#,
        )
        .unwrap();

        let result = OciImage::validate_oci_layout(temp_dir.path());

        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("index.json"));
    }

    #[test]
    fn test_validate_oci_layout_missing_blobs() {
        let temp_dir = TempDir::new().unwrap();

        // Create oci-layout file
        fs::write(
            temp_dir.path().join("oci-layout"),
            r#"{"imageLayoutVersion":"1.0.0"}"#,
        )
        .unwrap();

        // Create index.json
        fs::write(temp_dir.path().join("index.json"), "{}").unwrap();

        let result = OciImage::validate_oci_layout(temp_dir.path());

        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("blobs"));
    }

    #[test]
    fn test_validate_oci_layout_valid() {
        let temp_dir = TempDir::new().unwrap();

        // Create valid OCI layout structure
        create_minimal_oci_layout(temp_dir.path());

        let result = OciImage::validate_oci_layout(temp_dir.path());

        assert!(result.is_ok());
    }

    #[test]
    fn test_blob_path() {
        let root = PathBuf::from("/images/test");

        let path = OciImage::blob_path(&root, "sha256:abc123");
        assert_eq!(path, PathBuf::from("/images/test/blobs/sha256/abc123"));

        let path = OciImage::blob_path(&root, "abc123");
        assert_eq!(path, PathBuf::from("/images/test/blobs/sha256/abc123"));
    }

    #[test]
    fn test_from_path_valid_image() {
        let temp_dir = TempDir::new().unwrap();

        // Create a complete OCI image layout
        create_complete_oci_image(temp_dir.path());

        let image = OciImage::from_path(temp_dir.path()).unwrap();

        // Verify config was parsed
        assert_eq!(image.entrypoint(), Some(&["/bin/agent".to_string()][..]));
        assert_eq!(
            image.cmd(),
            Some(&["--port".to_string(), "8080".to_string()][..])
        );
        assert_eq!(image.working_dir(), Some("/workspace"));

        // Verify env was parsed
        let env = image.env();
        assert!(env
            .iter()
            .any(|(k, v)| k == "PATH" && v.contains("/usr/bin")));

        // Verify labels
        assert_eq!(image.label("a3s.type"), Some("agent"));

        // Verify Docker-compatible Healthcheck was parsed from raw config JSON.
        let health_check = image.config().health_check.as_ref().unwrap();
        assert_eq!(
            health_check.test,
            vec!["CMD-SHELL".to_string(), "test -f /tmp/healthy".to_string()]
        );
        assert_eq!(health_check.interval, Some(30));
        assert_eq!(health_check.timeout, Some(2));
        assert_eq!(health_check.retries, Some(2));
        assert_eq!(health_check.start_period, Some(5));

        // Verify layer paths
        assert_eq!(image.layer_paths().len(), 1);
    }

    #[test]
    fn test_from_path_exposes_manifest_digest() {
        let temp_dir = TempDir::new().unwrap();
        create_complete_oci_image(temp_dir.path());
        let image = OciImage::from_path(temp_dir.path()).unwrap();
        assert_eq!(image.manifest_digest(), "sha256:manifestxyz789");
    }

    #[test]
    fn test_from_path_nonexistent() {
        let result = OciImage::from_path("/nonexistent/path");

        assert!(result.is_err());
    }

    // Helper function to create minimal OCI layout structure
    fn create_minimal_oci_layout(path: &Path) {
        fs::write(path.join("oci-layout"), r#"{"imageLayoutVersion":"1.0.0"}"#).unwrap();

        fs::write(path.join("index.json"), "{}").unwrap();

        fs::create_dir_all(path.join("blobs/sha256")).unwrap();
    }

    // Helper function to create a complete OCI image for testing
    fn create_complete_oci_image(path: &Path) {
        // Create directory structure
        fs::create_dir_all(path.join("blobs/sha256")).unwrap();

        // Create oci-layout
        fs::write(path.join("oci-layout"), r#"{"imageLayoutVersion":"1.0.0"}"#).unwrap();

        // Create config blob
        let config_content = r#"{
            "architecture": "amd64",
            "os": "linux",
            "config": {
                "Entrypoint": ["/bin/agent"],
                "Cmd": ["--port", "8080"],
                "Env": ["PATH=/usr/local/bin:/usr/bin:/bin"],
                "WorkingDir": "/workspace",
                "Labels": {
                    "a3s.type": "agent",
                    "a3s.version": "1.0.0"
                },
                "Healthcheck": {
                    "Test": ["CMD-SHELL", "test -f /tmp/healthy"],
                    "Interval": 30000000000,
                    "Timeout": 1500000000,
                    "Retries": 2,
                    "StartPeriod": 5000000000
                }
            },
            "rootfs": {
                "type": "layers",
                "diff_ids": ["sha256:layer1hash"]
            },
            "history": []
        }"#;
        let config_hash = "configabc123";
        fs::write(path.join("blobs/sha256").join(config_hash), config_content).unwrap();

        // Create layer blob (empty tar.gz for testing)
        let layer_hash = "layerdef456";
        create_test_layer(&path.join("blobs/sha256").join(layer_hash));

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
        let manifest_hash = "manifestxyz789";
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

    #[test]
    fn test_from_oci_config_parses_volumes() {
        // Directly test OciImageConfig::from_oci_config with volumes
        let config_json = r#"{
            "architecture": "amd64",
            "os": "linux",
            "config": {
                "Volumes": {
                    "/data": {},
                    "/var/log": {}
                }
            },
            "rootfs": {
                "type": "layers",
                "diff_ids": []
            },
            "history": []
        }"#;
        let oci_config: oci_spec::image::ImageConfiguration =
            serde_json::from_str(config_json).unwrap();
        let config = OciImageConfig::from_oci_config(&oci_config, Vec::new());
        assert_eq!(config.volumes.len(), 2);
        assert!(config.volumes.contains(&"/data".to_string()));
        assert!(config.volumes.contains(&"/var/log".to_string()));
    }

    #[test]
    fn test_from_oci_config_no_volumes() {
        let config_json = r#"{
            "architecture": "amd64",
            "os": "linux",
            "config": {},
            "rootfs": {
                "type": "layers",
                "diff_ids": []
            },
            "history": []
        }"#;
        let oci_config: oci_spec::image::ImageConfiguration =
            serde_json::from_str(config_json).unwrap();
        let config = OciImageConfig::from_oci_config(&oci_config, Vec::new());
        assert!(config.volumes.is_empty());
    }

    #[test]
    fn test_parse_health_check_cmd() {
        let raw = serde_json::json!({
            "config": {
                "Healthcheck": {
                    "Test": ["CMD", "curl", "-f", "http://localhost/"],
                    "Interval": 30000000000u64,
                    "Timeout": 5000000000u64,
                    "Retries": 3u64,
                    "StartPeriod": 0u64
                }
            }
        });

        let hc = OciImage::parse_health_check_from_raw(&raw).unwrap();
        assert_eq!(hc.test, vec!["CMD", "curl", "-f", "http://localhost/"]);
        assert_eq!(hc.interval, Some(30));
        assert_eq!(hc.timeout, Some(5));
        assert_eq!(hc.retries, Some(3));
        assert_eq!(hc.start_period, None);
    }

    #[test]
    fn test_parse_health_check_cmd_shell_and_ceil_durations() {
        let raw = serde_json::json!({
            "config": {
                "Healthcheck": {
                    "Test": ["CMD-SHELL", "wget -qO- http://localhost/health"],
                    "Interval": 1500000000u64,
                    "Timeout": "1",
                    "Retries": "2",
                    "StartPeriod": 1u64
                }
            }
        });

        let hc = OciImage::parse_health_check_from_raw(&raw).unwrap();
        assert_eq!(
            hc.test,
            vec!["CMD-SHELL", "wget -qO- http://localhost/health"]
        );
        assert_eq!(hc.interval, Some(2));
        assert_eq!(hc.timeout, Some(1));
        assert_eq!(hc.retries, Some(2));
        assert_eq!(hc.start_period, Some(1));
    }

    #[test]
    fn test_parse_health_check_none_disables() {
        let raw = serde_json::json!({
            "config": {
                "Healthcheck": {
                    "Test": ["NONE"]
                }
            }
        });

        assert!(OciImage::parse_health_check_from_raw(&raw).is_none());
    }

    #[test]
    fn test_load_config_parses_onbuild_triggers() {
        // Verify that OnBuild entries in the raw OCI config JSON are parsed
        // and surfaced in OciImageConfig.onbuild (oci-spec 0.6 doesn't model this field).
        let temp_dir = TempDir::new().unwrap();
        fs::create_dir_all(temp_dir.path().join("blobs/sha256")).unwrap();
        fs::write(
            temp_dir.path().join("oci-layout"),
            r#"{"imageLayoutVersion":"1.0.0"}"#,
        )
        .unwrap();

        let config_content = r#"{
            "architecture": "amd64",
            "os": "linux",
            "config": {
                "OnBuild": ["RUN echo hello", "COPY . /app"]
            },
            "rootfs": {"type": "layers", "diff_ids": []},
            "history": []
        }"#;
        let config_hash = "onbuildcfg001";
        fs::write(
            temp_dir.path().join("blobs/sha256").join(config_hash),
            config_content,
        )
        .unwrap();

        let manifest_content = format!(
            r#"{{"schemaVersion":2,"config":{{"mediaType":"application/vnd.oci.image.config.v1+json","digest":"sha256:{}","size":{}}},"layers":[]}}"#,
            config_hash,
            config_content.len()
        );
        let manifest_hash = "onbuildmfst001";
        fs::write(
            temp_dir.path().join("blobs/sha256").join(manifest_hash),
            &manifest_content,
        )
        .unwrap();

        let index_content = format!(
            r#"{{"schemaVersion":2,"manifests":[{{"mediaType":"application/vnd.oci.image.manifest.v1+json","digest":"sha256:{}","size":{}}}]}}"#,
            manifest_hash,
            manifest_content.len()
        );
        fs::write(temp_dir.path().join("index.json"), index_content).unwrap();

        let image = OciImage::from_path(temp_dir.path()).unwrap();
        assert_eq!(
            image.config().onbuild,
            vec!["RUN echo hello", "COPY . /app"]
        );
    }

    // Helper function to create a test layer (minimal tar.gz)
    fn create_test_layer(path: &Path) {
        use flate2::write::GzEncoder;
        use flate2::Compression;
        use tar::Builder;

        let file = fs::File::create(path).unwrap();
        let encoder = GzEncoder::new(file, Compression::default());
        let mut builder = Builder::new(encoder);

        // Add a simple file
        let mut header = tar::Header::new_gnu();
        header.set_size(5);
        header.set_mode(0o644);
        header.set_cksum();

        builder
            .append_data(&mut header, "test.txt", b"hello" as &[u8])
            .unwrap();
        builder.finish().unwrap();
    }
}
