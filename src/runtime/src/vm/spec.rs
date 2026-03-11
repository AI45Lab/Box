//! Instance spec building — entrypoint resolution, volume mounts, OCI config.

use std::path::PathBuf;

use a3s_box_core::config::TeeConfig;
use a3s_box_core::error::{BoxError, Result};

use crate::oci::OciImageConfig;
use crate::rootfs::GUEST_WORKDIR;
use crate::vmm::{Entrypoint, FsMount, InstanceSpec};

use super::{fnv1a_hash, BoxLayout, VmManager};

const SBIN_INIT: &str = "/sbin/init";

impl VmManager {
    /// Build InstanceSpec from config and layout.
    pub(crate) fn build_instance_spec(&mut self, layout: &BoxLayout) -> Result<InstanceSpec> {
        // Build filesystem mounts
        let mut fs_mounts = vec![FsMount {
            tag: "workspace".to_string(),
            host_path: layout.workspace_path.clone(),
            read_only: false,
        }];

        // Add user-specified volume mounts (-v host:guest or -v host:guest:ro)
        for (i, vol) in self.config.volumes.iter().enumerate() {
            let mount = Self::parse_volume_mount(vol, i)?;
            fs_mounts.push(mount);
        }

        // Auto-create anonymous volumes for OCI VOLUME directives
        let user_guest_paths: std::collections::HashSet<String> = self
            .config
            .volumes
            .iter()
            .filter_map(|v| v.split(':').nth(1).map(String::from))
            .collect();
        let mut anon_vol_offset = self.config.volumes.len();

        if let Some(ref oci_config) = layout.oci_config {
            for vol_path in &oci_config.volumes {
                // Skip if the user already mounted something at this path
                if user_guest_paths.contains(vol_path) {
                    tracing::debug!(
                        path = vol_path,
                        "Skipping anonymous volume — user volume already covers this path"
                    );
                    continue;
                }

                // Generate a deterministic anonymous volume name
                let path_hash = &format!("{:x}", fnv1a_hash(vol_path))[..8];
                let short_box_id = &self.box_id[..8.min(self.box_id.len())];
                let anon_name = format!("anon_{}_{}", short_box_id, path_hash);

                // Create the volume via VolumeStore (best-effort)
                match self.create_anonymous_volume(&anon_name) {
                    Ok(host_path) => {
                        let tag = format!("vol{}", anon_vol_offset);
                        fs_mounts.push(FsMount {
                            tag: tag.clone(),
                            host_path: PathBuf::from(&host_path),
                            read_only: false,
                        });
                        self.anonymous_volumes.push(anon_name);
                        anon_vol_offset += 1;
                        tracing::info!(
                            volume = %tag,
                            guest_path = vol_path,
                            host_path = %host_path,
                            "Created anonymous volume for OCI VOLUME directive"
                        );
                    }
                    Err(e) => {
                        tracing::warn!(
                            path = vol_path,
                            error = %e,
                            "Failed to create anonymous volume, skipping"
                        );
                    }
                }
            }
        }

        // Determine whether guest init is installed (it becomes PID 1 and passes
        // BOX_EXEC_* env vars to the container entrypoint).
        let has_guest_init = layout.rootfs_path.join("sbin/init").exists();

        // Build entrypoint
        let mut entrypoint = if has_guest_init {
            // Guest init is PID 1. Pass container entrypoint/env via BOX_EXEC_* env vars.
            let (exec, args, container_env) = match &layout.oci_config {
                Some(oci_config) => {
                    let (exec, args) = Self::resolve_oci_entrypoint(
                        oci_config,
                        &self.config.cmd,
                        self.config.entrypoint_override.as_deref(),
                    );
                    (exec, args, oci_config.env.clone())
                }
                None => (SBIN_INIT.to_string(), vec![], vec![]),
            };

            // Pass exec + args as individual env vars (avoids spaces being truncated
            // by libkrun's env serialization).
            let mut env: Vec<(String, String)> = vec![
                ("BOX_EXEC_EXEC".to_string(), exec),
                ("BOX_EXEC_ARGC".to_string(), args.len().to_string()),
            ];
            for (i, arg) in args.iter().enumerate() {
                env.push((format!("BOX_EXEC_ARG_{}", i), arg.clone()));
            }

            // Pass the OCI working directory to guest init
            if let Some(ref oci_config) = layout.oci_config {
                if let Some(ref wd) = oci_config.working_dir {
                    env.push(("BOX_EXEC_WORKDIR".to_string(), wd.clone()));
                }
            }

            // Pass container environment variables with BOX_EXEC_ENV_ prefix
            for (key, value) in container_env {
                env.push((format!("BOX_EXEC_ENV_{}", key), value));
            }

            // Pass user volume mounts to guest init for mounting inside the VM.
            // Format: BOX_VOL_<index>=<tag>:<guest_path>[:ro]
            for (i, vol) in self.config.volumes.iter().enumerate() {
                let parts: Vec<&str> = vol.split(':').collect();
                if parts.len() >= 2 {
                    let guest_path = parts[1];
                    let mode = if parts.len() >= 3 && parts[2] == "ro" {
                        ":ro"
                    } else {
                        ""
                    };
                    env.push((
                        format!("BOX_VOL_{}", i),
                        format!("vol{}:{}{}", i, guest_path, mode),
                    ));
                }
            }

            // Pass anonymous volume mounts (from OCI VOLUME directives) to guest init
            if let Some(ref oci_config) = layout.oci_config {
                let mut anon_idx = self.config.volumes.len();
                for vol_path in &oci_config.volumes {
                    if user_guest_paths.contains(vol_path) {
                        continue;
                    }
                    env.push((
                        format!("BOX_VOL_{}", anon_idx),
                        format!("vol{}:{}", anon_idx, vol_path),
                    ));
                    anon_idx += 1;
                }
            }

            // Pass tmpfs mounts to guest init.
            // Format: BOX_TMPFS_<index>=<path>[:<options>]
            for (i, tmpfs_spec) in self.config.tmpfs.iter().enumerate() {
                env.push((format!("BOX_TMPFS_{}", i), tmpfs_spec.clone()));
            }

            // Pass security configuration to guest init
            let security_config = a3s_box_core::SecurityConfig::from_options(
                &self.config.security_opt,
                &self.config.cap_add,
                &self.config.cap_drop,
                self.config.privileged,
            );
            env.extend(security_config.to_env_vars());

            // Signal guest init to remount rootfs read-only after all setup
            if self.config.read_only {
                env.push(("BOX_READONLY".to_string(), "1".to_string()));
            }

            tracing::debug!(env = ?env, "Using guest init as PID 1");

            Entrypoint {
                executable: SBIN_INIT.to_string(),
                args: vec![],
                env,
            }
        } else {
            // No guest init — exec the container entrypoint directly as PID 1
            match &layout.oci_config {
                Some(oci_config) => {
                    let (executable, args) = Self::resolve_oci_entrypoint(
                        oci_config,
                        &self.config.cmd,
                        self.config.entrypoint_override.as_deref(),
                    );
                    let env = oci_config.env.clone();

                    tracing::debug!(
                        executable = %executable,
                        args = ?args,
                        env_count = env.len(),
                        workdir = ?oci_config.working_dir,
                        "Using OCI image entrypoint directly"
                    );

                    Entrypoint {
                        executable,
                        args,
                        env,
                    }
                }
                None => Entrypoint {
                    executable: SBIN_INIT.to_string(),
                    args: vec![],
                    env: vec![],
                },
            }
        };

        // Append user-specified environment variables (-e KEY=VALUE)
        if !self.config.extra_env.is_empty() {
            let mut env = entrypoint.env;
            for (key, value) in &self.config.extra_env {
                // Override existing keys or append new ones
                if let Some(existing) = env.iter_mut().find(|(k, _)| k == key) {
                    existing.1 = value.clone();
                } else {
                    env.push((key.clone(), value.clone()));
                }
            }
            entrypoint.env = env;
        }

        // Inject TEE simulation env var when simulate mode is enabled
        if matches!(self.config.tee, TeeConfig::SevSnp { simulate: true, .. })
            || matches!(self.config.tee, TeeConfig::Tdx { simulate: true, .. })
        {
            entrypoint
                .env
                .push(("A3S_TEE_SIMULATE".to_string(), "1".to_string()));
        }

        // Inject sidecar configuration so guest-init can launch the sidecar process
        if let Some(ref sidecar) = self.config.sidecar {
            entrypoint
                .env
                .push(("BOX_SIDECAR_IMAGE".to_string(), sidecar.image.clone()));
            entrypoint.env.push((
                "BOX_SIDECAR_VSOCK_PORT".to_string(),
                sidecar.vsock_port.to_string(),
            ));
            for (i, (key, value)) in sidecar.env.iter().enumerate() {
                entrypoint.env.push((
                    format!("BOX_SIDECAR_ENV_{}", i),
                    format!("{}={}", key, value),
                ));
            }
            entrypoint.env.push((
                "BOX_SIDECAR_ENV_COUNT".to_string(),
                sidecar.env.len().to_string(),
            ));
        }

        // Determine workdir
        let workdir = match &layout.oci_config {
            Some(oci_config) => oci_config
                .working_dir
                .clone()
                .unwrap_or_else(|| GUEST_WORKDIR.to_string()),
            None => GUEST_WORKDIR.to_string(),
        };

        // Extract user from OCI config (USER directive)
        let user = layout.oci_config.as_ref().and_then(|c| c.user.clone());

        Ok(InstanceSpec {
            box_id: self.box_id.clone(),
            vcpus: self.config.resources.vcpus as u8,
            memory_mib: self.config.resources.memory_mb,
            rootfs_path: layout.rootfs_path.clone(),
            exec_socket_path: layout.exec_socket_path.clone(),
            pty_socket_path: layout.pty_socket_path.clone(),
            attest_socket_path: layout.attest_socket_path.clone(),
            portfwd_socket_path: layout.portfwd_socket_path.clone(),
            fs_mounts,
            entrypoint,
            console_output: layout.console_output.clone(),
            workdir,
            tee_config: layout.tee_instance_config.clone(),
            port_map: self.config.port_map.clone(),
            user,
            network: None, // Network config is set by CLI when --network is specified
            resource_limits: self.config.resource_limits.clone(),
        })
    }

