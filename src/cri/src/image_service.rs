//! CRI ImageService implementation.
//!
//! Maps CRI image operations to A3S Box ImageStore and ImagePuller.

use std::pin::Pin;
use std::sync::Arc;

use futures::Stream;
use tonic::{Request, Response, Status};

use a3s_box_core::StoredImage;
use a3s_box_runtime::oci::{ImagePuller, ImageStore, OciImage, RegistryAuth};

use crate::cri_api::image_service_server::ImageService;
use crate::cri_api::*;
use crate::error::box_error_to_status;

type CriImageResponseStream<T> = Pin<Box<dyn Stream<Item = Result<T, Status>> + Send + 'static>>;

/// Strip a `:tag` suffix from an image reference, leaving the repository name.
///
/// A `:` only separates a tag when it appears in the final path segment, so
/// registry ports such as `host:5000/img` are preserved.
fn repo_name(reference: &str) -> &str {
    let segment_start = reference.rfind('/').map_or(0, |i| i + 1);
    match reference[segment_start..].find(':') {
        Some(rel) => &reference[..segment_start + rel],
        None => reference,
    }
}

/// Split an OCI config `User` string into a CRI uid / username.
///
/// The user may be `uid`, `uid:gid`, `name`, or `name:group`. A numeric
/// principal yields a uid; a named principal yields a username.
fn parse_image_user(user: &str) -> (Option<i64>, String) {
    let principal = user.split(':').next().unwrap_or(user);
    if principal.is_empty() {
        return (None, String::new());
    }
    match principal.parse::<i64>() {
        Ok(uid) => (Some(uid), String::new()),
        Err(_) => (None, principal.to_string()),
    }
}

/// Read the configured user of a stored image from its on-disk OCI config.
///
/// Returns empty values when the image declares no user or its config can't be
/// read (the CRI `Image` then reports neither a uid nor a username).
fn image_user(path: &std::path::Path) -> (Option<i64>, String) {
    OciImage::from_path(path)
        .ok()
        .and_then(|image| image.config().user.clone())
        .filter(|user| !user.is_empty())
        .map(|user| parse_image_user(&user))
        .unwrap_or((None, String::new()))
}

/// Build a single CRI [`Image`] from one or more stored images that share a
/// content digest.
///
/// CRI identifies an image by its digest (`id`); multiple tags pointing at the
/// same digest collapse into one image whose `repo_tags` lists every tag. The
/// group must be non-empty.
fn coalesced_image(group: &[StoredImage]) -> Image {
    let first = &group[0];
    let mut repo_tags: Vec<String> = Vec::new();
    let mut repo_digests: Vec<String> = Vec::new();
    for img in group {
        if img.reference.contains('@') {
            // A digest-pinned reference (`repo@sha256:...`) is a repo digest,
            // not a tag — an image pulled by digest has no repo tags.
            repo_digests.push(img.reference.clone());
        } else {
            repo_tags.push(img.reference.clone());
            repo_digests.push(format!("{}@{}", repo_name(&img.reference), first.digest));
        }
    }
    repo_tags.sort();
    repo_tags.dedup();
    repo_digests.sort();
    repo_digests.dedup();
    let (uid, username) = image_user(&first.path);
    Image {
        id: first.digest.clone(),
        repo_tags,
        repo_digests,
        size: first.size_bytes,
        uid: uid.map(|value| Int64Value { value }),
        username,
        spec: Some(ImageSpec {
            image: first.reference.clone(),
            annotations: Default::default(),
        }),
        pinned: false,
    }
}

/// Group stored images by digest into coalesced CRI images.
fn coalesce_images(stored: Vec<StoredImage>) -> Vec<Image> {
    let mut by_digest: std::collections::BTreeMap<String, Vec<StoredImage>> =
        std::collections::BTreeMap::new();
    for img in stored {
        by_digest.entry(img.digest.clone()).or_default().push(img);
    }
    by_digest
        .into_values()
        .map(|group| coalesced_image(&group))
        .collect()
}

/// Whether an image matches a CRI `ImageFilter` image reference (by id, tag,
/// or digest).
fn image_matches_filter(image: &Image, filter: &str) -> bool {
    image.id == filter
        || image.repo_tags.iter().any(|t| t == filter)
        || image.repo_digests.iter().any(|d| d == filter)
}

