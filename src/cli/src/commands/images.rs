//! `a3s-box images` command.

use clap::Args;

use crate::output;

use super::images_dir;

#[derive(Args)]
pub struct ImagesArgs {
    /// Only show image references (one per line)
    #[arg(short, long)]
    pub quiet: bool,

    /// Format output using placeholders: {{.Repository}}, {{.Tag}}, {{.Digest}},
    /// {{.Size}}, {{.Pulled}}, {{.Reference}}
    #[arg(long)]
    pub format: Option<String>,

    /// Filter output: `reference=<pattern>` (glob on repo[:tag]) or
    /// `label=<key>[=<value>]`. Can be repeated (all must match).
    #[arg(long = "filter")]
    pub filter: Vec<String>,
}

/// A parsed `--filter` predicate.
enum ImageFilter {
    Reference(String),
    Label(String, Option<String>),
}

impl ImageFilter {
    fn parse(spec: &str) -> Result<Self, String> {
        let (key, value) = spec
            .split_once('=')
            .ok_or_else(|| format!("Invalid --filter (expected key=value): {spec}"))?;
        match key {
            "reference" => Ok(ImageFilter::Reference(value.to_string())),
            "label" => {
                let (lk, lv) = match value.split_once('=') {
                    Some((k, v)) => (k.to_string(), Some(v.to_string())),
                    None => (value.to_string(), None),
                };
                Ok(ImageFilter::Label(lk, lv))
            }
            other => Err(format!(
                "Unsupported image filter '{other}' (supported: reference, label)"
            )),
        }
    }
}

/// Glob match where `*` matches any run of characters. Used for `reference=`.
fn glob_match(pattern: &str, text: &str) -> bool {
    let pat: Vec<char> = pattern.chars().collect();
    let txt: Vec<char> = text.chars().collect();
    let (mut p, mut t, mut star, mut mark) = (0usize, 0usize, None, 0usize);
    while t < txt.len() {
        if p < pat.len() && (pat[p] == '?' || pat[p] == txt[t]) {
            p += 1;
            t += 1;
        } else if p < pat.len() && pat[p] == '*' {
            star = Some(p);
            mark = t;
            p += 1;
        } else if let Some(sp) = star {
            p = sp + 1;
            mark += 1;
            t = mark;
        } else {
            return false;
        }
    }
    while p < pat.len() && pat[p] == '*' {
        p += 1;
    }
    p == pat.len()
}

pub async fn execute(args: ImagesArgs) -> Result<(), Box<dyn std::error::Error>> {
    let images_dir = images_dir();
    if !images_dir.exists() {
        if !args.quiet && args.format.is_none() {
            let table = output::new_table(&["REPOSITORY", "TAG", "DIGEST", "SIZE", "PULLED"]);
            println!("{table}");
        }
        return Ok(());
    }

    let store = super::open_image_store()?;
    let mut images = store.list().await;

    // --filter: keep only images matching every predicate.
    if !args.filter.is_empty() {
        let filters: Vec<ImageFilter> = args
            .filter
            .iter()
            .map(|f| ImageFilter::parse(f))
            .collect::<Result<_, _>>()?;
        images.retain(|img| filters.iter().all(|f| image_matches(img, f)));
    }

    // --quiet: print only references
    if args.quiet {
        for image in &images {
            println!("{}", image.reference);
        }
        return Ok(());
    }

    // Pre-compute display fields for each image
    let rows: Vec<ImageRow> = images.iter().map(ImageRow::from_stored).collect();

    // --format: custom template output
    if let Some(ref fmt) = args.format {
        for row in &rows {
            println!("{}", row.apply_format(fmt));
        }
        return Ok(());
    }

    // Default: table output
    let mut table = output::new_table(&["REPOSITORY", "TAG", "DIGEST", "SIZE", "PULLED"]);
    for row in &rows {
        table.add_row([
            &row.repository,
            &row.tag,
            &row.digest,
            &row.size,
            &row.pulled,
        ]);
    }

    println!("{table}");
    Ok(())
}

/// Whether a stored image satisfies one `--filter` predicate.
fn image_matches(image: &a3s_box_runtime::StoredImage, filter: &ImageFilter) -> bool {
    match filter {
        ImageFilter::Reference(pattern) => {
            // Match the glob against several name forms so `alpine`,
            // `alpine:3.19`, and `docker.io/library/alpine:3.19` all work.
            let mut candidates = vec![image.reference.clone()];
            if let Ok(r) = a3s_box_runtime::ImageReference::parse(&image.reference) {
                let tag = r.tag.clone().unwrap_or_else(|| "latest".to_string());
                candidates.push(format!("{}/{}:{}", r.registry, r.repository, tag));
                candidates.push(format!("{}:{}", r.repository, tag));
                candidates.push(r.repository.clone());
                // bare repo leaf, e.g. "alpine" from "library/alpine"
                if let Some(leaf) = r.repository.rsplit('/').next() {
                    candidates.push(leaf.to_string());
                    candidates.push(format!("{leaf}:{tag}"));
                }
            }
            candidates.iter().any(|c| glob_match(pattern, c))
        }
        ImageFilter::Label(key, value) => {
            let Ok(oci) = a3s_box_runtime::OciImage::from_path(&image.path) else {
                return false;
            };
            match oci.config().labels.get(key) {
                Some(v) => value.as_ref().is_none_or(|want| want == v),
                None => false,
            }
        }
    }
}

