//! Tests for the build engine.

#[cfg(test)]
#[allow(clippy::module_inception)]
mod tests {
    use super::super::utils::*;
    use super::super::{
        build, default_target_platform, scratch_config, validate_build_config, BuildConfig,
    };
    use crate::oci::{ImageStore, OciImage};
    use a3s_box_core::platform::Platform;
    use std::collections::HashMap;
    use std::path::PathBuf;
    use std::sync::Arc;

    #[test]
    fn test_resolve_path_absolute() {
        assert_eq!(resolve_path("/app", "/usr/bin"), "/usr/bin");
    }

    #[test]
    fn test_resolve_path_relative() {
        assert_eq!(resolve_path("/app", "src"), "/app/src");
    }

    #[test]
    fn test_resolve_path_root_workdir() {
        assert_eq!(resolve_path("/", "app"), "/app");
    }

    #[test]
    fn test_expand_args_braces() {
        let mut args = HashMap::new();
        args.insert("VERSION".to_string(), "3.19".to_string());
        assert_eq!(expand_args("alpine:${VERSION}", &args), "alpine:3.19");
    }

    #[test]
    fn test_expand_args_dollar() {
        let mut args = HashMap::new();
        args.insert("TAG".to_string(), "latest".to_string());
        assert_eq!(expand_args("image:$TAG", &args), "image:latest");
    }

    #[test]
    fn test_expand_args_no_match() {
        let args = HashMap::new();
        assert_eq!(expand_args("alpine:3.19", &args), "alpine:3.19");
    }

    #[test]
    fn test_format_size() {
        assert_eq!(format_size(500), "500 B");
        assert_eq!(format_size(1536), "1.5 KB");
        assert_eq!(format_size(1_500_000), "1.4 MB");
        assert_eq!(format_size(1_500_000_000), "1.4 GB");
    }

    fn test_build_config(platforms: Vec<Platform>) -> BuildConfig {
        BuildConfig {
            context_dir: PathBuf::from("/tmp/context"),
            dockerfile_path: PathBuf::from("/tmp/context/Dockerfile"),
            tag: Some("test:latest".to_string()),
            build_args: HashMap::new(),
            quiet: true,
            platforms,
            target: None,
            no_cache: false,
            metrics: None,
        }
    }

    #[test]
    fn test_validate_build_config_rejects_multi_platform() {
        let config = test_build_config(vec![Platform::linux_amd64(), Platform::linux_arm64()]);
        let err = validate_build_config(&config).unwrap_err().to_string();
        assert!(err.contains("Multi-platform builds are not implemented yet"));
    }

    #[test]
    fn test_validate_build_config_rejects_non_linux_platform() {
        let config = test_build_config(vec![Platform::new("windows", "amd64")]);
        let err = validate_build_config(&config).unwrap_err().to_string();
        assert!(err.contains("Only linux target platforms"));
    }

    #[test]
    fn test_default_target_platform_is_linux() {
        let platform = default_target_platform();
        assert_eq!(platform.os, "linux");
        assert!(!platform.architecture.is_empty());
    }

    #[test]
    fn test_scratch_config_is_empty_base() {
        let config = scratch_config();
        assert!(config.entrypoint.is_none());
        assert!(config.cmd.is_none());
        assert!(config.env.is_empty());
        assert!(config.volumes.is_empty());
    }

    #[tokio::test]
    async fn test_build_from_scratch_copy_metadata_without_network() {
        let tmp = tempfile::TempDir::new().unwrap();
        let context = tmp.path().join("context");
        let store_dir = tmp.path().join("images");
        std::fs::create_dir_all(&context).unwrap();
        std::fs::write(context.join("hello.txt"), "hello").unwrap();
        std::fs::write(
            context.join("Dockerfile"),
            r#"FROM scratch
COPY hello.txt /hello.txt
CMD ["cat", "/hello.txt"]
LABEL org.opencontainers.image.title="scratch-smoke"
"#,
        )
        .unwrap();

        let store = Arc::new(ImageStore::new(&store_dir, 1024 * 1024 * 100).unwrap());
        let result = build(
            BuildConfig {
                context_dir: context.clone(),
                dockerfile_path: context.join("Dockerfile"),
                tag: Some("scratch-smoke:latest".to_string()),
                build_args: HashMap::new(),
                quiet: true,
                platforms: vec![],
                target: None,
                no_cache: false,
                metrics: None,
            },
            store.clone(),
        )
        .await
        .unwrap();

        assert_eq!(result.reference, "scratch-smoke:latest");
        assert_eq!(result.layer_count, 1);

        let stored = store.get("scratch-smoke:latest").await.unwrap();
        let image = OciImage::from_path(&stored.path).unwrap();
        assert_eq!(
            image.config().cmd,
            Some(vec!["cat".to_string(), "/hello.txt".to_string()])
        );
        assert_eq!(
            image.label("org.opencontainers.image.title"),
            Some("scratch-smoke")
        );
    }

