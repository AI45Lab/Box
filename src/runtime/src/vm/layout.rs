//! Layout preparation — rootfs building, caching, TEE config, binary discovery.

use std::path::{Path, PathBuf};

use crate::cache::RootfsCache;
use crate::oci::OciRootfsBuilder;
use crate::vmm::TeeInstanceConfig;
use a3s_box_core::config::TeeConfig;
use a3s_box_core::error::{BoxError, Result};

use super::{BoxLayout, VmManager};

impl VmManager {
    pub(crate) async fn prepare_layout(&self) -> Result<BoxLayout> {
        // Create box-specific directories
        let box_dir = self.home_dir.join("boxes").join(&self.box_id);
        let socket_dir = box_dir.join("sockets");
        let logs_dir = box_dir.join("logs");

        std::fs::create_dir_all(&socket_dir).map_err(|e| BoxError::BoxBootError {
            message: format!("Failed to create socket directory: {}", e),
            hint: None,
        })?;

        std::fs::create_dir_all(&logs_dir).map_err(|e| BoxError::BoxBootError {
            message: format!("Failed to create logs directory: {}", e),
            hint: None,
        })?;

        // Resolve workspace path: empty config means use a per-box directory so the
        // host CWD is never accidentally exposed to the guest.
        let workspace_path = if self.config.workspace.as_os_str().is_empty() {
            box_dir.join("workspace")
        } else {
            PathBuf::from(&self.config.workspace)
        };
        if !workspace_path.exists() {
            std::fs::create_dir_all(&workspace_path).map_err(|e| BoxError::BoxBootError {
                message: format!("Failed to create workspace directory: {}", e),
                hint: None,
            })?;
        }
        // Canonicalize to absolute path (libkrun requires absolute paths for virtiofs)
        let workspace_path = workspace_path
            .canonicalize()
            .map_err(|e| BoxError::BoxBootError {
                message: format!(
                    "Failed to resolve workspace path {}: {}",
                    workspace_path.display(),
                    e
                ),
                hint: None,
            })?;

        // Pull OCI image from registry and extract at rootfs root.
        // Extracting at root preserves absolute symlinks and dynamic linker paths.
        let reference = &self.config.image;
        let images_dir = self.home_dir.join("images");
        let store = crate::oci::ImageStore::new(&images_dir, crate::DEFAULT_IMAGE_CACHE_SIZE)?;
        let mut puller = crate::oci::ImagePuller::new(
            std::sync::Arc::new(store),
            crate::oci::RegistryAuth::from_env(),
        );
        if let Some(ref m) = self.prom {
            puller = puller.set_metrics(m.clone());
        }
        if let Some(ref f) = self.pull_progress_fn {
            puller = puller.with_progress_fn(f.clone());
        }

        tracing::info!(reference = %reference, "Pulling OCI image from registry");

        let oci_image = puller.pull(reference).await?;

        let image_path = oci_image.root_dir().to_path_buf();

        // Try rootfs cache first — on hit, use the rootfs provider (overlay or copy)
        let cache_key = RootfsCache::compute_key(reference, &[], &[], &[]);
        let (rootfs_path, oci_config) =
            if let Some(cached_path) = self.try_rootfs_cache_path(&cache_key)? {
                tracing::info!(
                    cache_key = %&cache_key[..12],
                    reference = %reference,
                    provider = self.rootfs_provider.name(),
                    "Rootfs cache hit"
                );
                if let Some(ref prom) = self.prom {
                    prom.rootfs_cache_hits.inc();
                }
                let rootfs_path = self.rootfs_provider.prepare(&box_dir, &cached_path)?;

                #[cfg(target_os = "windows")]
                if let Ok(guest_init_path) = Self::find_guest_init() {
                    tracing::info!(
                        guest_init = %guest_init_path.display(),
                        "Refreshing guest init on cached Windows rootfs"
                    );
                    OciRootfsBuilder::new(&rootfs_path)
                        .with_guest_init(guest_init_path)
                        .install_guest_init_only()?;
                }

                let builder = OciRootfsBuilder::new(&rootfs_path).with_image(&image_path);
                (rootfs_path, Some(builder.image_config()?))
            } else {
                tracing::info!(
                    image = %image_path.display(),
                    "Building rootfs from pulled OCI image (cache miss)"
                );
                if let Some(ref prom) = self.prom {
                    prom.rootfs_cache_misses.inc();
                }

                let rootfs_path = box_dir.join("rootfs");
                let mut builder = OciRootfsBuilder::new(&rootfs_path).with_image(&image_path);

                // Install guest init if available (runs as PID 1, mounts virtiofs shares,
                // then execs the container entrypoint)
                if let Ok(guest_init_path) = Self::find_guest_init() {
                    tracing::info!(
                        guest_init = %guest_init_path.display(),
                        "Installing guest init"
                    );
                    builder = builder.with_guest_init(guest_init_path);
                } else {
                    tracing::warn!(
                        "Guest init binary not found; container entrypoint will run as PID 1"
                    );
                }

                builder.build()?;
                let config = builder.image_config()?;

                // Store in cache for next time
                self.store_rootfs_cache(&cache_key, &rootfs_path, reference);

                (rootfs_path, Some(config))
            };

        // Generate TEE configuration if enabled
        let tee_instance_config = self.generate_tee_config(&box_dir)?;

        Ok(BoxLayout {
            rootfs_path,
            exec_socket_path: socket_dir.join("exec.sock"),
            pty_socket_path: socket_dir.join("pty.sock"),
            attest_socket_path: socket_dir.join("attest.sock"),
            workspace_path,
            console_output: Some(logs_dir.join("console.log")),
            oci_config,
            tee_instance_config,
        })
    }

