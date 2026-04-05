//! BoxSdk — the main entry point for embedded sandbox management.

use std::path::PathBuf;

use a3s_box_core::config::{BoxConfig, ResourceConfig};
use a3s_box_core::error::{BoxError, Result};
use a3s_box_core::event::EventEmitter;
use a3s_box_core::network::NetworkMode;
use a3s_box_runtime::vmm::VmController;
use a3s_box_runtime::VmManager;

use crate::options::SandboxOptions;
use crate::sandbox::Sandbox;

/// The main SDK entry point for creating and managing sandboxes.
///
/// `BoxSdk` initializes shared resources (image cache, rootfs cache)
/// and provides a simple API for sandbox lifecycle management.
///
/// # Example
///
/// ```rust,no_run
/// use a3s_box_sdk::{BoxSdk, SandboxOptions};
///
/// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
/// let sdk = BoxSdk::new().await?;
/// let sandbox = sdk.create(SandboxOptions::default()).await?;
/// let result = sandbox.exec("echo", &["hello"]).await?;
/// sandbox.stop().await?;
/// # Ok(())
/// # }
/// ```
pub struct BoxSdk {
    /// Home directory for caches and state (~/.a3s).
    home_dir: PathBuf,
}

impl BoxSdk {
    /// Initialize the SDK with default settings.
    ///
    /// Sets up the home directory at `~/.a3s` and ensures
    /// cache directories exist.
    pub async fn new() -> Result<Self> {
        let home_dir = a3s_box_core::dirs_home();

        Self::init(home_dir).await
    }

    /// Initialize the SDK with a custom home directory.
    pub async fn with_home(home_dir: PathBuf) -> Result<Self> {
        Self::init(home_dir).await
    }

    async fn init(home_dir: PathBuf) -> Result<Self> {
        let dirs = ["bin", "images", "rootfs-cache", "sandboxes", "workspaces"];
        for dir in &dirs {
            let path = home_dir.join(dir);
            std::fs::create_dir_all(&path).map_err(|e| {
                BoxError::ConfigError(format!(
                    "Failed to create SDK directory {}: {}",
                    path.display(),
                    e
                ))
            })?;
        }

        tracing::info!(home = %home_dir.display(), "BoxSdk initialized");
        Ok(Self { home_dir })
    }

    /// Get the SDK home directory.
    pub fn home_dir(&self) -> &PathBuf {
        &self.home_dir
    }

    /// Create a new sandbox from the given options.
    ///
    /// This will:
    /// 1. Pull the OCI image (if not cached)
    /// 2. Build the rootfs
    /// 3. Boot a MicroVM
    /// 4. Wait for the guest agent to become healthy
    /// 5. Return a `Sandbox` handle for command execution
    pub async fn create(&self, options: SandboxOptions) -> Result<Sandbox> {
        let sandbox_id = uuid::Uuid::new_v4().to_string();
        let sandbox_name = options
            .name
            .clone()
            .unwrap_or_else(|| format!("sandbox-{}", &sandbox_id[..8]));

        tracing::info!(
            sandbox_id = %sandbox_id,
            name = %sandbox_name,
            image = %options.image,
            cpus = options.cpus,
            memory_mb = options.memory_mb,
            "Creating sandbox"
        );

        // Build BoxConfig from SandboxOptions
        let config = self.build_config(&options)?;

        // Create VM manager
        let event_emitter = EventEmitter::new(64);
        let mut vm = VmManager::with_box_id(config, event_emitter, sandbox_id.clone());

        // Ensure the shim binary is available and inject it as the VMM provider.
        // If embed-shim feature is enabled, this extracts the embedded binary to ~/.a3s/bin/.
        // Otherwise, falls back to VmController::find_shim() during boot().
        if let Some(shim_path) = crate::shim_embed::ensure_shim(&self.home_dir)? {
            let controller = VmController::new(shim_path)?;
            vm.set_provider(Box::new(controller));
        }

        // Boot the VM
        vm.boot().await?;

        // Get socket paths from the VM manager
        let exec_socket = vm
            .exec_socket_path()
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| {
                self.home_dir
                    .join("sandboxes")
                    .join(&sandbox_id)
                    .join("exec.sock")
            });
        let pty_socket = vm
            .pty_socket_path()
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| {
                self.home_dir
                    .join("sandboxes")
                    .join(&sandbox_id)
                    .join("pty.sock")
            });

        Ok(Sandbox::new(
            sandbox_id,
            sandbox_name,
            vm,
            exec_socket,
            pty_socket,
        ))
    }

    /// Build a BoxConfig from SandboxOptions.
    fn build_config(&self, options: &SandboxOptions) -> Result<BoxConfig> {
        let resources = ResourceConfig {
            vcpus: options.cpus,
            memory_mb: options.memory_mb,
            ..ResourceConfig::default()
        };

        let env: Vec<(String, String)> = options
            .env
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();

        let mut volumes: Vec<String> = options
            .mounts
            .iter()
            .map(|m| {
                if m.readonly {
                    format!("{}:{}:ro", m.host_path, m.guest_path)
                } else {
                    format!("{}:{}", m.host_path, m.guest_path)
                }
            })
            .collect();

        // Add persistent workspace as a volume mount
        if let Some(ref ws) = options.workspace {
            let ws_host_path = self.home_dir.join("workspaces").join(&ws.name);
            // Ensure workspace directory exists
            std::fs::create_dir_all(&ws_host_path).map_err(|e| {
                BoxError::ConfigError(format!(
                    "Failed to create workspace directory {}: {}",
                    ws_host_path.display(),
                    e
                ))
            })?;
            volumes.push(format!("{}:{}", ws_host_path.display(), ws.guest_path));
        }

        // Build port map from port forwards
        let port_map: Vec<String> = options
            .port_forwards
            .iter()
            .map(|pf| format!("{}:{}", pf.host_port, pf.guest_port))
            .collect();

        Ok(BoxConfig {
            image: options.image.clone(),
            resources,
            extra_env: env,
            volumes,
            port_map,
            cmd: options.workdir.as_ref().map(|_| vec![]).unwrap_or_default(),
            network: if options.network {
                NetworkMode::Tsi
            } else {
                NetworkMode::None
            },
            ..BoxConfig::default()
        })
    }
}