    /// Resolve the executable and args from an OCI image config.
    ///
    /// Follows Docker semantics:
    /// - If `entrypoint_override` is set, it replaces the OCI ENTRYPOINT
    /// - If ENTRYPOINT is set: executable = ENTRYPOINT[0], args = ENTRYPOINT[1:] + CMD
    /// - If only CMD is set: executable = CMD[0], args = CMD[1:]
    /// - If neither: fall back to `/sbin/init`
    /// - If `cmd_override` is non-empty, it replaces the OCI CMD
    ///
    /// Paths are used as-is since the OCI image is always extracted at rootfs root.
    fn resolve_oci_entrypoint(
        oci_config: &OciImageConfig,
        cmd_override: &[String],
        entrypoint_override: Option<&[String]>,
    ) -> (String, Vec<String>) {
        let oci_entrypoint = match entrypoint_override {
            Some(ep) => ep,
            None => oci_config.entrypoint.as_deref().unwrap_or(&[]),
        };
        let oci_cmd = if cmd_override.is_empty() {
            oci_config.cmd.as_deref().unwrap_or(&[])
        } else {
            cmd_override
        };

        if !oci_entrypoint.is_empty() {
            // ENTRYPOINT is set: use it as executable, CMD as additional args
            let exec = oci_entrypoint[0].clone();
            let mut args: Vec<String> = oci_entrypoint.iter().skip(1).cloned().collect();
            args.extend(oci_cmd.iter().cloned());
            (exec, args)
        } else if !oci_cmd.is_empty() {
            // Only CMD is set: use CMD[0] as executable, CMD[1:] as args
            let exec = oci_cmd[0].clone();
            let args: Vec<String> = oci_cmd.iter().skip(1).cloned().collect();
            (exec, args)
        } else {
            // Neither set: fall back to default init
            (SBIN_INIT.to_string(), vec![])
        }
    }