    /// Regression: a multi-stage `COPY --from=<stage> /abs/path` must resolve
    /// the absolute source inside the source stage's rootfs. Previously
    /// `context_dir.join("/abs")` discarded the base (Path::join semantics) and
    /// looked at the host root, so multi-stage copies failed with "source not
    /// found".
    /// `--target <stage>` builds only up to the named stage and emits that
    /// stage's image (not the final stage), and never runs later stages.
    #[tokio::test]
    async fn test_build_target_stage() {
        let tmp = tempfile::TempDir::new().unwrap();
        let context = tmp.path().join("context");
        let store_dir = tmp.path().join("images");
        std::fs::create_dir_all(&context).unwrap();
        std::fs::write(context.join("a.txt"), "a").unwrap();
        std::fs::write(context.join("b.txt"), "b").unwrap();
        std::fs::write(
            context.join("Dockerfile"),
            "FROM scratch AS builder\nCOPY a.txt /a.txt\nCMD [\"builder\"]\n\nFROM scratch\nCOPY b.txt /b.txt\nCMD [\"final\"]\n",
        )
        .unwrap();

        let store = Arc::new(ImageStore::new(&store_dir, 1024 * 1024 * 100).unwrap());
        let result = build(
            BuildConfig {
                context_dir: context.clone(),
                dockerfile_path: context.join("Dockerfile"),
                tag: Some("targeted:latest".to_string()),
                build_args: HashMap::new(),
                quiet: true,
                platforms: vec![],
                target: Some("builder".to_string()),
                no_cache: false,
                metrics: None,
            },
            store.clone(),
        )
        .await
        .unwrap();

        // The output image is the `builder` stage: CMD ["builder"], and its
        // single layer contains a.txt (NOT b.txt from the final stage).
        let stored = store.get("targeted:latest").await.unwrap();
        let image = OciImage::from_path(&stored.path).unwrap();
        assert_eq!(image.config().cmd, Some(vec!["builder".to_string()]));
        assert_eq!(result.layer_count, 1);

        // An unknown --target is a clear error.
        let err = build(
            BuildConfig {
                context_dir: context.clone(),
                dockerfile_path: context.join("Dockerfile"),
                tag: Some("x:latest".to_string()),
                build_args: HashMap::new(),
                quiet: true,
                platforms: vec![],
                target: Some("nope".to_string()),
                no_cache: false,
                metrics: None,
            },
            store.clone(),
        )
        .await
        .unwrap_err()
        .to_string();
        assert!(err.contains("target build stage 'nope' not found"));
    }

