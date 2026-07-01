//! OCI image reference parsing.
//!
//! Parses image references like `ghcr.io/a3s-box/code:v0.1.0` into structured components.

use a3s_box_core::error::{BoxError, Result};

/// Default registry when none is specified.
const DEFAULT_REGISTRY: &str = "docker.io";

/// Default tag when none is specified.
const DEFAULT_TAG: &str = "latest";

/// Parsed OCI image reference.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageReference {
    /// Registry hostname (e.g., "ghcr.io", "docker.io")
    pub registry: String,
    /// Repository path (e.g., "library/nginx", "a3s-box/code")
    pub repository: String,
    /// Tag (e.g., "latest", "v0.1.0")
    pub tag: Option<String>,
    /// Digest (e.g., "sha256:abc123...")
    pub digest: Option<String>,
}

impl ImageReference {
    /// Parse an image reference string.
    ///
    /// Supports formats:
    /// - `nginx` → docker.io/library/nginx:latest
    /// - `nginx:1.25` → docker.io/library/nginx:1.25
    /// - `myuser/myimage` → docker.io/myuser/myimage:latest
    /// - `ghcr.io/org/image:tag` → ghcr.io/org/image:tag
    /// - `ghcr.io/org/image@sha256:abc...` → ghcr.io/org/image@sha256:abc...
    pub fn parse(reference: &str) -> Result<Self> {
        let reference = reference.trim();
        if reference.is_empty() {
            return Err(BoxError::OciImageError("Empty image reference".to_string()));
        }

        // Split off digest first (@ separator)
        let (name_tag, digest) = if let Some(at_pos) = reference.rfind('@') {
            let digest_part = &reference[at_pos + 1..];
            if !digest_part.contains(':') {
                return Err(BoxError::OciImageError(format!(
                    "Invalid digest format in reference '{}': expected algorithm:hex",
                    reference
                )));
            }
            (&reference[..at_pos], Some(digest_part.to_string()))
        } else {
            (reference, None)
        };

        // Split tag (: separator, but only after the last /)
        let (name, tag) = if digest.is_some() {
            // If we have a digest, tag is in name_tag only if explicitly present
            if let Some(slash_pos) = name_tag.rfind('/') {
                let after_slash = &name_tag[slash_pos + 1..];
                if let Some(colon_pos) = after_slash.rfind(':') {
                    let tag = &after_slash[colon_pos + 1..];
                    let name = &name_tag[..slash_pos + 1 + colon_pos];
                    (name.to_string(), Some(tag.to_string()))
                } else {
                    (name_tag.to_string(), None)
                }
            } else if let Some(colon_pos) = name_tag.rfind(':') {
                let tag = &name_tag[colon_pos + 1..];
                let name = &name_tag[..colon_pos];
                (name.to_string(), Some(tag.to_string()))
            } else {
                (name_tag.to_string(), None)
            }
        } else {
            // No digest — split on last colon after last slash
            if let Some(slash_pos) = name_tag.rfind('/') {
                let after_slash = &name_tag[slash_pos + 1..];
                if let Some(colon_pos) = after_slash.rfind(':') {
                    let tag = &after_slash[colon_pos + 1..];
                    let name = &name_tag[..slash_pos + 1 + colon_pos];
                    (name.to_string(), Some(tag.to_string()))
                } else {
                    (name_tag.to_string(), None)
                }
            } else if let Some(colon_pos) = name_tag.rfind(':') {
                // No slash: the colon always introduces a tag. A bare
                // `registry:port` with no repository is not a valid image
                // reference, so numeric tags (`redis:7`, `node:18`,
                // `postgres:16`) must NOT be mistaken for a port.
                let tag = &name_tag[colon_pos + 1..];
                let name = &name_tag[..colon_pos];
                (name.to_string(), Some(tag.to_string()))
            } else {
                (name_tag.to_string(), None)
            }
        };

        // Determine registry and repository
        let (registry, repository) = Self::split_registry_repository(&name)?;

        // Apply default tag if no tag and no digest
        let tag = if tag.is_none() && digest.is_none() {
            Some(DEFAULT_TAG.to_string())
        } else {
            tag
        };

        Ok(ImageReference {
            registry,
            repository,
            tag,
            digest,
        })
    }