    /// Parse a volume mount string into an FsMount.
    ///
    /// Supported formats:
    /// - `host_path:guest_path` (read-write)
    /// - `host_path:guest_path:ro` (read-only)
    /// - `host_path:guest_path:rw` (read-write, explicit)
    fn parse_volume_mount(volume: &str, index: usize) -> Result<FsMount> {
        let parts: Vec<&str> = volume.split(':').collect();

        let (host_path_str, _guest_path, read_only) = match parts.len() {
            2 => (parts[0], parts[1], false),
            3 => {
                let ro = match parts[2] {
                    "ro" => true,
                    "rw" => false,
                    other => {
                        return Err(BoxError::ConfigError(format!(
                            "Invalid volume mode '{}' (expected 'ro' or 'rw'): {}",
                            other, volume
                        )));
                    }
                };
                (parts[0], parts[1], ro)
            }
            _ => {
                return Err(BoxError::ConfigError(format!(
                    "Invalid volume format (expected host:guest[:ro|rw]): {}",
                    volume
                )));
            }
        };

        // Resolve and validate host path
        let host_path = PathBuf::from(host_path_str);
        if !host_path.exists() {
            std::fs::create_dir_all(&host_path).map_err(|e| BoxError::BoxBootError {
                message: format!(
                    "Failed to create volume host directory {}: {}",
                    host_path.display(),
                    e
                ),
                hint: None,
            })?;
        }
        let host_path = host_path
            .canonicalize()
            .map_err(|e| BoxError::BoxBootError {
                message: format!(
                    "Failed to resolve volume path {}: {}",
                    host_path.display(),
                    e
                ),
                hint: None,
            })?;

        // Use a unique tag for each user volume
        let tag = format!("vol{}", index);

        tracing::info!(
            tag = %tag,
            host = %host_path.display(),
            guest = _guest_path,
            read_only,
            "Adding user volume mount"
        );

        Ok(FsMount {
            tag,
            host_path,
            read_only,
        })
    }

