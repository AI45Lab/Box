//! VM Snapshot Types — Configuration-based snapshot metadata.
//!
//! Snapshots capture the full VM configuration (not memory state) so a box
//! can be reconstructed from the saved spec. Combined with rootfs caching,
//! restore achieves sub-500ms cold start.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;

/// Metadata for a saved VM snapshot.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SnapshotMetadata {
    /// Unique snapshot identifier
    pub id: String,
    /// User-assigned name (or auto-generated)
    pub name: String,
    /// Box ID this snapshot was taken from
    pub source_box_id: String,
    /// OCI image reference used by the source box
    pub image: String,
    /// Number of vCPUs
    pub vcpus: u32,
    /// Memory in MB
    pub memory_mb: u32,
    /// Volume mounts (host:guest pairs)
    pub volumes: Vec<String>,
    /// Environment variables
    pub env: HashMap<String, String>,
    /// Command override
    pub cmd: Vec<String>,
    /// Entrypoint override
    #[serde(default)]
    pub entrypoint: Option<Vec<String>>,
    /// Working directory inside the box
    #[serde(default)]
    pub workdir: Option<String>,
    /// Port mappings
    #[serde(default)]
    pub port_map: Vec<String>,
    /// User-defined labels
    #[serde(default)]
    pub labels: HashMap<String, String>,
    /// Network mode
    #[serde(default)]
    pub network_mode: Option<String>,
    /// Rootfs cache key (for fast restore via cached rootfs)
    #[serde(default)]
    pub rootfs_cache_key: Option<String>,
    /// Size of the snapshot on disk in bytes
    pub size_bytes: u64,
    /// Creation timestamp
    pub created_at: DateTime<Utc>,
    /// User-provided description
    #[serde(default)]
    pub description: String,
}

impl SnapshotMetadata {
    /// Create a new snapshot metadata with required fields.
    pub fn new(id: String, name: String, source_box_id: String, image: String) -> Self {
        Self {
            id,
            name,
            source_box_id,
            image,
            vcpus: 2,
            memory_mb: 512,
            volumes: Vec::new(),
            env: HashMap::new(),
            cmd: Vec::new(),
            entrypoint: None,
            workdir: None,
            port_map: Vec::new(),
            labels: HashMap::new(),
            network_mode: None,
            rootfs_cache_key: None,
            size_bytes: 0,
            created_at: Utc::now(),
            description: String::new(),
        }
    }

    /// Set description.
    pub fn with_description(mut self, desc: &str) -> Self {
        self.description = desc.to_string();
        self
    }

    /// Set resources.
    pub fn with_resources(mut self, vcpus: u32, memory_mb: u32) -> Self {
        self.vcpus = vcpus;
        self.memory_mb = memory_mb;
        self
    }
}

/// Configuration for snapshot operations.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SnapshotConfig {
    /// Whether snapshots are enabled
    pub enabled: bool,
    /// Directory to store snapshots (default: ~/.a3s/snapshots)
    pub snapshot_dir: Option<PathBuf>,
    /// Maximum number of snapshots to keep (0 = unlimited)
    pub max_snapshots: usize,
    /// Maximum total size in bytes (0 = unlimited)
    pub max_total_bytes: u64,
}

