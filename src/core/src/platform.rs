//! Platform types for multi-architecture image builds.
//!
//! Represents target OS/architecture pairs used by Buildx-style
//! multi-platform builds and OCI Image Index manifests.

use serde::{Deserialize, Serialize};
use std::fmt;

/// A target platform (OS + architecture).
///
/// Used for multi-platform builds and OCI Image Index entries.
/// Compatible with Docker/OCI platform specification.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Platform {
    /// Operating system (e.g., "linux").
    pub os: String,
    /// CPU architecture (e.g., "amd64", "arm64").
    pub architecture: String,
    /// Optional variant (e.g., "v7" for armv7).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub variant: Option<String>,
}

impl Platform {
    /// Create a new platform.
    pub fn new(os: impl Into<String>, architecture: impl Into<String>) -> Self {
        Self {
            os: os.into(),
            architecture: architecture.into(),
            variant: None,
        }
    }

    /// Create a platform with a variant.
    pub fn with_variant(
        os: impl Into<String>,
        architecture: impl Into<String>,
        variant: impl Into<String>,
    ) -> Self {
        Self {
            os: os.into(),
            architecture: architecture.into(),
            variant: Some(variant.into()),
        }
    }

    /// linux/amd64
    pub fn linux_amd64() -> Self {
        Self::new("linux", "amd64")
    }

    /// linux/arm64
    pub fn linux_arm64() -> Self {
        Self::new("linux", "arm64")
    }

    /// Detect the current host platform.
    pub fn host() -> Self {
        let arch = match std::env::consts::ARCH {
            "x86_64" => "amd64",
            "aarch64" => "arm64",
            other => other,
        };
        Self::new("linux", arch)
    }

    /// Parse a platform string like "linux/amd64" or "linux/arm/v7".
    pub fn parse(s: &str) -> Result<Self, String> {
        let parts: Vec<&str> = s.split('/').collect();
        match parts.len() {
            2 => {
                let arch = normalize_arch(parts[1]);
                Ok(Self::new(parts[0], arch))
            }
            3 => {
                let arch = normalize_arch(parts[1]);
                Ok(Self::with_variant(parts[0], arch, parts[2]))
            }
            _ => Err(format!(
                "Invalid platform '{}': expected 'os/arch' or 'os/arch/variant'",
                s
            )),
        }
    }

    /// Parse a comma-separated list of platforms.
    ///
    /// Example: "linux/amd64,linux/arm64"
    pub fn parse_list(s: &str) -> Result<Vec<Self>, String> {
        s.split(',').map(|p| Self::parse(p.trim())).collect()
    }

    /// Check if this platform matches the host architecture.
    pub fn is_native(&self) -> bool {
        *self == Self::host()
    }

    /// Get the OCI architecture string.
    pub fn oci_arch(&self) -> &str {
        &self.architecture
    }
}

impl fmt::Display for Platform {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.os, self.architecture)?;
        if let Some(ref v) = self.variant {
            write!(f, "/{}", v)?;
        }
        Ok(())
    }
}

/// Normalize architecture names to OCI conventions.
fn normalize_arch(arch: &str) -> String {
    match arch {
        "x86_64" | "x86-64" => "amd64".to_string(),
        "aarch64" | "arm64v8" => "arm64".to_string(),
        "armhf" | "armv7l" => "arm".to_string(),
        "i386" | "i686" | "x86" => "386".to_string(),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_platform_new() {
        let p = Platform::new("linux", "amd64");
        assert_eq!(p.os, "linux");
        assert_eq!(p.architecture, "amd64");
        assert!(p.variant.is_none());
    }

    #[test]
    fn test_platform_with_variant() {
        let p = Platform::with_variant("linux", "arm", "v7");
        assert_eq!(p.variant, Some("v7".to_string()));
    }

    #[test]
    fn test_platform_display() {
        assert_eq!(Platform::linux_amd64().to_string(), "linux/amd64");
        assert_eq!(Platform::linux_arm64().to_string(), "linux/arm64");
        assert_eq!(
            Platform::with_variant("linux", "arm", "v7").to_string(),
            "linux/arm/v7"
        );
    }

    #[test]
    fn test_platform_parse() {
        let p = Platform::parse("linux/amd64").unwrap();
        assert_eq!(p, Platform::linux_amd64());

        let p = Platform::parse("linux/arm64").unwrap();
        assert_eq!(p, Platform::linux_arm64());

        let p = Platform::parse("linux/arm/v7").unwrap();
        assert_eq!(p.architecture, "arm");
        assert_eq!(p.variant, Some("v7".to_string()));
    }

    #[test]
    fn test_platform_parse_normalizes() {
        let p = Platform::parse("linux/x86_64").unwrap();
        assert_eq!(p.architecture, "amd64");

        let p = Platform::parse("linux/aarch64").unwrap();
        assert_eq!(p.architecture, "arm64");
    }

    #[test]
    fn test_platform_parse_invalid() {
        assert!(Platform::parse("linux").is_err());
        assert!(Platform::parse("a/b/c/d").is_err());
    }

    #[test]
    fn test_platform_parse_list() {
        let platforms = Platform::parse_list("linux/amd64,linux/arm64").unwrap();
        assert_eq!(platforms.len(), 2);
        assert_eq!(platforms[0], Platform::linux_amd64());
        assert_eq!(platforms[1], Platform::linux_arm64());
    }

    #[test]
    fn test_platform_parse_list_with_spaces() {
        let platforms = Platform::parse_list("linux/amd64, linux/arm64").unwrap();
        assert_eq!(platforms.len(), 2);
    }

    #[test]
    fn test_platform_host() {
        let host = Platform::host();
        assert_eq!(host.os, "linux");
        assert!(host.architecture == "amd64" || host.architecture == "arm64");
    }

    #[test]
    fn test_platform_is_native() {
        let host = Platform::host();
        assert!(host.is_native());
        // The opposite arch should not be native
        let other = if host.architecture == "amd64" {
            Platform::linux_arm64()
        } else {
            Platform::linux_amd64()
        };
        assert!(!other.is_native());
    }

    #[test]
    fn test_platform_serde_roundtrip() {
        let p = Platform::linux_amd64();
        let json = serde_json::to_string(&p).unwrap();
        let parsed: Platform = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, p);
    }

    #[test]
    fn test_platform_serde_with_variant() {
        let p = Platform::with_variant("linux", "arm", "v7");
        let json = serde_json::to_string(&p).unwrap();
        let parsed: Platform = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, p);
        assert_eq!(parsed.variant, Some("v7".to_string()));
    }

    #[test]
    fn test_normalize_arch() {
        assert_eq!(normalize_arch("x86_64"), "amd64");
        assert_eq!(normalize_arch("aarch64"), "arm64");
        assert_eq!(normalize_arch("armhf"), "arm");
        assert_eq!(normalize_arch("i386"), "386");
        assert_eq!(normalize_arch("riscv64"), "riscv64");
    }

    #[test]
    fn test_platform_equality() {
        assert_eq!(Platform::linux_amd64(), Platform::new("linux", "amd64"));
        assert_ne!(Platform::linux_amd64(), Platform::linux_arm64());
    }
}
