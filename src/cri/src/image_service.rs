//! CRI ImageService implementation.
//!
//! Maps CRI image operations to A3S Box ImageStore and ImagePuller.

use std::collections::HashMap;
use std::sync::Arc;

use a3s_box_core::StoredImage;
use base64::Engine;
use tonic::{Request, Response, Status};

use a3s_box_runtime::oci::{ImagePuller, ImageReference, ImageStore, OciImage, RegistryAuth};

use crate::cri_api::image_service_server::ImageService;
use crate::cri_api::*;
use crate::error::box_error_to_status;
use crate::persistent_store::PersistentCriStore;

/// A3S Box implementation of the CRI ImageService.
pub struct BoxImageService {
    image_store: Arc<ImageStore>,
    cri_store: Option<Arc<PersistentCriStore>>,
}

impl BoxImageService {
    /// Create a new BoxImageService.
    pub fn new(image_store: Arc<ImageStore>, _auth: RegistryAuth) -> Self {
        Self {
            image_store,
            cri_store: None,
        }
    }

    /// Attach shared CRI state so image deletion can respect container references.
    pub fn with_cri_store(mut self, store: Arc<PersistentCriStore>) -> Self {
        self.cri_store = Some(store);
        self
    }

    async fn find_image(&self, image: &str) -> Option<StoredImage> {
        self.image_store.find(image).await
    }

    async fn image_in_use(&self, requested: &str, stored: &StoredImage) -> Option<String> {
        let store = self.cri_store.as_ref()?;
        store
            .containers
            .list(None, None)
            .await
            .into_iter()
            .find(|container| image_ref_matches(&container.image_ref, requested, stored))
            .map(|container| container.id)
    }
}

fn image_ref_matches(container_ref: &str, requested: &str, stored: &StoredImage) -> bool {
    let stored_repo_digest = format!("{}@{}", stored.reference, stored.digest);
    let container_digest = container_ref
        .split_once('@')
        .map(|(_, digest)| digest)
        .or_else(|| {
            container_ref
                .starts_with("sha256:")
                .then_some(container_ref)
        });
    let requested_digest = requested
        .split_once('@')
        .map(|(_, digest)| digest)
        .or_else(|| requested.starts_with("sha256:").then_some(requested));

    container_ref == requested
        || container_ref == stored.reference
        || container_ref == stored.digest
        || container_ref == stored_repo_digest
        || container_digest == Some(stored.digest.as_str())
        || requested_digest.is_some_and(|digest| container_digest == Some(digest))
}

#[tonic::async_trait]
impl ImageService for BoxImageService {
    async fn list_images(
        &self,
        request: Request<ListImagesRequest>,
    ) -> Result<Response<ListImagesResponse>, Status> {
        let req = request.into_inner();
        let image_filter = req
            .filter
            .as_ref()
            .and_then(|filter| filter.image.as_ref())
            .map(|image| image.image.as_str())
            .filter(|image| !image.is_empty());

        let stored_images = self.image_store.list().await;

        let images: Vec<Image> = stored_images
            .into_iter()
            .filter(|img| {
                image_filter
                    .map(|filter| stored_image_matches(filter, img))
                    .unwrap_or(true)
            })
            .map(|img| Image {
                id: img.digest.clone(),
                repo_tags: vec![img.reference.clone()],
                repo_digests: vec![format!("{}@{}", img.reference, img.digest)],
                size: img.size_bytes,
                uid: None,
                username: String::new(),
                spec: Some(ImageSpec {
                    image: img.reference,
                    annotations: Default::default(),
                }),
                pinned: false,
            })
            .collect();

        Ok(Response::new(ListImagesResponse { images }))
    }