    /// Try to get a cached rootfs and copy it to the target path.
    ///
    /// Returns `Some(target_path)` if cache hit, `None` if cache miss.
    /// If caching is disabled in config, always returns `None`.
    #[cfg(test)]
    pub(crate) fn try_rootfs_cache(
        &self,
        cache_key: &str,
        target_path: &Path,
    ) -> Result<Option<PathBuf>> {
        if !self.config.cache.enabled {
            return Ok(None);
        }

        let cache_dir = self.resolve_cache_dir().join("rootfs");
        let cache = match RootfsCache::new(&cache_dir) {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(error = %e, "Failed to open rootfs cache, skipping");
                return Ok(None);
            }
        };

        match cache.get(cache_key)? {
            Some(cached_path) => {
                // Copy cached rootfs to target
                crate::cache::layer_cache::copy_dir_recursive(&cached_path, target_path)?;
                Ok(Some(target_path.to_path_buf()))
            }
            None => Ok(None),
        }
    }

    /// Try to get the cached rootfs path without copying.
    ///
    /// Returns `Some(cached_path)` if cache hit, `None` if cache miss.
    /// The caller is responsible for preparing the rootfs via `RootfsProvider`.
    pub(crate) fn try_rootfs_cache_path(&self, cache_key: &str) -> Result<Option<PathBuf>> {
        if !self.config.cache.enabled {
            return Ok(None);
        }

        let cache_dir = self.resolve_cache_dir().join("rootfs");
        let cache = match RootfsCache::new(&cache_dir) {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(error = %e, "Failed to open rootfs cache, skipping");
                return Ok(None);
            }
        };

        cache.get(cache_key)
    }

    /// Store a built rootfs in the cache for future reuse.
    ///
    /// Errors are logged but not propagated — caching is best-effort.
    pub(crate) fn store_rootfs_cache(
        &self,
        cache_key: &str,
        rootfs_path: &Path,
        description: &str,
    ) {
        if !self.config.cache.enabled {
            return;
        }

        let cache_dir = self.resolve_cache_dir().join("rootfs");
        let cache = match RootfsCache::new(&cache_dir) {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(error = %e, "Failed to open rootfs cache for storing");
                return;
            }
        };

        match cache.put(cache_key, rootfs_path, description) {
            Ok(_) => {
                tracing::debug!(
                    cache_key = %&cache_key[..cache_key.len().min(12)],
                    description = %description,
                    "Stored rootfs in cache"
                );
                // Prune if needed
                if let Err(e) = cache.prune(
                    self.config.cache.max_rootfs_entries,
                    self.config.cache.max_cache_bytes,
                ) {
                    tracing::warn!(error = %e, "Failed to prune rootfs cache");
                }
            }
            Err(e) => {
                tracing::warn!(error = %e, "Failed to store rootfs in cache");
            }
        }
    }

    /// Resolve the cache directory from config or default.
    pub(crate) fn resolve_cache_dir(&self) -> PathBuf {
        self.config
            .cache
            .cache_dir
            .clone()
            .unwrap_or_else(|| self.home_dir.join("cache"))
    }

    /// Generate TEE configuration file if TEE is enabled.
    pub(crate) fn generate_tee_config(&self, box_dir: &Path) -> Result<Option<TeeInstanceConfig>> {
        match &self.config.tee {
            TeeConfig::None => Ok(None),
            TeeConfig::SevSnp {
                workload_id,
                generation,
                simulate,
            } => {
                // In simulation mode, skip hardware check and TEE config
                // (the guest will generate simulated reports via A3S_TEE_SIMULATE env)
                if *simulate {
                    tracing::warn!("TEE simulation mode: skipping hardware check and TEE config");
                    return Ok(None);
                }

                // Verify hardware support
                crate::tee::require_sev_snp_support()?;

                // Generate TEE config JSON
                let config = serde_json::json!({
                    "workload_id": workload_id,
                    "cpus": self.config.resources.vcpus,
                    "ram_mib": self.config.resources.memory_mb,
                    "tee": "snp",
                    "tee_data": format!(r#"{{"gen":"{}"}}"#, generation.as_str()),
                    "attestation_url": ""
                });

                let config_path = box_dir.join("tee-config.json");
                std::fs::write(&config_path, serde_json::to_string_pretty(&config)?).map_err(
                    |e| {
                        BoxError::TeeConfig(format!(
                            "Failed to write TEE config to {}: {}",
                            config_path.display(),
                            e
                        ))
                    },
                )?;

                tracing::info!(
                    workload_id = %workload_id,
                    generation = %generation.as_str(),
                    config_path = %config_path.display(),
                    "Generated TEE configuration"
                );

                Ok(Some(TeeInstanceConfig {
                    config_path,
                    tee_type: "snp".to_string(),
                }))
            }
            TeeConfig::Tdx {
                workload_id,
                simulate,
            } => {
                if *simulate {
                    tracing::warn!("TDX simulation mode: skipping hardware check and TEE config");
                    return Ok(None);
                }

                // Intel TDX runtime support is not yet implemented.
                // The config variant exists for forward compatibility, but we
                // cannot boot a TDX VM today.
                Err(BoxError::TeeConfig(format!(
                    "Intel TDX is not yet supported at runtime (workload_id='{}'). \
                     Use tee=sev-snp or tee=none.",
                    workload_id
                )))
            }
        }
    }

    /// Find the guest init binary in common locations.
    ///
    /// Searches in order:
    /// 1. Same directory as current executable
    /// 2. target/debug or target/release (for development)
    /// 3. PATH
    ///
    /// The binary must be a Linux ELF executable since it runs inside the VM.
    pub(crate) fn find_guest_init() -> Result<PathBuf> {
        let mut candidates = Self::find_binary_candidates("a3s-box-guest-init");

        #[cfg(target_os = "windows")]
        {
            candidates.sort_by_key(|path| {
                let path_str = path.to_string_lossy();
                if path_str.contains("x86_64-unknown-linux-musl")
                    || path_str.contains("aarch64-unknown-linux-musl")
                {
                    0
                } else {
                    1
                }
            });
        }

        for path in candidates {
            if Self::is_linux_elf(&path) {
                return Ok(path);
            }
            tracing::debug!(
                path = %path.display(),
                "Skipping guest init (not a Linux ELF binary)"
            );
        }

        Err(BoxError::BoxBootError {
            message: "Linux guest init binary not found".to_string(),
            hint: Some(
                "Cross-compile with: cargo build -p a3s-box-guest-init --target aarch64-unknown-linux-musl"
                    .to_string(),
            ),
        })
    }

    /// Search common locations for a binary by name.
    fn find_binary_candidates(name: &str) -> Vec<PathBuf> {
        let mut candidates = Vec::new();

        // Try same directory as current executable
        if let Ok(exe_path) = std::env::current_exe() {
            if let Some(exe_dir) = exe_path.parent() {
                let path = exe_dir.join(name);
                if path.exists() {
                    candidates.push(path);
                }

                // Also search cross-compilation directories relative to the
                // exe's target root. When the exe is at target/debug/a3s-box,
                // cross-compiled guest binaries live at
                // target/aarch64-unknown-linux-musl/{debug,release}/.
                if let Some(target_root) = exe_dir.parent() {
                    let cross_dirs = [
                        "aarch64-unknown-linux-musl/debug",
                        "aarch64-unknown-linux-musl/release",
                        "x86_64-unknown-linux-musl/debug",
                        "x86_64-unknown-linux-musl/release",
                    ];
                    for dir in &cross_dirs {
                        let path = target_root.join(dir).join(name);
                        if path.exists() {
                            candidates.push(path);
                        }
                    }
                }
            }
        }

        // Try cross-compilation target directories relative to CWD (for development)
        let target_dirs = [
            "target/aarch64-unknown-linux-musl/debug",
            "target/aarch64-unknown-linux-musl/release",
            "target/x86_64-unknown-linux-musl/debug",
            "target/x86_64-unknown-linux-musl/release",
            "target/debug",
            "target/release",
        ];
        for dir in &target_dirs {
            let path = PathBuf::from(dir).join(name);
            if path.exists() {
                candidates.push(path);
            }
        }

        // Try PATH
        let home_bin = a3s_box_core::dirs_home().join("bin").join(name);
        if home_bin.exists() {
            candidates.push(home_bin);
        }

        // Try PATH
        if let Ok(path_var) = std::env::var("PATH") {
            for dir in std::env::split_paths(&path_var) {
                let path = dir.join(name);
                if path.exists() {
                    candidates.push(path);
                }
            }
        }

        candidates
    }

    /// Check if a file is a Linux ELF binary by reading its magic bytes.
    fn is_linux_elf(path: &std::path::Path) -> bool {
        let Ok(file) = std::fs::File::open(path) else {
            return false;
        };
        use std::io::Read;
        let mut header = [0u8; 18];
        let Ok(_) = (&file).read_exact(&mut header) else {
            return false;
        };
        // ELF magic: 0x7f 'E' 'L' 'F'
        if header[0..4] != [0x7f, b'E', b'L', b'F'] {
            return false;
        }
        // EI_OSABI (byte 7): 0x00 = ELFOSABI_NONE (System V / Linux)
        // or 0x03 = ELFOSABI_LINUX
        matches!(header[7], 0x00 | 0x03)
    }
}