/// Pre-computed display fields for a single image row.
struct ImageRow {
    reference: String,
    repository: String,
    tag: String,
    digest: String,
    size: String,
    pulled: String,
}

impl ImageRow {
    fn from_stored(image: &a3s_box_runtime::StoredImage) -> Self {
        let (repository, tag) = if crate::image_usage::is_dangling_reference(&image.reference) {
            // Dangling image (digest-keyed, no repo:tag) — Docker renders it as
            // `<none> <none>` rather than splitting the digest into repo + tag.
            ("<none>".to_string(), "<none>".to_string())
        } else {
            match a3s_box_runtime::ImageReference::parse(&image.reference) {
                Ok(r) => {
                    let repo = format!("{}/{}", r.registry, r.repository);
                    let tag = r.tag.unwrap_or_else(|| "<none>".to_string());
                    (repo, tag)
                }
                Err(_) => (image.reference.clone(), "<none>".to_string()),
            }
        };

        // Format digest: "sha256:" prefix + first 12 hex chars
        let digest = if let Some(hex) = image.digest.strip_prefix("sha256:") {
            let truncated = if hex.len() > 12 { &hex[..12] } else { hex };
            format!("sha256:{truncated}")
        } else {
            let truncated = if image.digest.len() > 12 {
                &image.digest[..12]
            } else {
                &image.digest
            };
            truncated.to_string()
        };

        Self {
            reference: image.reference.clone(),
            repository,
            tag,
            digest,
            size: output::format_bytes(image.size_bytes),
            pulled: output::format_ago(&image.pulled_at),
        }
    }