    async fn image_status(
        &self,
        request: Request<ImageStatusRequest>,
    ) -> Result<Response<ImageStatusResponse>, Status> {
        let req = request.into_inner();
        let image_spec = req
            .image
            .ok_or_else(|| Status::invalid_argument("image spec required"))?;

        let stored = self.find_image(&image_spec.image).await;

        let image = stored.as_ref().map(|img| Image {
            id: img.digest.clone(),
            repo_tags: vec![img.reference.clone()],
            repo_digests: vec![format!("{}@{}", img.reference, img.digest)],
            size: img.size_bytes,
            uid: None,
            username: String::new(),
            spec: Some(ImageSpec {
                image: img.reference.clone(),
                annotations: Default::default(),
            }),
            pinned: false,
        });
        let mut info: HashMap<String, String> = Default::default();
        if req.verbose {
            if let Some(img) = &stored {
                info.insert("a3s.digest".to_string(), img.digest.clone());
                info.insert(
                    "a3s.path".to_string(),
                    img.path.to_string_lossy().to_string(),
                );
                match OciImage::from_path(&img.path) {
                    Ok(oci_image) => {
                        let config = oci_image.config();
                        info.insert(
                            "a3s.entrypoint".to_string(),
                            serde_json::to_string(&config.entrypoint).unwrap_or_default(),
                        );
                        info.insert(
                            "a3s.cmd".to_string(),
                            serde_json::to_string(&config.cmd).unwrap_or_default(),
                        );
                        info.insert(
                            "a3s.env".to_string(),
                            serde_json::to_string(&config.env).unwrap_or_default(),
                        );
                        info.insert(
                            "a3s.working_dir".to_string(),
                            config.working_dir.clone().unwrap_or_default(),
                        );
                        info.insert(
                            "a3s.labels".to_string(),
                            serde_json::to_string(&config.labels).unwrap_or_default(),
                        );
                    }
                    Err(error) => {
                        info.insert("a3s.config_error".to_string(), error.to_string());
                    }
                }
            }
        }

        Ok(Response::new(ImageStatusResponse { image, info }))
    }

    async fn pull_image(
        &self,
        request: Request<PullImageRequest>,
    ) -> Result<Response<PullImageResponse>, Status> {
        let req = request.into_inner();
        let image_spec = req
            .image
            .ok_or_else(|| Status::invalid_argument("image spec required"))?;

        tracing::info!(image = %image_spec.image, "CRI PullImage");

        let config = a3s_box_core::A3sConfig::load_default().map_err(box_error_to_status)?;
        let default_registry = config.registry.default_image_registry();
        let reference =
            ImageReference::parse_with_default_registry(&image_spec.image, &default_registry)
                .map_err(box_error_to_status)?;
        let auth = registry_auth_for_pull(req.auth.as_ref(), &reference.registry)?;
        let mut puller = ImagePuller::new(self.image_store.clone(), auth)
            .with_default_registry(&default_registry);

        if let Some(platform) = image_platform_annotation(&image_spec.annotations) {
            puller = puller
                .with_platform(platform)
                .map_err(box_error_to_status)?;
        }

        let full_reference = reference.full_reference();
        let _oci_image = puller
            .pull(&full_reference)
            .await
            .map_err(box_error_to_status)?;

        // Return the image reference as the image_ref
        Ok(Response::new(PullImageResponse {
            image_ref: full_reference,
        }))
    }