impl std::fmt::Debug for BoxSdk {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BoxSdk")
            .field("home_dir", &self.home_dir)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_sdk_init_with_custom_home() {
        let tmp = tempfile::TempDir::new().unwrap();
        let sdk = BoxSdk::with_home(tmp.path().to_path_buf()).await.unwrap();
        assert_eq!(sdk.home_dir(), tmp.path());
        assert!(tmp.path().join("images").exists());
        assert!(tmp.path().join("rootfs-cache").exists());
        assert!(tmp.path().join("sandboxes").exists());
        assert!(tmp.path().join("workspaces").exists());
    }

    #[tokio::test]
    async fn test_sdk_build_config_image() {
        let tmp = tempfile::TempDir::new().unwrap();
        let sdk = BoxSdk::with_home(tmp.path().to_path_buf()).await.unwrap();

        let options = SandboxOptions {
            image: "python:3.12-slim".into(),
            cpus: 4,
            memory_mb: 2048,
            ..Default::default()
        };

        let config = sdk.build_config(&options).unwrap();
        assert_eq!(config.image, "python:3.12-slim");
        assert_eq!(config.resources.vcpus, 4);
        assert_eq!(config.resources.memory_mb, 2048);
    }

    #[tokio::test]
    async fn test_sdk_build_config_env() {
        let tmp = tempfile::TempDir::new().unwrap();
        let sdk = BoxSdk::with_home(tmp.path().to_path_buf()).await.unwrap();

        let options = SandboxOptions {
            image: "alpine:latest".into(),
            env: [("KEY".into(), "val".into())].into(),
            ..Default::default()
        };

        let config = sdk.build_config(&options).unwrap();
        assert!(config.extra_env.contains(&("KEY".into(), "val".into())));
    }

    #[tokio::test]
    async fn test_sdk_build_config_mounts() {
        let tmp = tempfile::TempDir::new().unwrap();
        let sdk = BoxSdk::with_home(tmp.path().to_path_buf()).await.unwrap();

        let options = SandboxOptions {
            image: "alpine:latest".into(),
            mounts: vec![
                crate::options::MountSpec {
                    host_path: "/tmp/data".into(),
                    guest_path: "/data".into(),
                    readonly: false,
                },
                crate::options::MountSpec {
                    host_path: "/tmp/config".into(),
                    guest_path: "/config".into(),
                    readonly: true,
                },
            ],
            ..Default::default()
        };

        let config = sdk.build_config(&options).unwrap();
        assert_eq!(config.volumes.len(), 2);
        assert_eq!(config.volumes[0], "/tmp/data:/data");
        assert_eq!(config.volumes[1], "/tmp/config:/config:ro");
    }

    #[tokio::test]
    async fn test_sdk_build_config_defaults() {
        let tmp = tempfile::TempDir::new().unwrap();
        let sdk = BoxSdk::with_home(tmp.path().to_path_buf()).await.unwrap();

        let options = SandboxOptions::default();
        let config = sdk.build_config(&options).unwrap();
        assert_eq!(config.resources.vcpus, 1);
        assert_eq!(config.resources.memory_mb, 256);
        assert!(config.extra_env.is_empty());
        assert!(config.volumes.is_empty());
        assert!(config.port_map.is_empty());
    }

    #[tokio::test]
    async fn test_sdk_build_config_port_forwards() {
        let tmp = tempfile::TempDir::new().unwrap();
        let sdk = BoxSdk::with_home(tmp.path().to_path_buf()).await.unwrap();

        let options = SandboxOptions {
            image: "alpine:latest".into(),
            port_forwards: vec![
                crate::options::PortForward {
                    guest_port: 8080,
                    host_port: 3000,
                    protocol: "tcp".into(),
                },
                crate::options::PortForward {
                    guest_port: 5432,
                    host_port: 5432,
                    protocol: "tcp".into(),
                },
            ],
            ..Default::default()
        };

        let config = sdk.build_config(&options).unwrap();
        assert_eq!(config.port_map.len(), 2);
        assert_eq!(config.port_map[0], "3000:8080");
        assert_eq!(config.port_map[1], "5432:5432");
    }

    #[tokio::test]
    async fn test_sdk_build_config_workspace() {
        let tmp = tempfile::TempDir::new().unwrap();
        let sdk = BoxSdk::with_home(tmp.path().to_path_buf()).await.unwrap();

        let options = SandboxOptions {
            image: "alpine:latest".into(),
            workspace: Some(crate::options::WorkspaceConfig {
                name: "my-project".into(),
                guest_path: "/workspace".into(),
            }),
            ..Default::default()
        };

        let config = sdk.build_config(&options).unwrap();
        // Workspace should be added as a volume mount
        assert_eq!(config.volumes.len(), 1);
        assert!(config.volumes[0].contains("my-project"));
        assert!(config.volumes[0].ends_with(":/workspace"));
        // Workspace directory should be created
        assert!(tmp.path().join("workspaces").join("my-project").exists());
    }
}