impl Default for SnapshotConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            snapshot_dir: None,
            max_snapshots: 0,
            max_total_bytes: 0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_snapshot_metadata_new() {
        let meta = SnapshotMetadata::new(
            "snap-001".to_string(),
            "my-snapshot".to_string(),
            "box-abc".to_string(),
            "alpine:latest".to_string(),
        );
        assert_eq!(meta.id, "snap-001");
        assert_eq!(meta.name, "my-snapshot");
        assert_eq!(meta.source_box_id, "box-abc");
        assert_eq!(meta.image, "alpine:latest");
        assert_eq!(meta.vcpus, 2);
        assert_eq!(meta.memory_mb, 512);
        assert!(meta.volumes.is_empty());
        assert!(meta.env.is_empty());
        assert!(meta.description.is_empty());
    }

    #[test]
    fn test_snapshot_metadata_with_description() {
        let meta = SnapshotMetadata::new(
            "snap-002".to_string(),
            "test".to_string(),
            "box-xyz".to_string(),
            "ubuntu:22.04".to_string(),
        )
        .with_description("Before migration");
        assert_eq!(meta.description, "Before migration");
    }

    #[test]
    fn test_snapshot_metadata_with_resources() {
        let meta = SnapshotMetadata::new(
            "snap-003".to_string(),
            "test".to_string(),
            "box-xyz".to_string(),
            "python:3.12".to_string(),
        )
        .with_resources(4, 2048);
        assert_eq!(meta.vcpus, 4);
        assert_eq!(meta.memory_mb, 2048);
    }

    #[test]
    fn test_snapshot_metadata_serde_roundtrip() {
        let mut meta = SnapshotMetadata::new(
            "snap-rt".to_string(),
            "roundtrip".to_string(),
            "box-123".to_string(),
            "nginx:latest".to_string(),
        );
        meta.vcpus = 4;
        meta.memory_mb = 1024;
        meta.volumes = vec!["/data:/data".to_string()];
        meta.env.insert("FOO".to_string(), "bar".to_string());
        meta.cmd = vec!["nginx".to_string(), "-g".to_string()];
        meta.entrypoint = Some(vec!["/docker-entrypoint.sh".to_string()]);
        meta.workdir = Some("/app".to_string());
        meta.port_map = vec!["8080:80".to_string()];
        meta.labels.insert("env".to_string(), "prod".to_string());
        meta.network_mode = Some("bridge".to_string());
        meta.rootfs_cache_key = Some("abc123".to_string());
        meta.size_bytes = 1024 * 1024;
        meta.description = "test snapshot".to_string();

        let json = serde_json::to_string(&meta).unwrap();
        let parsed: SnapshotMetadata = serde_json::from_str(&json).unwrap();

        assert_eq!(parsed.id, "snap-rt");
        assert_eq!(parsed.name, "roundtrip");
        assert_eq!(parsed.source_box_id, "box-123");
        assert_eq!(parsed.image, "nginx:latest");
        assert_eq!(parsed.vcpus, 4);
        assert_eq!(parsed.memory_mb, 1024);
        assert_eq!(parsed.volumes, vec!["/data:/data"]);
        assert_eq!(parsed.env.get("FOO").unwrap(), "bar");
        assert_eq!(parsed.cmd, vec!["nginx", "-g"]);
        assert_eq!(
            parsed.entrypoint,
            Some(vec!["/docker-entrypoint.sh".to_string()])
        );
        assert_eq!(parsed.workdir, Some("/app".to_string()));
        assert_eq!(parsed.port_map, vec!["8080:80"]);
        assert_eq!(parsed.labels.get("env").unwrap(), "prod");
        assert_eq!(parsed.network_mode, Some("bridge".to_string()));
        assert_eq!(parsed.rootfs_cache_key, Some("abc123".to_string()));
        assert_eq!(parsed.size_bytes, 1024 * 1024);
        assert_eq!(parsed.description, "test snapshot");
    }

    #[test]
    fn test_snapshot_metadata_deserialize_minimal() {
        let json = r#"{
            "id": "snap-min",
            "name": "minimal",
            "source_box_id": "box-1",
            "image": "alpine:latest",
            "vcpus": 1,
            "memory_mb": 256,
            "volumes": [],
            "env": {},
            "cmd": [],
            "size_bytes": 0,
            "created_at": "2024-01-01T00:00:00Z",
            "description": ""
        }"#;
        let meta: SnapshotMetadata = serde_json::from_str(json).unwrap();
        assert_eq!(meta.id, "snap-min");
        assert!(meta.entrypoint.is_none());
        assert!(meta.workdir.is_none());
        assert!(meta.port_map.is_empty());
        assert!(meta.labels.is_empty());
        assert!(meta.network_mode.is_none());
        assert!(meta.rootfs_cache_key.is_none());
    }

    #[test]
    fn test_snapshot_config_default() {
        let config = SnapshotConfig::default();
        assert!(config.enabled);
        assert!(config.snapshot_dir.is_none());
        assert_eq!(config.max_snapshots, 0);
        assert_eq!(config.max_total_bytes, 0);
    }

    #[test]
    fn test_snapshot_config_serde_roundtrip() {
        let config = SnapshotConfig {
            enabled: true,
            snapshot_dir: Some(PathBuf::from("/custom/snapshots")),
            max_snapshots: 10,
            max_total_bytes: 5 * 1024 * 1024 * 1024,
        };
        let json = serde_json::to_string(&config).unwrap();
        let parsed: SnapshotConfig = serde_json::from_str(&json).unwrap();
        assert!(parsed.enabled);
        assert_eq!(
            parsed.snapshot_dir,
            Some(PathBuf::from("/custom/snapshots"))
        );
        assert_eq!(parsed.max_snapshots, 10);
        assert_eq!(parsed.max_total_bytes, 5 * 1024 * 1024 * 1024);
    }

    #[test]
    fn test_snapshot_metadata_clone() {
        let meta = SnapshotMetadata::new(
            "snap-clone".to_string(),
            "clone-test".to_string(),
            "box-c".to_string(),
            "redis:7".to_string(),
        );
        let cloned = meta.clone();
        assert_eq!(cloned.id, meta.id);
        assert_eq!(cloned.name, meta.name);
        assert_eq!(cloned.source_box_id, meta.source_box_id);
    }
}