    /// Split a name into registry and repository components.
    fn split_registry_repository(name: &str) -> Result<(String, String)> {
        // Check if the first component looks like a registry hostname
        // (contains a dot or colon, or is "localhost")
        if let Some(slash_pos) = name.find('/') {
            let first = &name[..slash_pos];
            if first.contains('.') || first.contains(':') || first == "localhost" {
                let registry = first.to_string();
                let repo = name[slash_pos + 1..].to_string();
                if repo.is_empty() {
                    return Err(BoxError::OciImageError(format!(
                        "Empty repository in reference '{}'",
                        name
                    )));
                }
                return Ok((registry, repo));
            }
        }

        // No registry detected — use default
        let repository = if name.contains('/') {
            name.to_string()
        } else {
            // Single name like "nginx" → "library/nginx" for Docker Hub
            format!("library/{}", name)
        };

        Ok((DEFAULT_REGISTRY.to_string(), repository))
    }

    /// Get the full reference string.
    pub fn full_reference(&self) -> String {
        let mut s = format!("{}/{}", self.registry, self.repository);
        if let Some(ref tag) = self.tag {
            s.push(':');
            s.push_str(tag);
        }
        if let Some(ref digest) = self.digest {
            s.push('@');
            s.push_str(digest);
        }
        s
    }
}