#[cfg(test)]
mod tests {
    use super::super::BoxState;
    use super::*;
    use crate::cache::RootfsCache;
    use a3s_box_core::config::BoxConfig;
    use std::sync::Arc;
    use tempfile::TempDir;
    use tokio::sync::RwLock;

    fn make_vm_manager_with_home(home_dir: &Path) -> VmManager {
        use a3s_box_core::event::EventEmitter;
        let config = BoxConfig::default();
        let emitter = EventEmitter::new(10);
        VmManager {
            config,
            box_id: "test-box".to_string(),
            state: Arc::new(RwLock::new(BoxState::Created)),
            event_emitter: emitter,
            provider: None,
            handler: Arc::new(RwLock::new(None)),
            exec_client: None,
            net_manager: None,
            home_dir: home_dir.to_path_buf(),
            anonymous_volumes: Vec::new(),
            tee: None,
            rootfs_provider: crate::rootfs::default_provider(),
            exec_socket_path: None,
            pty_socket_path: None,
            prom: None,
            shim_exit_code: None,
            pull_progress_fn: None,
        }
    }

    #[test]
    fn test_resolve_cache_dir_default() {
        let tmp = TempDir::new().unwrap();
        let vm = make_vm_manager_with_home(tmp.path());

        let cache_dir = vm.resolve_cache_dir();
        assert_eq!(cache_dir, tmp.path().join("cache"));
    }