    /// Create an anonymous volume via VolumeStore.
    ///
    /// Returns the host path of the created volume.
    fn create_anonymous_volume(&self, name: &str) -> Result<String> {
        use crate::volume::VolumeStore;

        let store = VolumeStore::default_path()?;

        // If the volume already exists (e.g., from a previous run), reuse it
        if let Some(existing) = store.get(name)? {
            return Ok(existing.mount_point);
        }

        let mut config = a3s_box_core::volume::VolumeConfig::new(name, "");
        config
            .labels
            .insert("anonymous".to_string(), "true".to_string());
        config.attach(&self.box_id);
        let created = store.create(config)?;
        Ok(created.mount_point)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_parse_volume_mount_host_guest() {
        let temp = TempDir::new().unwrap();
        let host_path = temp.path().to_str().unwrap();
        let volume = format!("{}:/data", host_path);

        let mount = VmManager::parse_volume_mount(&volume, 0).unwrap();
        assert_eq!(mount.tag, "vol0");
        assert_eq!(mount.host_path, temp.path().canonicalize().unwrap());
        assert!(!mount.read_only);
    }

    #[test]
    fn test_parse_volume_mount_read_only() {
        let temp = TempDir::new().unwrap();
        let host_path = temp.path().to_str().unwrap();
        let volume = format!("{}:/data:ro", host_path);

        let mount = VmManager::parse_volume_mount(&volume, 1).unwrap();
        assert_eq!(mount.tag, "vol1");
        assert!(mount.read_only);
    }

    #[test]
    fn test_parse_volume_mount_explicit_rw() {
        let temp = TempDir::new().unwrap();
        let host_path = temp.path().to_str().unwrap();
        let volume = format!("{}:/data:rw", host_path);

        let mount = VmManager::parse_volume_mount(&volume, 2).unwrap();
        assert_eq!(mount.tag, "vol2");
        assert!(!mount.read_only);
    }

    #[test]
    fn test_parse_volume_mount_invalid_mode() {
        let temp = TempDir::new().unwrap();
        let host_path = temp.path().to_str().unwrap();
        let volume = format!("{}:/data:invalid", host_path);

        let result = VmManager::parse_volume_mount(&volume, 0);
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("Invalid volume mode"));
    }

