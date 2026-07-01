//! Helpers for deciding which stored images are protected by box records.

use std::collections::HashSet;

use a3s_box_runtime::StoredImage;

use crate::state::{BoxRecord, StateFile};
use crate::status;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageReferenceScope {
    /// Protect images referenced by any existing box record.
    AllBoxes,
    /// Protect images referenced by active boxes only.
    ActiveBoxes,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImagePruneMode {
    /// Prune only dangling local image references.
    Dangling,
    /// Prune all image references not protected by the selected scope.
    Unused,
}

pub fn referenced_images(state: &StateFile, scope: ImageReferenceScope) -> HashSet<String> {
    let mut references = HashSet::new();
    for record in state.records() {
        if should_include_record(record, scope) {
            insert_reference_aliases(&mut references, &record.image);
        }
    }
    references
}

pub fn is_prunable_reference(
    reference: &str,
    protected_references: &HashSet<String>,
    mode: ImagePruneMode,
) -> bool {
    if is_protected_reference(reference, protected_references) {
        return false;
    }

    match mode {
        ImagePruneMode::Dangling => is_dangling_reference(reference),
        ImagePruneMode::Unused => true,
    }
}

pub fn is_protected_reference(reference: &str, protected_references: &HashSet<String>) -> bool {
    let aliases = reference_aliases(reference);
    !aliases.is_disjoint(protected_references)
}

pub fn is_dangling_reference(reference: &str) -> bool {
    let reference = reference.trim();
    reference.is_empty()
        || reference == "<none>"
        || reference == "<none>:<none>"
        || reference.starts_with("sha256:")
}

fn should_include_record(record: &BoxRecord, scope: ImageReferenceScope) -> bool {
    match scope {
        ImageReferenceScope::AllBoxes => true,
        ImageReferenceScope::ActiveBoxes => status::is_active(record),
    }
}

pub fn reference_aliases(reference: &str) -> HashSet<String> {
    let mut aliases = HashSet::new();
    let reference = reference.trim();
    if reference.is_empty() {
        return aliases;
    }

    aliases.insert(reference.to_string());
    if !is_digest_reference(reference) {
        if let Ok(parsed) = a3s_box_runtime::ImageReference::parse(reference) {
            aliases.insert(parsed.full_reference());
        }
    }
    aliases
}

pub fn resolve_stored_image(
    images: &[StoredImage],
    query: &str,
) -> Result<Option<StoredImage>, String> {
    let query = query.trim();
    if query.is_empty() {
        return Ok(None);
    }

    let exact_matches = matching_images(images, query, MatchMode::Exact);
    if exact_matches.len() == 1 {
        return Ok(exact_matches.into_iter().next());
    }
    if exact_matches.len() > 1 {
        return Err(ambiguous_reference_error(query, &exact_matches));
    }

    let alias_matches = matching_images(images, query, MatchMode::Alias);
    if alias_matches.len() == 1 {
        return Ok(alias_matches.into_iter().next());
    }
    if alias_matches.len() > 1 {
        return Err(ambiguous_reference_error(query, &alias_matches));
    }

    let digest_matches = matching_images(images, query, MatchMode::Digest);
    if digest_matches.len() == 1 {
        return Ok(digest_matches.into_iter().next());
    }
    if digest_matches.len() > 1 {
        return Err(ambiguous_reference_error(query, &digest_matches));
    }

    Ok(None)
}

/// All images matching `query` (exact, then alias, then digest — first mode
/// with any match wins). Used by `rmi --force`, where an ambiguous digest
/// should remove every reference sharing it rather than erroring.
pub fn all_matching_images(images: &[StoredImage], query: &str) -> Vec<StoredImage> {
    let query = query.trim();
    if query.is_empty() {
        return Vec::new();
    }
    for mode in [MatchMode::Exact, MatchMode::Alias, MatchMode::Digest] {
        let matches = matching_images(images, query, mode);
        if !matches.is_empty() {
            return matches;
        }
    }
    Vec::new()
}

pub fn resolve_required_stored_image(
    images: &[StoredImage],
    query: &str,
) -> Result<StoredImage, String> {
    resolve_stored_image(images, query)?.ok_or_else(|| format!("Image not found: {query}"))
}

fn insert_reference_aliases(references: &mut HashSet<String>, reference: &str) {
    for alias in reference_aliases(reference) {
        references.insert(alias);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MatchMode {
    Exact,
    Alias,
    Digest,
}

fn matching_images(images: &[StoredImage], query: &str, mode: MatchMode) -> Vec<StoredImage> {
    let query_aliases = reference_aliases(query);
    let mut matches = Vec::new();

    for image in images {
        let matched = match mode {
            MatchMode::Exact => image.reference == query,
            MatchMode::Alias => {
                let image_aliases = reference_aliases(&image.reference);
                !image_aliases.is_disjoint(&query_aliases)
            }
            MatchMode::Digest => is_digest_reference(query) && digest_matches(&image.digest, query),
        };

        if matched
            && !matches
                .iter()
                .any(|m: &StoredImage| m.reference == image.reference)
        {
            matches.push(image.clone());
        }
    }

    matches
}

fn ambiguous_reference_error(query: &str, matches: &[StoredImage]) -> String {
    let mut references: Vec<_> = matches
        .iter()
        .map(|image| image.reference.as_str())
        .collect();
    references.sort_unstable();
    format!(
        "Image reference '{query}' is ambiguous; it matches: {}",
        references.join(", ")
    )
}

fn is_digest_reference(reference: &str) -> bool {
    if reference.starts_with("sha256:") {
        return true;
    }
    // A bare lowercase-hex string is a Docker image id (short id = 12 hex,
    // full = 64). Tags/repos that happen to be all-hex are still matched first
    // by Exact/Alias, so this only catches genuine id queries.
    let len = reference.len();
    (12..=64).contains(&len)
        && reference
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn digest_matches(stored_digest: &str, query: &str) -> bool {
    if stored_digest == query {
        return true;
    }
    // Accept both `sha256:<hex>` and a bare `<hex>` query, prefix-matching the
    // stored digest's hex (so `rmi 5b10f432ef3d` resolves the image).
    let query_hex = query.strip_prefix("sha256:").unwrap_or(query);
    if query_hex.is_empty() {
        return false;
    }
    let stored_hex = stored_digest
        .strip_prefix("sha256:")
        .unwrap_or(stored_digest);
    stored_hex.starts_with(query_hex)
}

pub fn normalize_reference(reference: &str) -> String {
    if is_digest_reference(reference) {
        return reference.to_string();
    }

    if let Ok(parsed) = a3s_box_runtime::ImageReference::parse(reference) {
        return parsed.full_reference();
    }
    reference.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_helpers::fixtures::{make_record, setup_state};
    use chrono::Utc;
    use std::path::PathBuf;

    fn stored(reference: &str, digest: &str) -> StoredImage {
        StoredImage {
            reference: reference.to_string(),
            digest: digest.to_string(),
            size_bytes: 1024,
            pulled_at: Utc::now(),
            last_used: Utc::now(),
            path: PathBuf::from("/tmp").join(digest.replace(':', "_")),
        }
    }

    #[test]
    fn test_referenced_images_all_boxes_includes_inactive_and_normalized_aliases() {
        let mut running = make_record("id-1", "running", "running", Some(1));
        running.image = "alpine:latest".to_string();
        let mut stopped = make_record("id-2", "stopped", "stopped", None);
        stopped.image = "nginx:1.25".to_string();
        let (_tmp, state) = setup_state(vec![running, stopped]);

        let references = referenced_images(&state, ImageReferenceScope::AllBoxes);

        assert!(references.contains("alpine:latest"));
        assert!(references.contains("docker.io/library/alpine:latest"));
        assert!(references.contains("nginx:1.25"));
        assert!(references.contains("docker.io/library/nginx:1.25"));
    }

    #[test]
    fn test_referenced_images_active_boxes_include_paused_but_exclude_stopped() {
        let mut running = make_record("id-1", "running", "running", Some(1));
        running.image = "alpine:latest".to_string();
        let mut paused = make_record("id-2", "paused", "paused", Some(1));
        paused.image = "redis:latest".to_string();
        let mut stopped = make_record("id-3", "stopped", "stopped", None);
        stopped.image = "nginx:latest".to_string();
        let (_tmp, state) = setup_state(vec![running, paused, stopped]);

        let references = referenced_images(&state, ImageReferenceScope::ActiveBoxes);

        assert!(references.contains("alpine:latest"));
        assert!(references.contains("redis:latest"));
        assert!(!references.contains("nginx:latest"));
    }

    #[test]
    fn test_is_dangling_reference() {
        assert!(is_dangling_reference("sha256:abcdef"));
        assert!(is_dangling_reference("<none>:<none>"));
        assert!(!is_dangling_reference("alpine:latest"));
        assert!(!is_dangling_reference("docker.io/library/alpine:latest"));
    }

    #[test]
    fn test_is_prunable_reference_respects_mode_and_protection() {
        let mut protected = HashSet::new();
        protected.insert("docker.io/library/alpine:latest".to_string());

        assert!(!is_prunable_reference(
            "alpine:latest",
            &protected,
            ImagePruneMode::Unused
        ));
        assert!(!is_prunable_reference(
            "redis:latest",
            &protected,
            ImagePruneMode::Dangling
        ));
        assert!(is_prunable_reference(
            "sha256:abcdef",
            &protected,
            ImagePruneMode::Dangling
        ));
        assert!(is_prunable_reference(
            "redis:latest",
            &protected,
            ImagePruneMode::Unused
        ));
    }

    #[test]
    fn test_is_protected_reference_checks_aliases() {
        let mut protected = HashSet::new();
        protected.insert("docker.io/library/alpine:latest".to_string());

        assert!(is_protected_reference("alpine:latest", &protected));
        assert!(is_protected_reference(
            "docker.io/library/alpine:latest",
            &protected
        ));
        assert!(!is_protected_reference("redis:latest", &protected));
    }

    #[test]
    fn test_reference_aliases_include_normalized_docker_hub_name() {
        let aliases = reference_aliases("alpine:latest");

        assert!(aliases.contains("alpine:latest"));
        assert!(aliases.contains("docker.io/library/alpine:latest"));
    }

    #[test]
    fn test_reference_aliases_do_not_normalize_digest_references() {
        let aliases = reference_aliases("sha256:abcdef");

        assert!(aliases.contains("sha256:abcdef"));
        assert_eq!(aliases.len(), 1);
    }

    #[test]
    fn test_resolve_stored_image_matches_normalized_alias() {
        let images = vec![stored("docker.io/library/alpine:latest", "sha256:abc")];

        let resolved = resolve_stored_image(&images, "alpine:latest")
            .unwrap()
            .unwrap();

        assert_eq!(resolved.reference, "docker.io/library/alpine:latest");
    }

    #[test]
    fn test_resolve_stored_image_matches_digest_reference() {
        let images = vec![stored("docker.io/library/alpine:latest", "sha256:abc")];

        let resolved = resolve_stored_image(&images, "sha256:abc")
            .unwrap()
            .unwrap();

        assert_eq!(resolved.reference, "docker.io/library/alpine:latest");
    }

    #[test]
    fn test_resolve_stored_image_matches_digest_prefix() {
        let images = vec![stored(
            "docker.io/library/alpine:latest",
            "sha256:abcdef123456",
        )];

        let resolved = resolve_stored_image(&images, "sha256:abcdef")
            .unwrap()
            .unwrap();

        assert_eq!(resolved.reference, "docker.io/library/alpine:latest");
    }

    #[test]
    fn test_resolve_stored_image_reports_ambiguous_digest() {
        let images = vec![
            stored("alpine:latest", "sha256:abc"),
            stored("docker.io/library/alpine:latest", "sha256:abc"),
        ];

        let error = resolve_stored_image(&images, "sha256:abc").unwrap_err();

        assert!(error.contains("ambiguous"));
        assert!(error.contains("alpine:latest"));
        assert!(error.contains("docker.io/library/alpine:latest"));
    }

    #[test]
    fn test_resolve_stored_image_reports_ambiguous_digest_prefix() {
        let images = vec![
            stored("alpine:latest", "sha256:abcdef111111"),
            stored("busybox:latest", "sha256:abcdef222222"),
        ];

        let error = resolve_stored_image(&images, "sha256:abcdef").unwrap_err();

        assert!(error.contains("ambiguous"));
        assert!(error.contains("alpine:latest"));
        assert!(error.contains("busybox:latest"));
    }

    #[test]
    fn test_normalize_reference() {
        assert_eq!(
            normalize_reference("alpine:latest"),
            "docker.io/library/alpine:latest"
        );
        assert_eq!(normalize_reference("sha256:abcdef"), "sha256:abcdef");
    }

    #[test]
    fn test_digest_matches_exact_and_prefix_queries() {
        assert!(digest_matches("sha256:abcdef123456", "sha256:abcdef123456"));
        assert!(digest_matches("sha256:abcdef123456", "sha256:abcdef"));
        assert!(!digest_matches("sha256:abcdef123456", "sha256:"));
        // A bare hex query prefix-matches the stored digest's hex (Docker short id).
        assert!(digest_matches("sha256:abcdef123456", "abcdef"));
        assert!(!digest_matches("sha256:abcdef123456", "ffffff"));
    }

    #[test]
    fn test_is_digest_reference_bare_hex_short_id() {
        assert!(is_digest_reference("sha256:abc"));
        // 12-hex Docker short id.
        assert!(is_digest_reference("5b10f432ef3d"));
        // Normal tags/repos are not treated as digests.
        assert!(!is_digest_reference("alpine"));
        assert!(!is_digest_reference("v1"));
        assert!(!is_digest_reference("latest"));
        // Uppercase hex is not a valid image id.
        assert!(!is_digest_reference("5B10F432EF3D"));
    }
}