/// A3S Box implementation of the CRI ImageService.
/// Convert a CRI per-request `AuthConfig` (kubelet fills it from a pod's
/// `imagePullSecrets`) into a registry credential. Supports the standard
/// username/password shape and the Docker-config base64 `auth` ("user:pass")
/// field. Bearer tokens (`identity_token`/`registry_token`) are not modeled by
/// `RegistryAuth` yet, so a token-only AuthConfig yields `None` and the caller
/// falls back to the service-default credential.
fn auth_config_to_registry_auth(auth: &AuthConfig) -> Option<RegistryAuth> {
    if !auth.username.is_empty() {
        return Some(RegistryAuth::basic(
            auth.username.clone(),
            auth.password.clone(),
        ));
    }
    if !auth.auth.is_empty() {
        use base64::Engine;
        if let Ok(decoded) = base64::engine::general_purpose::STANDARD.decode(auth.auth.trim()) {
            if let Ok(pair) = String::from_utf8(decoded) {
                if let Some((user, pass)) = pair.split_once(':') {
                    return Some(RegistryAuth::basic(user, pass));
                }
            }
        }
    }
    None
}

pub struct BoxImageService {
    image_store: Arc<ImageStore>,
    image_puller: Arc<ImagePuller>,
}

impl BoxImageService {
    /// Create a new BoxImageService.
    pub fn new(image_store: Arc<ImageStore>, auth: RegistryAuth) -> Self {
        let image_puller = Arc::new(ImagePuller::new(image_store.clone(), auth));
        Self {
            image_store,
            image_puller,
        }
    }

    /// Resolve a CRI image reference to its stored content digest (the image
    /// id used by ListImages / ImageStatus).
    ///
    /// CRI may address an image by its exact stored reference, by its image id
    /// (digest), or by an unnormalized name (e.g. without a tag, which
    /// defaults to `:latest`).
    async fn resolve_digest(&self, image: &str) -> Option<String> {
        self.image_store.resolve(image).await.map(|img| img.digest)
    }
}

#[tonic::async_trait]
impl ImageService for BoxImageService {
    type StreamImagesStream = CriImageResponseStream<StreamImagesResponse>;

    async fn list_images(
        &self,
        request: Request<ListImagesRequest>,
    ) -> Result<Response<ListImagesResponse>, Status> {
        let req = request.into_inner();
        let filter = req
            .filter
            .and_then(|filter| filter.image)
            .map(|image| image.image)
            .filter(|image| !image.is_empty());

        let mut images = coalesce_images(self.image_store.list().await);
        if let Some(filter) = filter {
            images.retain(|image| image_matches_filter(image, &filter));
        }

        Ok(Response::new(ListImagesResponse { images }))
    }

    async fn stream_images(
        &self,
        request: Request<StreamImagesRequest>,
    ) -> Result<Response<Self::StreamImagesStream>, Status> {
        let req = request.into_inner();
        let image_filter = req
            .filter
            .and_then(|filter| filter.image)
            .map(|image| image.image)
            .filter(|image| !image.is_empty());

        let mut images = coalesce_images(self.image_store.list().await);
        if let Some(image_filter) = image_filter {
            images.retain(|image| image_matches_filter(image, &image_filter));
        }

        let stream: Self::StreamImagesStream =
            Box::pin(tokio_stream::iter(vec![Ok(StreamImagesResponse {
                images,
            })]));
        Ok(Response::new(stream))
    }

    async fn image_status(
        &self,
        request: Request<ImageStatusRequest>,
    ) -> Result<Response<ImageStatusResponse>, Status> {
        let req = request.into_inner();
        let image_spec = req
            .image
            .ok_or_else(|| Status::invalid_argument("image spec required"))?;
        if image_spec.image.trim().is_empty() {
            // Reject an empty reference up front; otherwise it fails deep in
            // reference parsing with a confusing error.
            return Err(Status::invalid_argument(
                "image reference must not be empty",
            ));
        }

        let digest = self.resolve_digest(&image_spec.image).await;

        // Coalesce every reference sharing the digest into one image so
        // `repo_tags` reflects all tags (CRI identifies images by digest).
        let image = match digest {
            Some(digest) => {
                let group: Vec<StoredImage> = self
                    .image_store
                    .list()
                    .await
                    .into_iter()
                    .filter(|img| img.digest == digest)
                    .collect();
                (!group.is_empty()).then(|| coalesced_image(&group))
            }
            None => None,
        };

        Ok(Response::new(ImageStatusResponse {
            image,
            info: Default::default(),
        }))
    }

