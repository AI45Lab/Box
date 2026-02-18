//! Volume types for named volume management.
//!
//! Provides volume configuration and metadata for persistent
//! named volumes that can be shared across box instances.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Configuration for a named volume.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VolumeConfig {
    /// Volume name (unique identifier).
    pub name: String,

    /// Volume driver (currently only "local" is supported).
    #[serde(default = "default_driver")]
    pub driver: String,

    /// Host path where volume data is stored.
    pub mount_point: String,

    /// User-defined labels.
    #[serde(default)]
    pub labels: HashMap<String, String>,

    /// Box IDs currently using this volume.
    #[serde(default)]
    pub in_use_by: Vec<String>,

    /// Maximum size in bytes (0 = unlimited).
    #[serde(default)]
    pub size_limit: u64,

    /// Creation timestamp (RFC 3339).
    pub created_at: String,
}

fn default_driver() -> String {
    "local".to_string()
}

impl VolumeConfig {
    /// Create a new named volume.
    pub fn new(name: &str, mount_point: &str) -> Self {
        Self {
            name: name.to_string(),
            driver: "local".to_string(),
            mount_point: mount_point.to_string(),
            labels: HashMap::new(),
            in_use_by: Vec::new(),
            size_limit: 0,
            created_at: chrono::Utc::now().to_rfc3339(),
        }
    }

    /// Create a new named volume with a size limit.
    pub fn with_size_limit(name: &str, mount_point: &str, size_limit: u64) -> Self {
        let mut vol = Self::new(name, mount_point);
        vol.size_limit = size_limit;
        vol
    }

    /// Check if the volume exceeds its size limit.
    ///
    /// Returns `Ok(current_size)` if within quota, or `Err` with a message
    /// if the volume exceeds its limit. Returns `Ok(0)` if no limit is set.
    pub fn check_quota(&self) -> Result<u64, String> {
        if self.size_limit == 0 {
            return Ok(0);
        }
        let path = std::path::Path::new(&self.mount_point);
        if !path.exists() {
            return Ok(0);
        }
        let size = dir_size(path);
        if size > self.size_limit {
            Err(format!(
                "volume '{}' exceeds size limit: {} > {} bytes",
                self.name, size, self.size_limit
            ))
        } else {
            Ok(size)
        }
    }

    /// Mark a box as using this volume.
    pub fn attach(&mut self, box_id: &str) {
        if !self.in_use_by.contains(&box_id.to_string()) {
            self.in_use_by.push(box_id.to_string());
        }
    }

    /// Remove a box from this volume's users.
    pub fn detach(&mut self, box_id: &str) {
        self.in_use_by.retain(|id| id != box_id);
    }

    /// Check if any boxes are using this volume.
    pub fn is_in_use(&self) -> bool {
        !self.in_use_by.is_empty()
    }
}