    #[test]
    fn test_resolve_cache_dir_custom() {
        let tmp = TempDir::new().unwrap();
        let mut vm = make_vm_manager_with_home(tmp.path());
        vm.config.cache.cache_dir = Some(PathBuf::from("/custom/cache"));

        let cache_dir = vm.resolve_cache_dir();
        assert_eq!(cache_dir, PathBuf::from("/custom/cache"));
    }

    #[test]
    fn test_try_rootfs_cache_disabled() {
        let tmp = TempDir::new().unwrap();
        let mut vm = make_vm_manager_with_home(tmp.path());
        vm.config.cache.enabled = false;

        let target = tmp.path().join("target");
        let result = vm.try_rootfs_cache("some_key", &target).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn test_try_rootfs_cache_miss() {
        let tmp = TempDir::new().unwrap();
        let vm = make_vm_manager_with_home(tmp.path());

        let target = tmp.path().join("target");
        let result = vm.try_rootfs_cache("nonexistent_key", &target).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn test_try_rootfs_cache_hit() {
        let tmp = TempDir::new().unwrap();
        let vm = make_vm_manager_with_home(tmp.path());

        // Pre-populate the cache
        let cache_dir = tmp.path().join("cache").join("rootfs");
        let cache = RootfsCache::new(&cache_dir).unwrap();
        let source = tmp.path().join("source_rootfs");
        std::fs::create_dir_all(&source).unwrap();
        std::fs::write(source.join("agent.bin"), "binary").unwrap();
        cache.put("test_key", &source, "test").unwrap();

        // Now try_rootfs_cache should hit
        let target = tmp.path().join("target_rootfs");
        let result = vm.try_rootfs_cache("test_key", &target).unwrap();
        assert!(result.is_some());
        assert_eq!(result.unwrap(), target);
        assert!(target.join("agent.bin").is_file());
        assert_eq!(
            std::fs::read_to_string(target.join("agent.bin")).unwrap(),
            "binary"
        );
    }

    #[test]
    fn test_store_rootfs_cache_disabled() {
        let tmp = TempDir::new().unwrap();
        let mut vm = make_vm_manager_with_home(tmp.path());
        vm.config.cache.enabled = false;

        let source = tmp.path().join("rootfs");
        std::fs::create_dir_all(&source).unwrap();
        std::fs::write(source.join("f.txt"), "data").unwrap();

        // Should not store anything
        vm.store_rootfs_cache("key", &source, "test");

        // Cache directory should not even be created
        let cache_dir = tmp.path().join("cache").join("rootfs");
        assert!(!cache_dir.exists());
    }

    #[test]
    fn test_store_rootfs_cache_success() {
        let tmp = TempDir::new().unwrap();
        let vm = make_vm_manager_with_home(tmp.path());

        let source = tmp.path().join("rootfs");
        std::fs::create_dir_all(&source).unwrap();
        std::fs::write(source.join("agent.bin"), "binary").unwrap();

        vm.store_rootfs_cache("store_key", &source, "test image");

        // Verify it was stored
        let cache_dir = tmp.path().join("cache").join("rootfs");
        let cache = RootfsCache::new(&cache_dir).unwrap();
        let result = cache.get("store_key").unwrap();
        assert!(result.is_some());
    }

    #[test]
    fn test_store_rootfs_cache_prunes_on_store() {
        let tmp = TempDir::new().unwrap();
        let mut vm = make_vm_manager_with_home(tmp.path());
        vm.config.cache.max_rootfs_entries = 2;

        let source = tmp.path().join("rootfs");
        std::fs::create_dir_all(&source).unwrap();
        std::fs::write(source.join("f.txt"), "data").unwrap();

        // Store 3 entries (exceeds max_rootfs_entries=2)
        for i in 0..3 {
            vm.store_rootfs_cache(&format!("key{}", i), &source, &format!("entry {}", i));
            std::thread::sleep(std::time::Duration::from_millis(10));
        }

        // After pruning, should have at most 2 entries
        let cache_dir = tmp.path().join("cache").join("rootfs");
        let cache = RootfsCache::new(&cache_dir).unwrap();
        assert!(cache.entry_count().unwrap() <= 2);
    }

    #[tokio::test]
    async fn test_exec_command_rejects_created_state() {
        let tmp = TempDir::new().unwrap();
        let vm = make_vm_manager_with_home(tmp.path());

        let result = vm.exec_command(vec!["echo".to_string()], 0).await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.to_string().contains("not yet booted"));
    }

    #[tokio::test]
    async fn test_exec_command_rejects_stopped_state() {
        let tmp = TempDir::new().unwrap();
        let vm = make_vm_manager_with_home(tmp.path());
        *vm.state.write().await = BoxState::Stopped;

        let result = vm.exec_command(vec!["echo".to_string()], 0).await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.to_string().contains("stopped"));
    }