    async fn pull_image(
        &self,
        request: Request<PullImageRequest>,
    ) -> Result<Response<PullImageResponse>, Status> {
        let req = request.into_inner();
        let image_spec = req
            .image
            .ok_or_else(|| Status::invalid_argument("image spec required"))?;
        if image_spec.image.trim().is_empty() {
            // Reject an empty reference up front; otherwise it fails deep in
            // reference parsing with a confusing error.
            return Err(Status::invalid_argument(
                "image reference must not be empty",
            ));
        }

        // Honor kubelet's per-request credentials (imagePullSecrets). When the
        // request carries usable auth, pull with a one-off puller built from it;
        // otherwise fall back to the service-default registry credential.
        match req.auth.as_ref().and_then(auth_config_to_registry_auth) {
            Some(auth) => {
                tracing::info!(image = %image_spec.image, "CRI PullImage (request auth)");
                ImagePuller::new(self.image_store.clone(), auth)
                    .pull(&image_spec.image)
                    .await
                    .map_err(box_error_to_status)?;
            }
            None => {
                tracing::info!(image = %image_spec.image, "CRI PullImage");
                self.image_puller
                    .pull(&image_spec.image)
                    .await
                    .map_err(box_error_to_status)?;
            }
        }

        // CRI uses the content-addressable image id (digest) as the canonical
        // image_ref, so callers can dedupe different tags of the same image.
        // Fall back to the requested reference if the digest can't be resolved.
        let image_ref = self
            .resolve_digest(&image_spec.image)
            .await
            .unwrap_or(image_spec.image);

        Ok(Response::new(PullImageResponse { image_ref }))
    }

    async fn remove_image(
        &self,
        request: Request<RemoveImageRequest>,
    ) -> Result<Response<RemoveImageResponse>, Status> {
        let req = request.into_inner();
        let image_spec = req
            .image
            .ok_or_else(|| Status::invalid_argument("image spec required"))?;
        if image_spec.image.trim().is_empty() {
            // Reject an empty reference up front; otherwise it fails deep in
            // reference parsing with a confusing error.
            return Err(Status::invalid_argument(
                "image reference must not be empty",
            ));
        }

        tracing::info!(image = %image_spec.image, "CRI RemoveImage");

        // Resolve the reference to its digest the same way ImageStatus does, so
        // `rmi <short-tag>` matches an image stored under its fully-qualified name
        // (ImageStatus resolved it but RemoveImage previously passed the raw ref to
        // store.remove, which only exact-matches a key or digest). Removing by the
        // resolved digest drops every reference to the image — the CRI
        // "remove the image" semantic. Fall back to the raw ref (a bare digest/id,
        // or an unknown image) so store.remove still handles it / errors as before.
        let target = self
            .resolve_digest(&image_spec.image)
            .await
            .unwrap_or_else(|| image_spec.image.clone());

        self.image_store
            .remove(&target)
            .await
            .map_err(box_error_to_status)?;

        Ok(Response::new(RemoveImageResponse {}))
    }

    async fn image_fs_info(
        &self,
        _request: Request<ImageFsInfoRequest>,
    ) -> Result<Response<ImageFsInfoResponse>, Status> {
        let total_bytes = self.image_store.total_size().await;

        let usage = FilesystemUsage {
            timestamp: chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0),
            fs_id: Some(FilesystemIdentifier {
                mountpoint: self.image_store.store_dir().to_string_lossy().to_string(),
            }),
            used_bytes: Some(UInt64Value { value: total_bytes }),
            inodes_used: None,
        };