    async fn remove_image(
        &self,
        request: Request<RemoveImageRequest>,
    ) -> Result<Response<RemoveImageResponse>, Status> {
        let req = request.into_inner();
        let image_spec = req
            .image
            .ok_or_else(|| Status::invalid_argument("image spec required"))?;

        tracing::info!(image = %image_spec.image, "CRI RemoveImage");

        let stored = self
            .find_image(&image_spec.image)
            .await
            .ok_or_else(|| Status::not_found(format!("Image not found: {}", image_spec.image)))?;
        if let Some(container_id) = self.image_in_use(&image_spec.image, &stored).await {
            return Err(Status::failed_precondition(format!(
                "Image {} is used by container {}",
                image_spec.image, container_id
            )));
        }

        self.image_store
            .remove(&stored.reference)
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

fn registry_auth_for_pull(
    auth: Option<&AuthConfig>,
    registry: &str,
) -> Result<RegistryAuth, Status> {
    let Some(auth) = auth else {
        return Ok(RegistryAuth::from_credential_store(registry));
    };

    if !auth.username.is_empty() || !auth.password.is_empty() {
        if auth.username.is_empty() || auth.password.is_empty() {
            return Err(Status::invalid_argument(
                "CRI PullImage auth requires both username and password",
            ));
        }
        return Ok(RegistryAuth::basic(&auth.username, &auth.password));
    }

    if !auth.auth.is_empty() {
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(auth.auth.trim())
            .map_err(|e| Status::invalid_argument(format!("invalid CRI auth field: {e}")))?;
        let decoded = String::from_utf8(decoded)
            .map_err(|e| Status::invalid_argument(format!("invalid CRI auth UTF-8: {e}")))?;
        let Some((username, password)) = decoded.split_once(':') else {
            return Err(Status::invalid_argument(
                "invalid CRI auth field: expected base64(username:password)",
            ));
        };
        if username.is_empty() || password.is_empty() {
            return Err(Status::invalid_argument(
                "invalid CRI auth field: username and password are required",
            ));
        }
        return Ok(RegistryAuth::basic(username, password));
    }

    Ok(RegistryAuth::from_credential_store(registry))
}

fn image_platform_annotation(annotations: &HashMap<String, String>) -> Option<&str> {
    [
        "io.kubernetes.cri.image-platform",
        "io.kubernetes.cri.platform",
        "io.cri-containerd.image-platform",
        "a3s.io/platform",
    ]
    .iter()
    .find_map(|key| annotations.get(*key).map(String::as_str))
    .filter(|value| !value.trim().is_empty())
}

fn stored_image_matches(query: &str, stored: &StoredImage) -> bool {
    let stored_repo_digest = format!("{}@{}", stored.reference, stored.digest);

    if query == stored.reference
        || query == stored.digest
        || query == stored_repo_digest
        || query.starts_with("sha256:") && query == stored.digest
    {
        return true;
    }

    let Ok(query_ref) = ImageReference::parse(query) else {
        return false;
    };
    if query_ref.full_reference() == stored.reference {
        return true;
    }

    let Ok(stored_ref) = ImageReference::parse(&stored.reference) else {
        return false;
    };

    if let Some(query_digest) = query_ref.digest.as_deref() {
        stored.digest == query_digest
            && stored_ref.registry == query_ref.registry
            && stored_ref.repository == query_ref.repository
            && (query_ref.tag.is_none() || query_ref.tag == stored_ref.tag)
    } else {
        stored_ref.registry == query_ref.registry
            && stored_ref.repository == query_ref.repository
            && stored_ref.tag == query_ref.tag
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::container::{Container, ContainerState};
    use crate::state::NoopStateStore;
    use a3s_box_runtime::oci::ImageStore;

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

    fn make_test_service_with_cri_store(
        cri_store: Arc<PersistentCriStore>,
    ) -> (BoxImageService, tempfile::TempDir) {
        let (store, tmp) = make_test_store();
        let svc = BoxImageService::new(store, RegistryAuth::anonymous()).with_cri_store(cri_store);
        (svc, tmp)
    }

    fn test_container(id: &str, image_ref: &str) -> Container {
        Container {
            id: id.to_string(),
            sandbox_id: "sb-1".to_string(),
            name: format!("container-{}", id),
            image_ref: image_ref.to_string(),
            state: ContainerState::Created,
            created_at: 1_000_000_000,
            started_at: 0,
            finished_at: 0,
            exit_code: 0,
            labels: HashMap::new(),
            annotations: HashMap::new(),
            log_path: String::new(),
            command: vec!["true".to_string()],
            args: vec![],
            envs: vec![],
            working_dir: None,
            stdin: false,
            tty: false,
        }
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

    async fn put_test_image_with_config(store: &ImageStore, reference: &str, digest: &str) {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("blobs/sha256")).unwrap();
        std::fs::write(
            tmp.path().join("oci-layout"),
            r#"{"imageLayoutVersion":"1.0.0"}"#,
        )
        .unwrap();

        let config_content = r#"{
            "architecture": "amd64",
            "os": "linux",
            "config": {
                "Entrypoint": ["/bin/server"],
                "Cmd": ["--port", "8080"],
                "Env": ["PATH=/usr/bin:/bin", "APP_ENV=test"],
                "WorkingDir": "/srv/app",
                "Labels": {"org.opencontainers.image.title": "test-image"}
            },
            "rootfs": {"type": "layers", "diff_ids": ["sha256:layer1hash"]},
            "history": []
        }"#;
        let config_hash = "configabc123";
        std::fs::write(
            tmp.path().join("blobs/sha256").join(config_hash),
            config_content,
        )
        .unwrap();

        let layer_hash = "layerdef456";
        std::fs::write(tmp.path().join("blobs/sha256").join(layer_hash), b"layer").unwrap();

        let manifest_content = format!(
            r#"{{
                "schemaVersion": 2,
                "mediaType": "application/vnd.oci.image.manifest.v1+json",
                "config": {{
                    "mediaType": "application/vnd.oci.image.config.v1+json",
                    "digest": "sha256:{}",
                    "size": {}
                }},
                "layers": [{{
                    "mediaType": "application/vnd.oci.image.layer.v1.tar+gzip",
                    "digest": "sha256:{}",
                    "size": 5
                }}]
            }}"#,
            config_hash,
            config_content.len(),
            layer_hash
        );
        let manifest_hash = "manifestxyz789";
        std::fs::write(
            tmp.path().join("blobs/sha256").join(manifest_hash),
            &manifest_content,
        )
        .unwrap();

        let index_content = format!(
            r#"{{
                "schemaVersion": 2,
                "mediaType": "application/vnd.oci.image.index.v1+json",
                "manifests": [{{
                    "mediaType": "application/vnd.oci.image.manifest.v1+json",
                    "digest": "sha256:{}",
                    "size": {}
                }}]
            }}"#,
            manifest_hash,
            manifest_content.len()
        );
        std::fs::write(tmp.path().join("index.json"), index_content).unwrap();

        store.put(reference, digest, tmp.path()).await.unwrap();
    }

    // ── ListImages ───────────────────────────────────────────────────

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
    async fn test_list_images_filter_by_tag() {
        let (svc, _tmp) = make_test_service();
        put_test_image(&svc.image_store, "nginx:latest", "sha256:aaa111").await;
        put_test_image(&svc.image_store, "alpine:3.18", "sha256:bbb222").await;

        let resp = svc
            .list_images(Request::new(ListImagesRequest {
                filter: Some(ImageFilter {
                    image: Some(ImageSpec {
                        image: "nginx:latest".to_string(),
                        annotations: Default::default(),
                    }),
                }),
            }))
            .await
            .unwrap()
            .into_inner();

        assert_eq!(resp.images.len(), 1);
        assert_eq!(resp.images[0].repo_tags, vec!["nginx:latest"]);
    }

    #[tokio::test]
    async fn test_list_images_filter_by_digest() {
        let (svc, _tmp) = make_test_service();
        put_test_image(&svc.image_store, "nginx:latest", "sha256:aaa111").await;
        put_test_image(&svc.image_store, "alpine:3.18", "sha256:bbb222").await;

        let resp = svc
            .list_images(Request::new(ListImagesRequest {
                filter: Some(ImageFilter {
                    image: Some(ImageSpec {
                        image: "sha256:bbb222".to_string(),
                        annotations: Default::default(),
                    }),
                }),
            }))
            .await
            .unwrap()
            .into_inner();

        assert_eq!(resp.images.len(), 1);
        assert_eq!(resp.images[0].repo_tags, vec!["alpine:3.18"]);
    }

    #[tokio::test]
    async fn test_list_images_filter_by_repo_digest() {
        let (svc, _tmp) = make_test_service();
        put_test_image(&svc.image_store, "nginx:latest", "sha256:aaa111").await;
        put_test_image(&svc.image_store, "alpine:3.18", "sha256:bbb222").await;

        let resp = svc
            .list_images(Request::new(ListImagesRequest {
                filter: Some(ImageFilter {
                    image: Some(ImageSpec {
                        image: "library/nginx@sha256:aaa111".to_string(),
                        annotations: Default::default(),
                    }),
                }),
            }))
            .await
            .unwrap()
            .into_inner();

        assert_eq!(resp.images.len(), 1);
        assert_eq!(resp.images[0].repo_tags, vec!["nginx:latest"]);
    }

    #[tokio::test]
    async fn test_list_images_filter_by_repo_digest_requires_matching_repository() {
        let (svc, _tmp) = make_test_service();
        put_test_image(&svc.image_store, "nginx:latest", "sha256:aaa111").await;

        let resp = svc
            .list_images(Request::new(ListImagesRequest {
                filter: Some(ImageFilter {
                    image: Some(ImageSpec {
                        image: "library/redis@sha256:aaa111".to_string(),
                        annotations: Default::default(),
                    }),
                }),
            }))
            .await
            .unwrap()
            .into_inner();

        assert!(resp.images.is_empty());
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

    #[tokio::test]
    async fn test_image_status_found_by_digest() {
        let (svc, _tmp) = make_test_service();
        put_test_image(&svc.image_store, "nginx:latest", "sha256:aaa111").await;

        let resp = svc
            .image_status(Request::new(ImageStatusRequest {
                image: Some(ImageSpec {
                    image: "sha256:aaa111".to_string(),
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

    #[tokio::test]
    async fn test_image_status_found_by_repo_digest() {
        let (svc, _tmp) = make_test_service();
        put_test_image(&svc.image_store, "nginx:latest", "sha256:aaa111").await;

        let resp = svc
            .image_status(Request::new(ImageStatusRequest {
                image: Some(ImageSpec {
                    image: "nginx@sha256:aaa111".to_string(),
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

    #[tokio::test]
    async fn test_image_status_repo_digest_prefers_matching_reference() {
        let (svc, _tmp) = make_test_service();
        put_test_image(&svc.image_store, "nginx:latest", "sha256:aaa111").await;
        put_test_image(&svc.image_store, "alpine:3.18", "sha256:aaa111").await;

        let resp = svc
            .image_status(Request::new(ImageStatusRequest {
                image: Some(ImageSpec {
                    image: "alpine:3.18@sha256:aaa111".to_string(),
                    annotations: Default::default(),
                }),
                verbose: false,
            }))
            .await
            .unwrap()
            .into_inner();

        let image = resp.image.unwrap();
        assert_eq!(image.id, "sha256:aaa111");
        assert_eq!(image.repo_tags, vec!["alpine:3.18"]);
    }

    #[tokio::test]
    async fn test_image_status_verbose_includes_oci_config() {
        let (svc, _tmp) = make_test_service();
        put_test_image_with_config(&svc.image_store, "nginx:latest", "sha256:aaa111").await;

        let resp = svc
            .image_status(Request::new(ImageStatusRequest {
                image: Some(ImageSpec {
                    image: "nginx:latest".to_string(),
                    annotations: Default::default(),
                }),
                verbose: true,
            }))
            .await
            .unwrap()
            .into_inner();

        assert_eq!(resp.info.get("a3s.digest").unwrap(), "sha256:aaa111");
        assert_eq!(
            resp.info.get("a3s.entrypoint").unwrap(),
            r#"["/bin/server"]"#
        );
        assert_eq!(resp.info.get("a3s.cmd").unwrap(), r#"["--port","8080"]"#);
        assert_eq!(resp.info.get("a3s.working_dir").unwrap(), "/srv/app");
        assert!(resp.info.get("a3s.env").unwrap().contains("APP_ENV"));
        assert!(resp.info.get("a3s.labels").unwrap().contains("test-image"));
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

    #[tokio::test]
    async fn test_remove_image_by_digest_success() {
        let (svc, _tmp) = make_test_service();
        put_test_image(&svc.image_store, "nginx:latest", "sha256:aaa111").await;

        svc.remove_image(Request::new(RemoveImageRequest {
            image: Some(ImageSpec {
                image: "sha256:aaa111".to_string(),
                annotations: Default::default(),
            }),
        }))
        .await
        .unwrap();

        assert!(svc.image_store.get("nginx:latest").await.is_none());
    }

    #[tokio::test]
    async fn test_remove_image_repo_digest_removes_matching_reference() {
        let (svc, _tmp) = make_test_service();
        put_test_image(&svc.image_store, "nginx:latest", "sha256:aaa111").await;
        put_test_image(&svc.image_store, "alpine:3.18", "sha256:aaa111").await;

        svc.remove_image(Request::new(RemoveImageRequest {
            image: Some(ImageSpec {
                image: "alpine:3.18@sha256:aaa111".to_string(),
                annotations: Default::default(),
            }),
        }))
        .await
        .unwrap();

        assert!(svc.image_store.get("alpine:3.18").await.is_none());
        assert!(svc.image_store.get("nginx:latest").await.is_some());
    }

    #[tokio::test]
    async fn test_remove_image_rejects_container_reference() {
        let cri_store = Arc::new(PersistentCriStore::new(Arc::new(NoopStateStore)));
        cri_store
            .add_container(test_container("c-1", "nginx:latest"))
            .await;

        let (svc, _tmp) = make_test_service_with_cri_store(cri_store);
        put_test_image(&svc.image_store, "nginx:latest", "sha256:aaa111").await;

        let result = svc
            .remove_image(Request::new(RemoveImageRequest {
                image: Some(ImageSpec {
                    image: "nginx:latest".to_string(),
                    annotations: Default::default(),
                }),
            }))
            .await;

        assert!(result.is_err());
        assert_eq!(result.unwrap_err().code(), tonic::Code::FailedPrecondition);
        assert!(svc.image_store.get("nginx:latest").await.is_some());
    }

    #[tokio::test]
    async fn test_remove_image_by_digest_rejects_container_reference() {
        let cri_store = Arc::new(PersistentCriStore::new(Arc::new(NoopStateStore)));
        cri_store
            .add_container(test_container("c-1", "sha256:aaa111"))
            .await;

        let (svc, _tmp) = make_test_service_with_cri_store(cri_store);
        put_test_image(&svc.image_store, "nginx:latest", "sha256:aaa111").await;

        let result = svc
            .remove_image(Request::new(RemoveImageRequest {
                image: Some(ImageSpec {
                    image: "sha256:aaa111".to_string(),
                    annotations: Default::default(),
                }),
            }))
            .await;

        assert!(result.is_err());
        assert_eq!(result.unwrap_err().code(), tonic::Code::FailedPrecondition);
        assert!(svc.image_store.get("nginx:latest").await.is_some());
    }

    #[tokio::test]
    async fn test_remove_image_rejects_repo_digest_container_reference() {
        let cri_store = Arc::new(PersistentCriStore::new(Arc::new(NoopStateStore)));
        cri_store
            .add_container(test_container("c-1", "nginx@sha256:aaa111"))
            .await;

        let (svc, _tmp) = make_test_service_with_cri_store(cri_store);
        put_test_image(&svc.image_store, "nginx:latest", "sha256:aaa111").await;

        let result = svc
            .remove_image(Request::new(RemoveImageRequest {
                image: Some(ImageSpec {
                    image: "sha256:aaa111".to_string(),
                    annotations: Default::default(),
                }),
            }))
            .await;

        assert!(result.is_err());
        assert_eq!(result.unwrap_err().code(), tonic::Code::FailedPrecondition);
        assert!(svc.image_store.get("nginx:latest").await.is_some());
    }

    #[tokio::test]
    async fn test_remove_repo_digest_rejects_digest_container_reference() {
        let cri_store = Arc::new(PersistentCriStore::new(Arc::new(NoopStateStore)));
        cri_store
            .add_container(test_container("c-1", "sha256:aaa111"))
            .await;

        let (svc, _tmp) = make_test_service_with_cri_store(cri_store);
        put_test_image(&svc.image_store, "nginx:latest", "sha256:aaa111").await;

        let result = svc
            .remove_image(Request::new(RemoveImageRequest {
                image: Some(ImageSpec {
                    image: "nginx@sha256:aaa111".to_string(),
                    annotations: Default::default(),
                }),
            }))
            .await;

        assert!(result.is_err());
        assert_eq!(result.unwrap_err().code(), tonic::Code::FailedPrecondition);
        assert!(svc.image_store.get("nginx:latest").await.is_some());
    }

    #[test]
    fn test_registry_auth_for_pull_accepts_username_password() {
        let auth = AuthConfig {
            username: "alice".to_string(),
            password: "secret".to_string(),
            auth: String::new(),
            server_address: "registry.example.com".to_string(),
            identity_token: String::new(),
            registry_token: String::new(),
        };

        assert!(registry_auth_for_pull(Some(&auth), "registry.example.com").is_ok());
    }

    #[test]
    fn test_registry_auth_for_pull_accepts_encoded_auth() {
        let auth = AuthConfig {
            username: String::new(),
            password: String::new(),
            auth: "YWxpY2U6c2VjcmV0".to_string(),
            server_address: "registry.example.com".to_string(),
            identity_token: String::new(),
            registry_token: String::new(),
        };

        assert!(registry_auth_for_pull(Some(&auth), "registry.example.com").is_ok());
    }

    #[test]
    fn test_registry_auth_for_pull_rejects_partial_basic_auth() {
        let auth = AuthConfig {
            username: "alice".to_string(),
            password: String::new(),
            auth: String::new(),
            server_address: "registry.example.com".to_string(),
            identity_token: String::new(),
            registry_token: String::new(),
        };

        let err = registry_auth_for_pull(Some(&auth), "registry.example.com").unwrap_err();
        assert_eq!(err.code(), tonic::Code::InvalidArgument);
    }

    #[test]
    fn test_image_platform_annotation_prefers_cri_key() {
        let annotations = HashMap::from([
            ("a3s.io/platform".to_string(), "linux/amd64".to_string()),
            (
                "io.kubernetes.cri.image-platform".to_string(),
                "linux/arm64".to_string(),
            ),
        ]);

        assert_eq!(image_platform_annotation(&annotations), Some("linux/arm64"));
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
}