/// Recursively calculate directory size in bytes.
fn dir_size(path: &std::path::Path) -> u64 {
    let mut total = 0u64;
    if let Ok(entries) = std::fs::read_dir(path) {
        for entry in entries.flatten() {
            let p = entry.path();
            if p.is_dir() {
                total += dir_size(&p);
            } else if let Ok(meta) = p.metadata() {
                total += meta.len();
            }
        }
    }
    total
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_volume_config_new() {
        let vol = VolumeConfig::new("mydata", "/home/user/.a3s/volumes/mydata");
        assert_eq!(vol.name, "mydata");
        assert_eq!(vol.driver, "local");
        assert!(vol.in_use_by.is_empty());
        assert!(vol.labels.is_empty());
    }

    #[test]
    fn test_volume_attach_detach() {
        let mut vol = VolumeConfig::new("mydata", "/tmp/vol");
        vol.attach("box-1");
        vol.attach("box-2");
        assert_eq!(vol.in_use_by.len(), 2);
        assert!(vol.is_in_use());

        vol.detach("box-1");
        assert_eq!(vol.in_use_by.len(), 1);
        assert!(vol.is_in_use());

        vol.detach("box-2");
        assert!(!vol.is_in_use());
    }

    #[test]
    fn test_volume_attach_idempotent() {
        let mut vol = VolumeConfig::new("mydata", "/tmp/vol");
        vol.attach("box-1");
        vol.attach("box-1");
        assert_eq!(vol.in_use_by.len(), 1);
    }

    #[test]
    fn test_volume_detach_nonexistent() {
        let mut vol = VolumeConfig::new("mydata", "/tmp/vol");
        vol.detach("nonexistent"); // should not panic
        assert!(vol.in_use_by.is_empty());
    }

    #[test]
    fn test_volume_with_labels() {
        let mut vol = VolumeConfig::new("mydata", "/tmp/vol");
        vol.labels.insert("env".to_string(), "prod".to_string());
        assert_eq!(vol.labels.get("env").unwrap(), "prod");
    }

    #[test]
    fn test_volume_serialization() {
        let mut vol = VolumeConfig::new("mydata", "/tmp/vol");
        vol.attach("box-1");
        vol.labels.insert("env".to_string(), "test".to_string());

        let json = serde_json::to_string(&vol).unwrap();
        let parsed: VolumeConfig = serde_json::from_str(&json).unwrap();

        assert_eq!(parsed.name, "mydata");
        assert_eq!(parsed.in_use_by, vec!["box-1"]);
        assert_eq!(parsed.labels.get("env").unwrap(), "test");
    }

    #[test]
    fn test_volume_default_driver() {
        let json = r#"{"name":"test","mount_point":"/tmp","created_at":"2024-01-01T00:00:00Z"}"#;
        let vol: VolumeConfig = serde_json::from_str(json).unwrap();
        assert_eq!(vol.driver, "local");
    }

    #[test]
    fn test_volume_size_limit_default_zero() {
        let vol = VolumeConfig::new("test", "/tmp/vol");
        assert_eq!(vol.size_limit, 0);
    }

    #[test]
    fn test_volume_with_size_limit() {
        let vol = VolumeConfig::with_size_limit("test", "/tmp/vol", 1024 * 1024);
        assert_eq!(vol.size_limit, 1024 * 1024);
        assert_eq!(vol.name, "test");
    }

    #[test]
    fn test_volume_check_quota_no_limit() {
        let vol = VolumeConfig::new("test", "/tmp/nonexistent");
        assert!(vol.check_quota().is_ok());
        assert_eq!(vol.check_quota().unwrap(), 0);
    }

    #[test]
    fn test_volume_check_quota_within_limit() {
        let dir = std::env::temp_dir().join("a3s_test_vol_quota_ok");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("data.txt"), "hello").unwrap();

        let vol = VolumeConfig::with_size_limit(
            "test",
            dir.to_str().unwrap(),
            1024 * 1024, // 1MB limit
        );
        let result = vol.check_quota();
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), 5); // "hello" = 5 bytes

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_volume_check_quota_exceeded() {
        let dir = std::env::temp_dir().join("a3s_test_vol_quota_exceed");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("data.txt"), "hello world!").unwrap();

        let vol = VolumeConfig::with_size_limit(
            "test",
            dir.to_str().unwrap(),
            5, // 5 byte limit
        );
        let result = vol.check_quota();
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("exceeds size limit"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_volume_size_limit_serde() {
        let vol = VolumeConfig::with_size_limit("test", "/tmp/vol", 1024);
        let json = serde_json::to_string(&vol).unwrap();
        let parsed: VolumeConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.size_limit, 1024);
    }

    #[test]
    fn test_dir_size_empty() {
        let dir = std::env::temp_dir().join("a3s_test_dir_size_empty");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        assert_eq!(dir_size(&dir), 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_dir_size_with_files() {
        let dir = std::env::temp_dir().join("a3s_test_dir_size_files");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.txt"), "aaa").unwrap();
        std::fs::write(dir.join("b.txt"), "bb").unwrap();
        assert_eq!(dir_size(&dir), 5);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_dir_size_recursive() {
        let dir = std::env::temp_dir().join("a3s_test_dir_size_recursive");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        std::fs::write(dir.join("a.txt"), "aaa").unwrap();
        std::fs::write(dir.join("sub").join("b.txt"), "bb").unwrap();
        assert_eq!(dir_size(&dir), 5);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
