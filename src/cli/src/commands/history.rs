//! `a3s-box history` command — Show image layer history.

use clap::Args;
use oci_spec::image::{History, ImageConfiguration};

use crate::image_usage;
use crate::output;

#[derive(Args)]
pub struct HistoryArgs {
    pub image: String,
    #[arg(short, long)]
    pub quiet: bool,
    #[arg(long)]
    pub no_trunc: bool,
}

pub async fn execute(args: HistoryArgs) -> Result<(), Box<dyn std::error::Error>> {
    let store = super::open_image_store()?;
    let images = store.list().await;
    let stored = image_usage::resolve_required_stored_image(&images, &args.image)?;
    let oci_config = load_image_configuration(&stored.path)?;
    let history: &Vec<History> = oci_config.history();

    if args.quiet {
        let diff_ids: &Vec<String> = oci_config.rootfs().diff_ids();
        for id in diff_ids {
            println!("{id}");
        }
        return Ok(());
    }

    let mut table = output::new_table(&["CREATED", "CREATED BY", "SIZE", "COMMENT"]);
    let layer_sizes = get_layer_sizes(&stored.path)?;
    let mut layer_idx = 0;

    for entry in history {
        let created_field = History::created(entry);
        let created = match created_field {
            Some(t) => format_timestamp(t),
            None => "<unknown>".to_string(),
        };

        let created_by_field = History::created_by(entry);
        let created_by = match created_by_field {
            Some(s) => {
                if args.no_trunc {
                    s.clone()
                } else {
                    truncate_str(s, 60)
                }
            }
            None => String::new(),
        };

        let is_empty = entry.empty_layer().unwrap_or(false);
        let size = if is_empty {
            "0 B".to_string()
        } else {
            let s = layer_sizes.get(layer_idx).copied().unwrap_or(0);
            layer_idx += 1;
            output::format_bytes(s)
        };

        let comment_field = History::comment(entry);
        let comment = comment_field.clone().unwrap_or_default();

        table.add_row([&created, &created_by, &size, &comment]);
    }

    println!("{table}");
    Ok(())
}

fn load_image_configuration(
    image_dir: &std::path::Path,
) -> Result<ImageConfiguration, Box<dyn std::error::Error>> {
    let index_content = std::fs::read_to_string(image_dir.join("index.json"))
        .map_err(|e| format!("Failed to read index.json: {e}"))?;
    let index: serde_json::Value = serde_json::from_str(&index_content)?;
    let manifest_digest = index["manifests"][0]["digest"]
        .as_str()
        .ok_or("No manifest digest in index.json")?;
    let manifest_path = blob_path(image_dir, manifest_digest);
    let manifest_content = std::fs::read_to_string(&manifest_path)
        .map_err(|e| format!("Failed to read manifest: {e}"))?;
    let manifest: serde_json::Value = serde_json::from_str(&manifest_content)?;
    let config_digest = manifest["config"]["digest"]
        .as_str()
        .ok_or("No config digest in manifest")?;
    let config_path = blob_path(image_dir, config_digest);
    let config_content =
        std::fs::read_to_string(&config_path).map_err(|e| format!("Failed to read config: {e}"))?;
    let config: ImageConfiguration = serde_json::from_str(&config_content)
        .map_err(|e| format!("Failed to parse config: {e}"))?;
    Ok(config)
}

fn get_layer_sizes(image_dir: &std::path::Path) -> Result<Vec<u64>, Box<dyn std::error::Error>> {
    let index_content = std::fs::read_to_string(image_dir.join("index.json"))?;
    let index: serde_json::Value = serde_json::from_str(&index_content)?;
    let manifest_digest = index["manifests"][0]["digest"]
        .as_str()
        .ok_or("No manifest digest")?;
    let manifest_path = blob_path(image_dir, manifest_digest);
    let manifest_content = std::fs::read_to_string(&manifest_path)?;
    let manifest: serde_json::Value = serde_json::from_str(&manifest_content)?;
    let sizes = manifest["layers"]
        .as_array()
        .map(|layers| {
            layers
                .iter()
                .map(|l| l["size"].as_u64().unwrap_or(0))
                .collect()
        })
        .unwrap_or_default();
    Ok(sizes)
}

fn blob_path(root_dir: &std::path::Path, digest: &str) -> std::path::PathBuf {
    let parts: Vec<&str> = digest.split(':').collect();
    let (algorithm, hash) = if parts.len() == 2 {
        (parts[0], parts[1])
    } else {
        ("sha256", digest)
    };
    root_dir.join("blobs").join(algorithm).join(hash)
}

fn truncate_str(s: &str, max_len: usize) -> String {
    if s.len() <= max_len {
        s.to_string()
    } else {
        format!("{}...", &s[..max_len.saturating_sub(3)])
    }
}

fn format_timestamp(ts: &str) -> String {
    chrono::DateTime::parse_from_rfc3339(ts)
        .map(|dt| output::format_ago(&dt.with_timezone(&chrono::Utc)))
        .unwrap_or_else(|_| ts.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use a3s_box_runtime::StoredImage;
    use chrono::Utc;
    use std::path::PathBuf;

    #[test]
    fn test_truncate_str_short() {
        assert_eq!(truncate_str("hello", 10), "hello");
    }
    #[test]
    fn test_truncate_str_exact() {
        assert_eq!(truncate_str("hello", 5), "hello");
    }
    #[test]
    fn test_truncate_str_long() {
        assert_eq!(truncate_str("hello world", 8), "hello...");
    }
    #[test]
    fn test_truncate_str_very_short_limit() {
        assert_eq!(truncate_str("hello world", 3), "...");
    }
    #[test]
    fn test_blob_path_with_prefix() {
        let root = std::path::Path::new("/images/test");
        assert_eq!(
            blob_path(root, "sha256:abc123"),
            std::path::PathBuf::from("/images/test/blobs/sha256/abc123")
        );
    }
    #[test]
    fn test_blob_path_without_prefix() {
        let root = std::path::Path::new("/images/test");
        assert_eq!(
            blob_path(root, "abc123"),
            std::path::PathBuf::from("/images/test/blobs/sha256/abc123")
        );
    }

    #[test]
    fn test_history_resolution_accepts_normalized_alias() {
        let images = vec![StoredImage {
            reference: "docker.io/library/alpine:latest".to_string(),
            digest: "sha256:abc".to_string(),
            size_bytes: 1024,
            pulled_at: Utc::now(),
            last_used: Utc::now(),
            path: PathBuf::from("/tmp/image"),
        }];

        let stored = image_usage::resolve_required_stored_image(&images, "alpine:latest").unwrap();

        assert_eq!(stored.reference, "docker.io/library/alpine:latest");
    }
    #[test]
    fn test_format_timestamp_valid() {
        let result = format_timestamp("2024-01-01T00:00:00Z");
        assert!(!result.is_empty());
    }
    #[test]
    fn test_format_timestamp_invalid() {
        assert_eq!(format_timestamp("not-a-date"), "not-a-date");
    }
}