    #[test]
    fn test_parse_volume_mount_invalid_format() {
        let result = VmManager::parse_volume_mount("invalid", 0);
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("Invalid volume format"));
    }

    #[test]
    fn test_parse_volume_mount_creates_missing_dir() {
        let temp = TempDir::new().unwrap();
        let host_path = temp.path().join("nonexistent");
        let volume = format!("{}:/data", host_path.display());

        assert!(!host_path.exists());
        let mount = VmManager::parse_volume_mount(&volume, 0).unwrap();
        assert!(host_path.exists());
        assert_eq!(mount.host_path, host_path.canonicalize().unwrap());
    }

    #[test]
    fn test_resolve_oci_entrypoint_with_entrypoint_and_cmd() {
        let config = OciImageConfig {
            entrypoint: Some(vec!["/bin/app".to_string()]),
            cmd: Some(vec!["--flag".to_string()]),
            env: vec![],
            working_dir: None,
            user: None,
            exposed_ports: vec![],
            labels: std::collections::HashMap::new(),
            volumes: vec![],
            stop_signal: None,
            health_check: None,
            onbuild: vec![],
        };

        let (exec, args) = VmManager::resolve_oci_entrypoint(&config, &[], None);
        assert_eq!(exec, "/bin/app");
        assert_eq!(args, vec!["--flag"]);
    }

    #[test]
    fn test_resolve_oci_entrypoint_cmd_only() {
        let config = OciImageConfig {
            entrypoint: None,
            cmd: Some(vec![
                "/bin/sh".to_string(),
                "-c".to_string(),
                "echo hi".to_string(),
            ]),
            env: vec![],
            working_dir: None,
            user: None,
            exposed_ports: vec![],
            labels: std::collections::HashMap::new(),
            volumes: vec![],
            stop_signal: None,
            health_check: None,
            onbuild: vec![],
        };

        let (exec, args) = VmManager::resolve_oci_entrypoint(&config, &[], None);
        assert_eq!(exec, "/bin/sh");
        assert_eq!(args, vec!["-c", "echo hi"]);
    }

    #[test]
    fn test_resolve_oci_entrypoint_neither() {
        let config = OciImageConfig {
            entrypoint: None,
            cmd: None,
            env: vec![],
            working_dir: None,
            user: None,
            exposed_ports: vec![],
            labels: std::collections::HashMap::new(),
            volumes: vec![],
            stop_signal: None,
            health_check: None,
            onbuild: vec![],
        };

        let (exec, _args) = VmManager::resolve_oci_entrypoint(&config, &[], None);
        assert_eq!(exec, "/sbin/init");
    }

    #[test]
    fn test_resolve_oci_entrypoint_cmd_override() {
        let config = OciImageConfig {
            entrypoint: None,
            cmd: Some(vec!["/bin/sh".to_string()]),
            env: vec![],
            working_dir: None,
            user: None,
            exposed_ports: vec![],
            labels: std::collections::HashMap::new(),
            volumes: vec![],
            stop_signal: None,
            health_check: None,
            onbuild: vec![],
        };

        let override_cmd = vec!["sleep".to_string(), "3600".to_string()];
        let (exec, args) = VmManager::resolve_oci_entrypoint(&config, &override_cmd, None);
        assert_eq!(exec, "sleep");
        assert_eq!(args, vec!["3600"]);
    }

    #[test]
    fn test_resolve_oci_entrypoint_with_override() {
        let config = OciImageConfig {
            entrypoint: Some(vec!["/bin/app".to_string()]),
            cmd: Some(vec!["--flag".to_string()]),
            env: vec![],
            working_dir: None,
            user: None,
            exposed_ports: vec![],
            labels: std::collections::HashMap::new(),
            volumes: vec![],
            stop_signal: None,
            health_check: None,
            onbuild: vec![],
        };

        // Override replaces the image entrypoint entirely
        let override_ep = vec!["/bin/sh".to_string(), "-c".to_string()];
        let (exec, args) = VmManager::resolve_oci_entrypoint(&config, &[], Some(&override_ep));
        assert_eq!(exec, "/bin/sh");
        // args = entrypoint[1:] + cmd
        assert_eq!(args, vec!["-c", "--flag"]);
    }

    #[test]
    fn test_resolve_oci_entrypoint_override_with_cmd_override() {
        let config = OciImageConfig {
            entrypoint: Some(vec!["/bin/app".to_string()]),
            cmd: Some(vec!["--flag".to_string()]),
            env: vec![],
            working_dir: None,
            user: None,
            exposed_ports: vec![],
            labels: std::collections::HashMap::new(),
            volumes: vec![],
            stop_signal: None,
            health_check: None,
            onbuild: vec![],
        };

        // Both entrypoint and cmd overridden
        let override_ep = vec!["/bin/sh".to_string()];
        let cmd_override = vec!["echo".to_string(), "hello".to_string()];
        let (exec, args) =
            VmManager::resolve_oci_entrypoint(&config, &cmd_override, Some(&override_ep));
        assert_eq!(exec, "/bin/sh");
        assert_eq!(args, vec!["echo", "hello"]);
    }
}