        Ok(Response::new(ImageFsInfoResponse {
            image_filesystems: vec![usage],
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use a3s_box_runtime::oci::ImageStore;
    use futures::StreamExt;

    /// Create a test ImageStore backed by a temp directory.
    fn make_test_store() -> (Arc<ImageStore>, tempfile::TempDir) {
        let tmp = tempfile::tempdir().unwrap();
        let store_dir = tmp.path().join("images");
        let store = ImageStore::new(&store_dir, 100 * 1024 * 1024).unwrap();
        (Arc::new(store), tmp)
    }

    /// Create a BoxImageService for testing.
    fn make_test_service() -> (BoxImageService, tempfile::TempDir) {
        let (store, tmp) = make_test_store();
        let svc = BoxImageService::new(store, RegistryAuth::anonymous());
        (svc, tmp)
    }

    /// Put a fake image into the store for testing.
    async fn put_test_image(store: &ImageStore, reference: &str, digest: &str) {
        let tmp = tempfile::tempdir().unwrap();
        // Create a minimal OCI layout
        std::fs::write(
            tmp.path().join("oci-layout"),
            r#"{"imageLayoutVersion":"1.0.0"}"#,
        )
        .unwrap();
        let blobs_dir = tmp.path().join("blobs").join("sha256");
        std::fs::create_dir_all(&blobs_dir).unwrap();
        std::fs::write(blobs_dir.join("dummy"), b"test layer data").unwrap();

        store.put(reference, digest, tmp.path()).await.unwrap();
    }

    // ── ListImages ───────────────────────────────────────────────────

    #[test]
    fn test_parse_image_user() {
        assert_eq!(parse_image_user("1002"), (Some(1002), String::new()));
        assert_eq!(parse_image_user("1002:1002"), (Some(1002), String::new()));
        assert_eq!(parse_image_user("nobody"), (None, "nobody".to_string()));
        assert_eq!(
            parse_image_user("nobody:nogroup"),
            (None, "nobody".to_string())
        );
        assert_eq!(parse_image_user(""), (None, String::new()));
    }

    #[tokio::test]
    async fn test_list_images_empty() {
        let (svc, _tmp) = make_test_service();
        let resp = svc
            .list_images(Request::new(ListImagesRequest { filter: None }))
            .await
            .unwrap()
            .into_inner();
        assert!(resp.images.is_empty());
    }

    #[tokio::test]
    async fn test_list_images_with_entries() {
        let (svc, _tmp) = make_test_service();
        put_test_image(&svc.image_store, "nginx:latest", "sha256:aaa111").await;
        put_test_image(&svc.image_store, "alpine:3.18", "sha256:bbb222").await;

        let resp = svc
            .list_images(Request::new(ListImagesRequest { filter: None }))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(resp.images.len(), 2);

        // Verify image fields
        let refs: Vec<&str> = resp
            .images
            .iter()
            .flat_map(|i| &i.repo_tags)
            .map(|s| s.as_str())
            .collect();
        assert!(refs.contains(&"nginx:latest"));
        assert!(refs.contains(&"alpine:3.18"));
    }

    #[tokio::test]
    async fn test_list_images_coalesces_tags_by_digest() {
        // Three tags of one digest must collapse into a single image whose
        // repo_tags lists all three (CRI identifies images by digest).
        let (svc, _tmp) = make_test_service();
        put_test_image(&svc.image_store, "img:1", "sha256:same").await;
        put_test_image(&svc.image_store, "img:2", "sha256:same").await;
        put_test_image(&svc.image_store, "img:3", "sha256:same").await;

        let resp = svc
            .list_images(Request::new(ListImagesRequest { filter: None }))
            .await
            .unwrap()
            .into_inner();

        assert_eq!(resp.images.len(), 1, "one digest => one image");
        let mut tags = resp.images[0].repo_tags.clone();
        tags.sort();
        assert_eq!(tags, vec!["img:1", "img:2", "img:3"]);
        assert_eq!(resp.images[0].id, "sha256:same");
    }

    #[tokio::test]
    async fn test_list_images_digest_pin_is_repo_digest_not_tag() {
        // An image pulled by digest (`repo@sha256:...`) has no repo tag; the
        // pinned reference must be reported as a repo digest.
        let (svc, _tmp) = make_test_service();
        put_test_image(
            &svc.image_store,
            "gcr.io/x/img@sha256:pinned",
            "sha256:pinned",
        )
        .await;

        let resp = svc
            .list_images(Request::new(ListImagesRequest { filter: None }))
            .await
            .unwrap()
            .into_inner();

        assert_eq!(resp.images.len(), 1);
        assert!(
            resp.images[0].repo_tags.is_empty(),
            "no tag for a digest pin"
        );
        assert!(resp.images[0]
            .repo_digests
            .contains(&"gcr.io/x/img@sha256:pinned".to_string()));
    }

    #[tokio::test]
    async fn test_image_status_resolves_untagged_name() {
        // CRI ImageStatus may be queried by an untagged name; it must resolve
        // to the stored `:latest` reference and report it in repo_tags.
        let (svc, _tmp) = make_test_service();
        put_test_image(
            &svc.image_store,
            "docker.io/library/nginx:latest",
            "sha256:n1",
        )
        .await;

        let resp = svc
            .image_status(Request::new(ImageStatusRequest {
                image: Some(ImageSpec {
                    image: "nginx".to_string(),
                    annotations: Default::default(),
                }),
                verbose: false,
            }))
            .await
            .unwrap()
            .into_inner();

        let image = resp.image.expect("untagged name should resolve");
        assert_eq!(image.id, "sha256:n1");
        assert!(image
            .repo_tags
            .contains(&"docker.io/library/nginx:latest".to_string()));
    }

    #[tokio::test]
    async fn test_image_status_resolves_by_digest() {
        // ImageStatus queried by image id (digest) must return the image.
        let (svc, _tmp) = make_test_service();
        put_test_image(&svc.image_store, "redis:7", "sha256:r7").await;

        let resp = svc
            .image_status(Request::new(ImageStatusRequest {
                image: Some(ImageSpec {
                    image: "sha256:r7".to_string(),
                    annotations: Default::default(),
                }),
                verbose: false,
            }))
            .await
            .unwrap()
            .into_inner();

        let image = resp.image.expect("digest should resolve");
        assert_eq!(image.id, "sha256:r7");
        assert!(image.repo_tags.contains(&"redis:7".to_string()));
    }

    #[tokio::test]
    async fn test_image_status_resolves_by_name_at_digest() {
        // ImageStatus queried by a `name@sha256:...` digest pin must resolve.
        let (svc, _tmp) = make_test_service();
        put_test_image(&svc.image_store, "busybox:1.36", "sha256:bb36").await;

        let resp = svc
            .image_status(Request::new(ImageStatusRequest {
                image: Some(ImageSpec {
                    image: "busybox@sha256:bb36".to_string(),
                    annotations: Default::default(),
                }),
                verbose: false,
            }))
            .await
            .unwrap()
            .into_inner();

        let image = resp.image.expect("name@digest should resolve");
        assert_eq!(image.id, "sha256:bb36");
    }

    #[tokio::test]
    async fn test_stream_images_with_filter() {
        let (svc, _tmp) = make_test_service();
        put_test_image(&svc.image_store, "nginx:latest", "sha256:aaa111").await;
        put_test_image(&svc.image_store, "alpine:3.18", "sha256:bbb222").await;

        let mut stream = svc
            .stream_images(Request::new(StreamImagesRequest {
                filter: Some(ImageFilter {
                    image: Some(ImageSpec {
                        image: "alpine:3.18".to_string(),
                        annotations: Default::default(),
                    }),
                }),
            }))
            .await
            .unwrap()
            .into_inner();

        let response = stream.next().await.unwrap().unwrap();
        assert_eq!(response.images.len(), 1);
        assert_eq!(
            response.images[0].spec.as_ref().unwrap().image,
            "alpine:3.18"
        );
        assert!(stream.next().await.is_none());
    }

    // ── ImageStatus ──────────────────────────────────────────────────

    #[tokio::test]
    async fn test_image_status_missing_spec() {
        let (svc, _tmp) = make_test_service();
        let result = svc
            .image_status(Request::new(ImageStatusRequest {
                image: None,
                verbose: false,
            }))
            .await;
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().code(), tonic::Code::InvalidArgument);
    }

    #[tokio::test]
    async fn test_image_status_not_found() {
        let (svc, _tmp) = make_test_service();
        let resp = svc
            .image_status(Request::new(ImageStatusRequest {
                image: Some(ImageSpec {
                    image: "nonexistent:latest".to_string(),
                    annotations: Default::default(),
                }),
                verbose: false,
            }))
            .await
            .unwrap()
            .into_inner();
        assert!(resp.image.is_none());
    }

    #[tokio::test]
    async fn test_image_status_found() {
        let (svc, _tmp) = make_test_service();
        put_test_image(&svc.image_store, "nginx:latest", "sha256:aaa111").await;

        let resp = svc
            .image_status(Request::new(ImageStatusRequest {
                image: Some(ImageSpec {
                    image: "nginx:latest".to_string(),
                    annotations: Default::default(),
                }),
                verbose: false,
            }))
            .await
            .unwrap()
            .into_inner();

        let image = resp.image.unwrap();
        assert_eq!(image.id, "sha256:aaa111");
        assert!(image.repo_tags.contains(&"nginx:latest".to_string()));
    }

    // ── RemoveImage ──────────────────────────────────────────────────

    #[tokio::test]
    async fn test_remove_image_missing_spec() {
        let (svc, _tmp) = make_test_service();
        let result = svc
            .remove_image(Request::new(RemoveImageRequest { image: None }))
            .await;
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().code(), tonic::Code::InvalidArgument);
    }

    #[tokio::test]
    async fn test_remove_image_not_found() {
        let (svc, _tmp) = make_test_service();
        let result = svc
            .remove_image(Request::new(RemoveImageRequest {
                image: Some(ImageSpec {
                    image: "nonexistent:latest".to_string(),
                    annotations: Default::default(),
                }),
            }))
            .await;
        // ImageStore.remove returns an error for missing images
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_remove_image_success() {
        let (svc, _tmp) = make_test_service();
        put_test_image(&svc.image_store, "nginx:latest", "sha256:aaa111").await;

        svc.remove_image(Request::new(RemoveImageRequest {
            image: Some(ImageSpec {
                image: "nginx:latest".to_string(),
                annotations: Default::default(),
            }),
        }))
        .await
        .unwrap();

        // Verify it's gone
        assert!(svc.image_store.get("nginx:latest").await.is_none());
    }

    // ── ImageFsInfo ──────────────────────────────────────────────────

    #[tokio::test]
    async fn test_image_fs_info_empty() {
        let (svc, _tmp) = make_test_service();
        let resp = svc
            .image_fs_info(Request::new(ImageFsInfoRequest {}))
            .await
            .unwrap()
            .into_inner();

        assert_eq!(resp.image_filesystems.len(), 1);
        let fs = &resp.image_filesystems[0];
        assert_eq!(fs.used_bytes.as_ref().unwrap().value, 0);
    }

    #[tokio::test]
    async fn test_image_fs_info_with_images() {
        let (svc, _tmp) = make_test_service();
        put_test_image(&svc.image_store, "nginx:latest", "sha256:aaa111").await;

        let resp = svc
            .image_fs_info(Request::new(ImageFsInfoRequest {}))
            .await
            .unwrap()
            .into_inner();

        assert_eq!(resp.image_filesystems.len(), 1);
        let fs = &resp.image_filesystems[0];
        assert!(fs.used_bytes.as_ref().unwrap().value > 0);
        assert!(fs.fs_id.is_some());
    }

    #[test]
    fn test_auth_config_to_registry_auth() {
        // username/password (the common imagePullSecrets shape) -> basic
        let ra = auth_config_to_registry_auth(&AuthConfig {
            username: "alice".into(),
            password: "secret".into(),
            ..Default::default()
        });
        assert!(ra.is_some());
        let dbg = format!("{ra:?}");
        assert!(dbg.contains("alice") && dbg.contains("secret"), "got {dbg}");

        // Docker-config base64 "user:pass" auth field -> basic
        use base64::Engine;
        let encoded = base64::engine::general_purpose::STANDARD.encode("bob:pw123");
        let ra = auth_config_to_registry_auth(&AuthConfig {
            auth: encoded,
            ..Default::default()
        });
        let dbg = format!("{ra:?}");
        assert!(dbg.contains("bob") && dbg.contains("pw123"), "got {dbg}");

        // Bearer-token-only AuthConfig -> None (no RegistryAuth bearer support yet)
        assert!(auth_config_to_registry_auth(&AuthConfig {
            identity_token: "tok".into(),
            ..Default::default()
        })
        .is_none());

        // Empty -> None (caller falls back to the service-default credential)
        assert!(auth_config_to_registry_auth(&AuthConfig::default()).is_none());
    }
}