    /// `.dockerignore` must keep ignored context paths (secrets, `.git`,
    /// `node_modules`) out of `COPY .`, with `!` negation re-including.
    #[tokio::test]
    async fn test_build_honors_dockerignore() {
        let tmp = tempfile::TempDir::new().unwrap();
        let context = tmp.path().join("context");
        let store_dir = tmp.path().join("images");
        std::fs::create_dir_all(context.join(".git")).unwrap();
        std::fs::create_dir_all(context.join("logs")).unwrap();
        std::fs::write(context.join(".env"), "SECRET").unwrap();
        std::fs::write(context.join(".git/config"), "g").unwrap();
        std::fs::write(context.join("keep.txt"), "keep").unwrap();
        std::fs::write(context.join("logs/a.log"), "x").unwrap();
        std::fs::write(context.join("logs/important.log"), "y").unwrap();
        std::fs::write(
            context.join(".dockerignore"),
            ".git\n.env\n**/*.log\n!logs/important.log\n",
        )
        .unwrap();
        std::fs::write(context.join("Dockerfile"), "FROM scratch\nCOPY . /app\n").unwrap();

        let store = Arc::new(ImageStore::new(&store_dir, 1024 * 1024 * 100).unwrap());
        build(
            BuildConfig {
                context_dir: context.clone(),
                dockerfile_path: context.join("Dockerfile"),
                tag: Some("di:latest".to_string()),
                build_args: HashMap::new(),
                quiet: true,
                platforms: vec![],
                target: None,
                no_cache: false,
                metrics: None,
            },
            store.clone(),
        )
        .await
        .unwrap();

        let stored = store.get("di:latest").await.unwrap();
        // Read the single layer and collect file paths.
        let image = OciImage::from_path(&stored.path).unwrap();
        let layer = &image.layer_paths()[0];
        let file = std::fs::File::open(layer).unwrap();
        let dec = flate2::read::GzDecoder::new(file);
        let mut ar = tar::Archive::new(dec);
        let names: Vec<String> = ar
            .entries()
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.header().entry_type().is_file())
            .map(|e| e.path().unwrap().to_string_lossy().to_string())
            .collect();

        assert!(names.iter().any(|n| n == "app/keep.txt"));
        assert!(names.iter().any(|n| n == "app/logs/important.log")); // !negation
        assert!(
            !names.iter().any(|n| n.contains(".env")),
            "secret leaked: {names:?}"
        );
        assert!(
            !names.iter().any(|n| n.contains(".git")),
            ".git leaked: {names:?}"
        );
        assert!(
            !names.iter().any(|n| n == "app/logs/a.log"),
            "*.log leaked: {names:?}"
        );
    }

    #[tokio::test]
    async fn test_build_multistage_copy_from_absolute_source() {
        let tmp = tempfile::TempDir::new().unwrap();
        let context = tmp.path().join("context");
        let store_dir = tmp.path().join("images");
        std::fs::create_dir_all(&context).unwrap();
        std::fs::write(context.join("run.sh"), "built-artifact").unwrap();
        std::fs::write(
            context.join("Dockerfile"),
            r#"FROM scratch AS builder
COPY run.sh /run.sh

FROM scratch
COPY --from=builder /run.sh /work/run.sh
CMD ["/work/run.sh"]
"#,
        )
        .unwrap();

        let store = Arc::new(ImageStore::new(&store_dir, 1024 * 1024 * 100).unwrap());
        let result = build(
            BuildConfig {
                context_dir: context.clone(),
                dockerfile_path: context.join("Dockerfile"),
                tag: Some("multistage:latest".to_string()),
                build_args: HashMap::new(),
                quiet: true,
                platforms: vec![],
                target: None,
                no_cache: false,
                metrics: None,
            },
            store.clone(),
        )
        .await
        .expect("multi-stage COPY --from with an absolute source must build");

        // Only the final stage's single layer is in the output image.
        assert_eq!(result.layer_count, 1);
        let stored = store.get("multistage:latest").await.unwrap();
        let image = OciImage::from_path(&stored.path).unwrap();
        assert_eq!(image.config().cmd, Some(vec!["/work/run.sh".to_string()]));
    }

    #[tokio::test]
    async fn test_add_url_invalid_host_returns_error() {
        // Verify that ADD <url> with an unreachable host returns a BuildError,
        // not a silent skip. Uses a guaranteed-invalid host.
        use super::super::handlers::handle_add;
        use tempfile::TempDir;

        let tmp = TempDir::new().unwrap();
        let rootfs = tmp.path().join("rootfs");
        let layers = tmp.path().join("layers");
        std::fs::create_dir_all(&rootfs).unwrap();
        std::fs::create_dir_all(&layers).unwrap();

        let result = tokio::task::spawn_blocking(move || {
            handle_add(
                &["http://this-host-does-not-exist.invalid/file.txt".to_string()],
                "/tmp/file.txt",
                None,
                tmp.path(),
                &rootfs,
                &layers,
                "/",
                0,
                None,
            )
        })
        .await
        .unwrap();

        assert!(result.is_err(), "Expected error for unreachable URL");
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("ADD URL download failed"),
            "Expected ADD URL error, got: {msg}"
        );
    }
}