    /// Apply a format template, replacing `{{.Field}}` placeholders.
    fn apply_format(&self, fmt: &str) -> String {
        fmt.replace("{{.Repository}}", &self.repository)
            .replace("{{.Tag}}", &self.tag)
            .replace("{{.Digest}}", &self.digest)
            .replace("{{.Size}}", &self.size)
            .replace("{{.Pulled}}", &self.pulled)
            .replace("{{.Reference}}", &self.reference)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use a3s_box_runtime::StoredImage;
    use chrono::Utc;
    use std::path::PathBuf;

    fn sample_stored(reference: &str, digest: &str, size: u64) -> StoredImage {
        StoredImage {
            reference: reference.to_string(),
            digest: digest.to_string(),
            size_bytes: size,
            pulled_at: Utc::now(),
            last_used: Utc::now(),
            path: PathBuf::from("/tmp/test"),
        }
    }

    // --- filter tests ---

    #[test]
    fn test_glob_match() {
        assert!(glob_match("webapp", "webapp"));
        assert!(glob_match("web*", "webapp"));
        assert!(glob_match("web*:2", "webapp:2"));
        assert!(!glob_match("web*:2", "webapp:1"));
        assert!(glob_match("*", "anything"));
        assert!(!glob_match("db", "webapp"));
    }

    #[test]
    fn test_image_filter_parse() {
        assert!(matches!(
            ImageFilter::parse("reference=alpine").unwrap(),
            ImageFilter::Reference(p) if p == "alpine"
        ));
        assert!(matches!(
            ImageFilter::parse("label=tier=web").unwrap(),
            ImageFilter::Label(k, Some(v)) if k == "tier" && v == "web"
        ));
        assert!(matches!(
            ImageFilter::parse("label=tier").unwrap(),
            ImageFilter::Label(k, None) if k == "tier"
        ));
        assert!(ImageFilter::parse("nocolon").is_err());
        assert!(ImageFilter::parse("dangling=true").is_err());
    }

    #[test]
    fn test_image_matches_reference() {
        let img = sample_stored("docker.io/library/webapp:2", "sha256:abc", 100);
        assert!(image_matches(
            &img,
            &ImageFilter::Reference("webapp".into())
        ));
        assert!(image_matches(
            &img,
            &ImageFilter::Reference("web*:2".into())
        ));
        assert!(image_matches(
            &img,
            &ImageFilter::Reference("docker.io/library/webapp:2".into())
        ));
        assert!(!image_matches(&img, &ImageFilter::Reference("db".into())));
    }

    // --- ImageRow::from_stored tests ---

    #[test]
    fn test_from_stored_simple_name() {
        let stored = sample_stored("nginx:1.25", "sha256:abcdef1234567890abcdef", 1024);
        let row = ImageRow::from_stored(&stored);

        assert_eq!(row.repository, "docker.io/library/nginx");
        assert_eq!(row.tag, "1.25");
        assert_eq!(row.digest, "sha256:abcdef123456");
        assert_eq!(row.reference, "nginx:1.25");
    }

    #[test]
    fn test_from_stored_custom_registry() {
        let stored = sample_stored(
            "ghcr.io/a3s-box/code:v0.1.0",
            "sha256:aabbccdd11223344aabbccdd",
            2048,
        );
        let row = ImageRow::from_stored(&stored);

        assert_eq!(row.repository, "ghcr.io/a3s-box/code");
        assert_eq!(row.tag, "v0.1.0");
        assert_eq!(row.digest, "sha256:aabbccdd1122");
    }

    #[test]
    fn test_from_stored_no_tag_defaults_latest() {
        let stored = sample_stored("alpine", "sha256:1234567890ab", 512);
        let row = ImageRow::from_stored(&stored);

        assert_eq!(row.repository, "docker.io/library/alpine");
        assert_eq!(row.tag, "latest");
    }

    #[test]
    fn test_from_stored_digest_short() {
        let stored = sample_stored("nginx:latest", "sha256:abcd", 100);
        let row = ImageRow::from_stored(&stored);

        // Short digest should not be truncated
        assert_eq!(row.digest, "sha256:abcd");
    }

    #[test]
    fn test_from_stored_digest_no_prefix() {
        let stored = sample_stored("nginx:latest", "abcdef1234567890", 100);
        let row = ImageRow::from_stored(&stored);

        // Without sha256: prefix, truncate to 12 chars
        assert_eq!(row.digest, "abcdef123456");
    }

    #[test]
    fn test_from_stored_digest_exactly_12() {
        let stored = sample_stored("nginx:latest", "sha256:abcdef123456", 100);
        let row = ImageRow::from_stored(&stored);

        assert_eq!(row.digest, "sha256:abcdef123456");
    }

    #[test]
    fn test_from_stored_invalid_reference_fallback() {
        // An empty reference is treated as dangling and rendered `<none> <none>`,
        // matching Docker's display for untagged images.
        let stored = sample_stored("", "sha256:abc", 100);
        let row = ImageRow::from_stored(&stored);

        assert_eq!(row.repository, "<none>");
        assert_eq!(row.tag, "<none>");
    }

    #[test]
    fn test_from_stored_digest_keyed_dangling_renders_none_none() {
        // A digest-keyed dangling image (displaced by a re-tag) renders as
        // `<none> <none>`, not repository="sha256" / tag=<64-char hex>.
        let stored = sample_stored(
            "sha256:6c3a24f143efe95ba48765929f1cf75cef0eb96c784f85d588660440796fccbb",
            "sha256:6c3a24f143efe95ba48765929f1cf75cef0eb96c784f85d588660440796fccbb",
            100,
        );
        let row = ImageRow::from_stored(&stored);

        assert_eq!(row.repository, "<none>");
        assert_eq!(row.tag, "<none>");
    }

    // --- ImageRow::apply_format tests ---

    #[test]
    fn test_apply_format_repository() {
        let row = ImageRow {
            reference: "nginx:1.25".to_string(),
            repository: "docker.io/library/nginx".to_string(),
            tag: "1.25".to_string(),
            digest: "sha256:abcdef123456".to_string(),
            size: "1.0 KB".to_string(),
            pulled: "5 minutes ago".to_string(),
        };

        assert_eq!(
            row.apply_format("{{.Repository}}:{{.Tag}}"),
            "docker.io/library/nginx:1.25"
        );
    }

    #[test]
    fn test_apply_format_all_fields() {
        let row = ImageRow {
            reference: "nginx:1.25".to_string(),
            repository: "docker.io/library/nginx".to_string(),
            tag: "1.25".to_string(),
            digest: "sha256:abcdef123456".to_string(),
            size: "1.0 KB".to_string(),
            pulled: "5 minutes ago".to_string(),
        };

        let result = row.apply_format("{{.Reference}} {{.Digest}} {{.Size}} {{.Pulled}}");
        assert_eq!(
            result,
            "nginx:1.25 sha256:abcdef123456 1.0 KB 5 minutes ago"
        );
    }

    #[test]
    fn test_apply_format_no_placeholders() {
        let row = ImageRow {
            reference: "nginx:1.25".to_string(),
            repository: "docker.io/library/nginx".to_string(),
            tag: "1.25".to_string(),
            digest: "sha256:abcdef123456".to_string(),
            size: "1.0 KB".to_string(),
            pulled: "5 minutes ago".to_string(),
        };

        assert_eq!(row.apply_format("plain text"), "plain text");
    }

    #[test]
    fn test_apply_format_repeated_placeholder() {
        let row = ImageRow {
            reference: "nginx:1.25".to_string(),
            repository: "docker.io/library/nginx".to_string(),
            tag: "1.25".to_string(),
            digest: "sha256:abcdef123456".to_string(),
            size: "1.0 KB".to_string(),
            pulled: "5 minutes ago".to_string(),
        };

        assert_eq!(row.apply_format("{{.Tag}}-{{.Tag}}"), "1.25-1.25");
    }
}