    #[tokio::test]
    async fn test_exec_command_no_client() {
        let tmp = TempDir::new().unwrap();
        let vm = make_vm_manager_with_home(tmp.path());
        *vm.state.write().await = BoxState::Ready;

        let result = vm.exec_command(vec!["echo".to_string()], 0).await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.to_string().contains("not connected"));
    }

    #[test]
    fn test_try_and_store_roundtrip() {
        let tmp = TempDir::new().unwrap();
        let vm = make_vm_manager_with_home(tmp.path());

        // First call: cache miss
        let target1 = tmp.path().join("target1");
        let result = vm.try_rootfs_cache("roundtrip_key", &target1).unwrap();
        assert!(result.is_none());

        // Build rootfs manually
        let built_rootfs = tmp.path().join("built");
        std::fs::create_dir_all(&built_rootfs).unwrap();
        std::fs::write(built_rootfs.join("init"), "init_binary").unwrap();
        std::fs::create_dir_all(built_rootfs.join("etc")).unwrap();
        std::fs::write(built_rootfs.join("etc/config"), "config_data").unwrap();

        // Store in cache
        vm.store_rootfs_cache("roundtrip_key", &built_rootfs, "roundtrip test");

        // Second call: cache hit
        let target2 = tmp.path().join("target2");
        let result = vm.try_rootfs_cache("roundtrip_key", &target2).unwrap();
        assert!(result.is_some());
        assert!(target2.join("init").is_file());
        assert_eq!(
            std::fs::read_to_string(target2.join("init")).unwrap(),
            "init_binary"
        );
        assert_eq!(
            std::fs::read_to_string(target2.join("etc/config")).unwrap(),
            "config_data"
        );
    }
}