impl std::fmt::Display for ImageReference {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.full_reference())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_simple_name() {
        let r = ImageReference::parse("nginx").unwrap();
        assert_eq!(r.registry, "docker.io");
        assert_eq!(r.repository, "library/nginx");
        assert_eq!(r.tag, Some("latest".to_string()));
        assert_eq!(r.digest, None);
    }

    #[test]
    fn test_parse_name_with_tag() {
        let r = ImageReference::parse("nginx:1.25").unwrap();
        assert_eq!(r.registry, "docker.io");
        assert_eq!(r.repository, "library/nginx");
        assert_eq!(r.tag, Some("1.25".to_string()));
        assert_eq!(r.digest, None);
    }

    #[test]
    fn test_parse_user_repo() {
        let r = ImageReference::parse("myuser/myimage").unwrap();
        assert_eq!(r.registry, "docker.io");
        assert_eq!(r.repository, "myuser/myimage");
        assert_eq!(r.tag, Some("latest".to_string()));
    }

    #[test]
    fn test_parse_user_repo_with_tag() {
        let r = ImageReference::parse("myuser/myimage:v1.0").unwrap();
        assert_eq!(r.registry, "docker.io");
        assert_eq!(r.repository, "myuser/myimage");
        assert_eq!(r.tag, Some("v1.0".to_string()));
    }

    #[test]
    fn test_parse_custom_registry() {
        let r = ImageReference::parse("ghcr.io/a3s-box/code:v0.1.0").unwrap();
        assert_eq!(r.registry, "ghcr.io");
        assert_eq!(r.repository, "a3s-box/code");
        assert_eq!(r.tag, Some("v0.1.0".to_string()));
    }

    #[test]
    fn test_parse_custom_registry_no_tag() {
        let r = ImageReference::parse("ghcr.io/a3s-box/code").unwrap();
        assert_eq!(r.registry, "ghcr.io");
        assert_eq!(r.repository, "a3s-box/code");
        assert_eq!(r.tag, Some("latest".to_string()));
    }

    #[test]
    fn test_parse_digest_only() {
        let r = ImageReference::parse(
            "ghcr.io/a3s-box/code@sha256:abcdef1234567890abcdef1234567890abcdef1234567890abcdef1234567890",
        )
        .unwrap();
        assert_eq!(r.registry, "ghcr.io");
        assert_eq!(r.repository, "a3s-box/code");
        assert_eq!(r.tag, None);
        assert_eq!(
            r.digest,
            Some(
                "sha256:abcdef1234567890abcdef1234567890abcdef1234567890abcdef1234567890"
                    .to_string()
            )
        );
    }

    #[test]
    fn test_parse_tag_and_digest() {
        let r =
            ImageReference::parse("ghcr.io/a3s-box/code:v0.1.0@sha256:abcdef1234567890").unwrap();
        assert_eq!(r.registry, "ghcr.io");
        assert_eq!(r.repository, "a3s-box/code");
        assert_eq!(r.tag, Some("v0.1.0".to_string()));
        assert_eq!(r.digest, Some("sha256:abcdef1234567890".to_string()));
    }

    #[test]
    fn test_parse_localhost_registry() {
        let r = ImageReference::parse("localhost/myimage:test").unwrap();
        assert_eq!(r.registry, "localhost");
        assert_eq!(r.repository, "myimage");
        assert_eq!(r.tag, Some("test".to_string()));
    }

    #[test]
    fn test_parse_registry_with_port() {
        let r = ImageReference::parse("myregistry.io:5000/myimage:v1").unwrap();
        assert_eq!(r.registry, "myregistry.io:5000");
        assert_eq!(r.repository, "myimage");
        assert_eq!(r.tag, Some("v1".to_string()));
    }

    #[test]
    fn test_parse_empty_reference() {
        let r = ImageReference::parse("");
        assert!(r.is_err());
    }

    #[test]
    fn test_parse_whitespace_reference() {
        let r = ImageReference::parse("  nginx  ").unwrap();
        assert_eq!(r.repository, "library/nginx");
    }

    #[test]
    fn test_parse_invalid_digest() {
        let r = ImageReference::parse("nginx@invaliddigest");
        assert!(r.is_err());
    }

    #[test]
    fn test_full_reference() {
        let r = ImageReference::parse("ghcr.io/a3s-box/code:v0.1.0").unwrap();
        assert_eq!(r.full_reference(), "ghcr.io/a3s-box/code:v0.1.0");
    }

    #[test]
    fn test_full_reference_with_digest() {
        let r = ImageReference {
            registry: "ghcr.io".to_string(),
            repository: "a3s-box/code".to_string(),
            tag: Some("v0.1.0".to_string()),
            digest: Some("sha256:abc123".to_string()),
        };
        assert_eq!(
            r.full_reference(),
            "ghcr.io/a3s-box/code:v0.1.0@sha256:abc123"
        );
    }

    #[test]
    fn test_display() {
        let r = ImageReference::parse("nginx:1.25").unwrap();
        assert_eq!(format!("{}", r), "docker.io/library/nginx:1.25");
    }

    /// Regression: a purely numeric tag with no registry (`redis:7`, `node:18`)
    /// must be parsed as a tag, not mistaken for a registry port.
    #[test]
    fn test_numeric_tag_is_not_a_port() {
        for (input, repo, tag) in [
            ("redis:7", "library/redis", "7"),
            ("node:18", "library/node", "18"),
            ("postgres:16", "library/postgres", "16"),
            ("mgmt:1", "library/mgmt", "1"),
        ] {
            let r = ImageReference::parse(input).unwrap();
            assert_eq!(r.registry, "docker.io", "registry for {input}");
            assert_eq!(r.repository, repo, "repository for {input}");
            assert_eq!(r.tag.as_deref(), Some(tag), "tag for {input}");
        }
        // A real registry:port/repo:tag still splits correctly.
        let r = ImageReference::parse("localhost:5000/app:2").unwrap();
        assert_eq!(r.registry, "localhost:5000");
        assert_eq!(r.repository, "app");
        assert_eq!(r.tag.as_deref(), Some("2"));
    }

    #[test]
    fn test_deep_repository_path() {
        let r = ImageReference::parse("ghcr.io/org/sub/image:v1").unwrap();
        assert_eq!(r.registry, "ghcr.io");
        assert_eq!(r.repository, "org/sub/image");
        assert_eq!(r.tag, Some("v1".to_string()));
    }
}
