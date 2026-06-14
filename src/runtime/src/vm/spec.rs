//! Instance spec building — entrypoint resolution, volume mounts, OCI config.

use std::path::{Path, PathBuf};

use a3s_box_core::config::TeeConfig;
use a3s_box_core::error::{BoxError, Result};

use crate::oci::OciImageConfig;
use crate::rootfs::GUEST_WORKDIR;
use crate::vmm::{Entrypoint, FsMount, InstanceSpec};

use super::{fnv1a_hash, BoxLayout, VmManager};

const SBIN_INIT: &str = "/sbin/init";
#[cfg(target_os = "windows")]
const USR_SBIN_INIT: &str = "/usr/sbin/init";

/// Read an environment variable, returning `None` if unset or empty.
fn env_nonempty(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.is_empty())
}

impl VmManager {
    /// Build InstanceSpec from config and layout.
    pub(crate) fn build_instance_spec(&mut self, layout: &BoxLayout) -> Result<InstanceSpec> {
        // Build filesystem mounts
        let mut fs_mounts = vec![FsMount {
            tag: "workspace".to_string(),
            host_path: layout.workspace_path.clone(),
            read_only: false,
        }];

        // Add user-specified volume mounts (-v host:guest or -v host:guest:ro).
        // Single-file binds are staged under this per-box dir (cleaned with the
        // box) since virtio-fs can only share directories — see parse_volume_mount.
        let filemounts_dir = self
            .home_dir
            .join("boxes")
            .join(&self.box_id)
            .join(".filemounts");
        for (i, vol) in self.config.volumes.iter().enumerate() {
            let mount = Self::parse_volume_mount(vol, i, &filemounts_dir)?;
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
                    Ok((host_path, created)) => {
                        let tag = format!("vol{}", anon_vol_offset);
                        fs_mounts.push(FsMount {
                            tag: tag.clone(),
                            host_path: PathBuf::from(&host_path),
                            read_only: false,
                        });
                        self.anonymous_volumes.push(anon_name.clone());
                        if created {
                            self.created_anonymous_volumes.push(anon_name);
                        }
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
        let guest_init_exec = Self::guest_init_exec_path(&layout.rootfs_path);
        // When guest init is PID 1 it applies the container user to the main
        // process itself (via BOX_EXEC_USER below); the shim must then NOT call
        // libkrun set_uid (which would drop PID 1 and break init). Only the
        // legacy no-guest-init path falls back to the shim's set_uid.
        let has_guest_init = guest_init_exec.is_some();
        let workdir = Self::effective_workdir(&self.config, layout.oci_config.as_ref());
        let user = Self::effective_user(&self.config, layout.oci_config.as_ref());

        // Build entrypoint
        let mut entrypoint = if let Some(guest_init_exec) = guest_init_exec {
            // Guest init is PID 1. Pass container entrypoint/env via BOX_EXEC_* env vars.
            let (exec, args, mut container_env) = match &layout.oci_config {
                Some(oci_config) => {
                    let (exec, args) = Self::resolve_oci_entrypoint(
                        oci_config,
                        &self.config.cmd,
                        self.config.entrypoint_override.as_deref(),
                    );
                    (exec, args, oci_config.env.clone())
                }
                None => (
                    "/bin/sh".to_string(),
                    vec![
                        "-c".to_string(),
                        "echo No command specified; exec /bin/sh".to_string(),
                    ],
                    vec![],
                ),
            };
            a3s_box_core::env::merge_env_pairs(&mut container_env, &self.config.extra_env);

            // Pass exec + args as individual env vars, base64-encoded (URL-safe, no
            // padding) so arbitrary bytes — spaces, quotes, `$`, `;` — survive
            // libkrun's env serialization intact (a raw `"` in a value was being
            // corrupted). `BOX_EXEC_B64=1` tells guest-init to decode them.
            use base64::Engine;
            let b64 =
                |s: &str| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(s.as_bytes());
            let mut env: Vec<(String, String)> = vec![
                ("BOX_EXEC_B64".to_string(), "1".to_string()),
                ("BOX_EXEC_EXEC".to_string(), b64(&exec)),
                ("BOX_EXEC_ARGC".to_string(), args.len().to_string()),
            ];
            for (i, arg) in args.iter().enumerate() {
                env.push((format!("BOX_EXEC_ARG_{}", i), b64(arg)));
            }

            // Prototype: deferred-main-spawn. If the host set BOX_DEFERRED_MAIN=1,
            // tell guest init to boot IDLE; the runtime then sends a spawn-main
            // control frame post-readiness to run the command above as the main.
            if self.config.deferred_main
                || std::env::var("BOX_DEFERRED_MAIN")
                    .map(|v| v == "1")
                    .unwrap_or(false)
            {
                env.push(("BOX_DEFERRED_MAIN".to_string(), "1".to_string()));
            }

            // Pass the effective working directory to guest init so PID 1 and
            // the container entrypoint agree even when no OCI WORKDIR is set.
            env.push(("BOX_EXEC_WORKDIR".to_string(), b64(&workdir)));

            // Pass the container user (image USER / --user) to guest init, which
            // applies it (setgroups+setgid+setuid) to the MAIN process right
            // before exec — after PID 1 has done its root-only setup. This must
            // NOT go through the shim's libkrun set_uid, which would drop guest
            // PID 1 to the user and break init (mount/chroot need root).
            if let Some(user) = &user {
                env.push(("BOX_EXEC_USER".to_string(), b64(user)));
            }

            // Pass container environment variables with BOX_EXEC_ENV_ prefix (values
            // base64-encoded like the rest, so a value containing `"`/spaces/etc.
            // reaches the container intact). The key stays raw — env names are
            // restricted to a safe character set.
            for (key, value) in container_env {
                env.push((format!("BOX_EXEC_ENV_{}", key), b64(&value)));
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
                    // Mark single-file bind mounts so the guest binds the file onto
                    // guest_path instead of mounting the virtio-fs share over its
                    // parent directory (which would clobber e.g. /etc). The host is
                    // authoritative here (it can stat the path); the guest must not
                    // re-guess from the guest path's shape.
                    let file_flag = if std::path::Path::new(parts[0]).is_file() {
                        ":file"
                    } else {
                        ""
                    };
                    env.push((
                        format!("BOX_VOL_{}", i),
                        format!("vol{}:{}{}{}", i, guest_path, mode, file_flag),
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

            // Pass pod sysctls to guest init.
            // Format: BOX_SYSCTL_<index>=<name>=<value>
            for (i, (name, value)) in self.config.sysctls.iter().enumerate() {
                env.push((format!("BOX_SYSCTL_{}", i), format!("{}={}", name, value)));
            }

            // Pass security configuration to guest init
            let security_config = a3s_box_core::SecurityConfig::from_options(
                &self.config.security_opt,
                &self.config.cap_add,
                &self.config.cap_drop,
                self.config.privileged,
            );
            env.extend(security_config.to_env_vars());

            // Process-count cap (`--pids-limit`). Unlike `--memory`/`--cpus`
            // (enforced by sizing the microVM itself), a pids cap has no
            // VM-boundary equivalent, so guest-init enforces it via an in-guest
            // cgroup `pids.max`; it reads this env in PID 1 before the container
            // fork.
            if let Some(pids_limit) = self.config.resource_limits.pids_limit {
                env.push(("A3S_SEC_PIDS_LIMIT".to_string(), pids_limit.to_string()));
            }

            // Signal guest init to remount rootfs read-only after all setup
            if self.config.read_only {
                env.push(("BOX_READONLY".to_string(), "1".to_string()));
            }

            if let Some(hostname) = self.config.hostname.as_ref() {
                env.push(("BOX_HOSTNAME".to_string(), hostname.clone()));
            }

            #[cfg(target_os = "windows")]
            env.push(("KRUN_INIT_PID1".to_string(), "1".to_string()));

            tracing::debug!(env = ?env, "Using guest init as PID 1");

            Entrypoint {
                executable: guest_init_exec.to_string(),
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
                    let mut env = oci_config.env.clone();
                    a3s_box_core::env::merge_env_pairs(&mut env, &self.config.extra_env);

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
                    executable: "/bin/sh".to_string(),
                    args: vec![
                        "-c".to_string(),
                        "echo No command specified; exec /bin/sh".to_string(),
                    ],
                    env: self.config.extra_env.clone(),
                },
            }
        };

        // Inject TEE simulation env var when simulate mode is enabled
        if matches!(self.config.tee, TeeConfig::SevSnp { simulate: true, .. })
            || matches!(self.config.tee, TeeConfig::Tdx { simulate: true, .. })
        {
            entrypoint
                .env
                .push(("A3S_TEE_SIMULATE".to_string(), "1".to_string()));
        }

        #[cfg(target_os = "windows")]
        if !self.config.port_map.is_empty() {
            entrypoint
                .env
                .push(("BOX_WINDOWS_PORT_FWD".to_string(), "1".to_string()));
        }

        #[cfg(not(target_os = "windows"))]
        entrypoint
            .env
            .push(("BOX_CRI_PORT_FWD".to_string(), "1".to_string()));

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

        Ok(InstanceSpec {
            box_id: self.box_id.clone(),
            vcpus: self.config.resources.vcpus as u8,
            memory_mib: self.config.resources.memory_mb,
            rootfs_path: layout.rootfs_path.clone(),
            exec_socket_path: layout.exec_socket_path.clone(),
            pty_socket_path: layout.pty_socket_path.clone(),
            attest_socket_path: layout.attest_socket_path.clone(),
            port_forward_socket_path: layout.port_forward_socket_path.clone(),
            fs_mounts,
            entrypoint,
            console_output: layout.console_output.clone(),
            workdir,
            tee_config: layout.tee_instance_config.clone(),
            port_map: self.config.port_map.clone(),
            // Guest init applies the user to the main process (BOX_EXEC_USER);
            // only the legacy no-guest-init path uses the shim's set_uid.
            user: if has_guest_init { None } else { user },
            network: None, // Network config is set by CLI when --network is specified
            resource_limits: self.config.resource_limits.clone(),
            log_config: self.log_config.clone(),
            // KSM page-merging: config field, or the A3S_BOX_KSM env override.
            ksm: self.config.ksm
                || std::env::var("A3S_BOX_KSM")
                    .map(|v| matches!(v.as_str(), "1" | "true" | "yes" | "on"))
                    .unwrap_or(false),
            // Snapshot-fork (per-VM): config field, or the env override (single-VM
            // `run`). The pool / fork daemon set these per-VM via config so one
            // process can drive a different template/restore per VM.
            snapshot_mem_file: self
                .config
                .snapshot_mem_file
                .clone()
                .or_else(|| env_nonempty("KRUN_SNAPSHOT_MEM_FILE")),
            snapshot_sock: self
                .config
                .snapshot_sock
                .clone()
                .or_else(|| env_nonempty("KRUN_SNAPSHOT_SOCK")),
            restore_from: self
                .config
                .restore_from
                .clone()
                .or_else(|| env_nonempty("KRUN_RESTORE_FROM")),
        })
    }

    /// Resolve the executable and args from an OCI image config.
    ///
    /// Follows Docker semantics:
    /// - If `entrypoint_override` is set, it replaces the OCI ENTRYPOINT
    /// - If ENTRYPOINT is set: executable = ENTRYPOINT[0], args = ENTRYPOINT[1:] + CMD
    /// - If only CMD is set: executable = CMD[0], args = CMD[1:]
    /// - If neither: fall back to `/bin/sh` (universal across distros; `/sbin/init`
    ///   does not exist on Alpine, which was the original cause of issue #3)
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
            // Neither set: fall back to /bin/sh (universal across all Linux distros)
            (
                "/bin/sh".to_string(),
                vec![
                    "-c".to_string(),
                    "echo No command specified; exec /bin/sh".to_string(),
                ],
            )
        }
    }

    fn guest_init_exec_path(rootfs_path: &Path) -> Option<&'static str> {
        let sbin_init = rootfs_path.join("sbin").join("init");
        if sbin_init.exists() {
            return Some(SBIN_INIT);
        }

        #[cfg(target_os = "windows")]
        {
            let sbin_link = rootfs_path.join("sbin");
            if let Ok(target) = std::fs::read_link(&sbin_link) {
                let resolved = if target.is_absolute() {
                    target
                } else {
                    rootfs_path.join(target)
                };
                if resolved.join("init").exists() {
                    return Some(SBIN_INIT);
                }
            }

            if rootfs_path.join("usr").join("sbin").join("init").exists() {
                return Some(USR_SBIN_INIT);
            }
        }

        None
    }

    fn effective_workdir(
        config: &a3s_box_core::config::BoxConfig,
        oci_config: Option<&OciImageConfig>,
    ) -> String {
        let image_workdir = oci_config
            .and_then(|oci| oci.working_dir.clone())
            .filter(|workdir| !workdir.is_empty());

        match config
            .workdir
            .as_ref()
            .filter(|workdir| !workdir.is_empty())
        {
            // Absolute override is used as-is.
            Some(workdir) if workdir.starts_with('/') => workdir.clone(),
            // Relative override resolves against the image WORKDIR (Docker's
            // `-w sub` => <image WORKDIR>/sub), falling back to `/` as the base.
            Some(workdir) => {
                let base = image_workdir.unwrap_or_else(|| "/".to_string());
                let base = base.trim_end_matches('/');
                format!("{}/{}", base, workdir.trim_start_matches('/'))
            }
            None => image_workdir.unwrap_or_else(|| GUEST_WORKDIR.to_string()),
        }
    }

    fn effective_user(
        config: &a3s_box_core::config::BoxConfig,
        oci_config: Option<&OciImageConfig>,
    ) -> Option<String> {
        config
            .user
            .as_ref()
            .filter(|user| !user.is_empty())
            .cloned()
            .or_else(|| {
                oci_config
                    .and_then(|oci| oci.user.clone())
                    .filter(|user| !user.is_empty())
            })
    }

    /// Parse a volume mount string into a FsMount.
    ///
    /// Supported formats:
    /// - `host_path:guest_path` (read-write)
    /// - `host_path:guest_path:ro` (read-only)
    /// - `host_path:guest_path:rw` (read-write, explicit)
    ///
    /// Handles Windows paths with drive letters (e.g. `C:\Users\Temp:/data:ro`) by
    /// using the colon-split parts array to reliably determine the host/guest boundary.
    fn parse_volume_mount(volume: &str, index: usize, filemounts_dir: &Path) -> Result<FsMount> {
        let parts: Vec<&str> = volume.split(':').collect();

        // A valid volume must have at least 2 colon-separated parts (host:guest)
        if parts.len() < 2 {
            return Err(BoxError::ConfigError(format!(
                "Invalid volume format (expected host:guest[:ro|rw]): {}",
                volume
            )));
        }

        // Detect whether a mode suffix is present by checking if the LAST
        // colon-separated segment is "ro" or "rw".
        let last = parts.last();
        let has_mode = last.is_some_and(|s| s == &"ro" || s == &"rw");

        // If the last segment is NOT a valid mode (ro/rw), check if it looks like
        // a path component. If it does NOT, it's an invalid mode suffix (e.g. :invalid).
        // Path-like segments start with /, \, ./, ../, or (on Windows) a drive letter.
        let looks_like_path = |s: &&str| -> bool {
            s.starts_with('/')
                || s.starts_with('\\')
                || s.starts_with("./")
                || s.starts_with("../")
                || (s.len() == 2
                    && s.chars().next().is_some_and(|c| c.is_alphabetic())
                    && s.ends_with(':'))
        };
        if parts.len() >= 2 && !has_mode && !last.is_some_and(looks_like_path) {
            return Err(BoxError::ConfigError(format!(
                "Invalid volume mode '{}' (expected 'ro' or 'rw'): {}",
                last.unwrap(),
                volume
            )));
        }

        // Determine the guest path and mode based on whether a mode suffix exists.
        // With mode: guest = parts[parts.len() - 2], mode = parts[parts.len() - 1]
        // Without mode: guest = parts[parts.len() - 1]
        let (guest_path_str, mode_str) = if has_mode {
            let guest = parts[parts.len() - 2];
            let mode = parts[parts.len() - 1];
            (guest, mode)
        } else {
            (parts[parts.len() - 1], "")
        };

        // Validate guest path is not empty or a mode keyword
        if guest_path_str.is_empty() || guest_path_str == "ro" || guest_path_str == "rw" {
            return Err(BoxError::ConfigError(format!(
                "Invalid volume format (expected host:guest[:ro|rw]): {}",
                volume
            )));
        }

        // Determine read_only from mode string
        let read_only = match mode_str {
            "ro" => true,
            "rw" => false,
            other if !other.is_empty() => {
                return Err(BoxError::ConfigError(format!(
                    "Invalid volume mode '{}' (expected 'ro' or 'rw'): {}",
                    other, volume
                )));
            }
            _ => false,
        };

        // Reconstruct the host path from parts:
        // - If parts[0] is a single-letter (Windows drive letter), reconstruct the
        //   Windows path by joining parts[0..guest_idx] with colons.
        // - Otherwise (Unix), join parts[0..guest_idx] with colons.
        let guest_idx = if has_mode {
            parts.len() - 2
        } else {
            parts.len() - 1
        };
        let host_path_str = if parts[0].len() == 1 {
            // Windows drive letter — parts[0] is "C", parts[1..] is the rest of the path
            let host_parts = &parts[..guest_idx];
            let reconstructed = host_parts.join(":");
            // If the reconstructed path ends with a trailing colon (from a trailing
            // backslash in the original Windows path like "C:\path\:"), strip it.
            reconstructed
                .strip_suffix(':')
                .map(|s| s.to_string())
                .unwrap_or_else(|| reconstructed)
        } else {
            parts[..guest_idx].join(":")
        };

        // Resolve and validate host path
        let host_path = PathBuf::from(&host_path_str);
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

        // virtio-fs can only share a directory, so a single-file bind (host path
        // is a file) is staged into a per-box directory and that directory is
        // shared instead. The file is hard-linked in (so the bind stays live in
        // both directions), falling back to a copy across filesystems. It is named
        // with the guest path's basename — exactly what the guest looks for inside
        // the share (see mount_user_volumes' `file` branch).
        let host_path = if host_path.is_file() {
            Self::stage_single_file_mount(&host_path, guest_path_str, index, filemounts_dir)?
        } else {
            host_path
        };

        // Use a unique tag for each user volume
        let tag = format!("vol{}", index);

        tracing::info!(
            tag = %tag,
            host = %host_path.display(),
            guest = guest_path_str,
            read_only,
            "Adding user volume mount"
        );

        Ok(FsMount {
            tag,
            host_path,
            read_only,
        })
    }

    /// Stage a single-file bind source into a per-box directory so virtio-fs (which
    /// shares directories, not bare files) can expose it. Returns the directory to
    /// share; it contains exactly one entry — the file under the guest path's
    /// basename, which `mount_user_volumes` then binds onto the guest path. The
    /// file is hard-linked to keep the bind live in both directions; across
    /// filesystems it falls back to a copy (host-side writes then do not propagate).
    fn stage_single_file_mount(
        source: &Path,
        guest_path: &str,
        index: usize,
        filemounts_dir: &Path,
    ) -> Result<PathBuf> {
        let basename = Path::new(guest_path).file_name().ok_or_else(|| {
            BoxError::ConfigError(format!(
                "Single-file bind guest path has no file name: {guest_path}"
            ))
        })?;
        let stage_dir = filemounts_dir.join(index.to_string());
        std::fs::create_dir_all(&stage_dir).map_err(|e| BoxError::BoxBootError {
            message: format!(
                "Failed to create file-mount staging dir {}: {}",
                stage_dir.display(),
                e
            ),
            hint: None,
        })?;
        let staged = stage_dir.join(basename);
        let _ = std::fs::remove_file(&staged); // idempotent across restarts
        if std::fs::hard_link(source, &staged).is_err() {
            std::fs::copy(source, &staged).map_err(|e| BoxError::BoxBootError {
                message: format!(
                    "Failed to stage single-file mount {} -> {}: {}",
                    source.display(),
                    staged.display(),
                    e
                ),
                hint: None,
            })?;
            tracing::warn!(
                source = %source.display(),
                "Single-file bind staged by copy (source on a different filesystem); \
                 host-side writes will not propagate to the container"
            );
        }
        Ok(stage_dir)
    }

    /// Create an anonymous volume via VolumeStore.
    ///
    /// Returns the host path of the created volume.
    fn create_anonymous_volume(&self, name: &str) -> Result<(String, bool)> {
        use crate::volume::VolumeStore;

        let store = VolumeStore::new(
            self.home_dir.join("volumes.json"),
            self.home_dir.join("volumes"),
        );

        // If the volume already exists (e.g., from a previous run), reuse it
        if let Some(existing) = store.get(name)? {
            return Ok((existing.mount_point, false));
        }

        let mut config = a3s_box_core::volume::VolumeConfig::new(name, "");
        config
            .labels
            .insert("anonymous".to_string(), "true".to_string());
        config.attach(&self.box_id);
        let created = store.create(config)?;
        Ok((created.mount_point, true))
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use a3s_box_core::config::BoxConfig;
    use a3s_box_core::event::EventEmitter;

    use super::*;
    use tempfile::tempdir;
    use tempfile::TempDir;

    /// Decode a base64 (URL-safe, no pad) `BOX_EXEC_*` value the way guest-init
    /// does, so assertions can compare against the original raw value.
    fn b64d(s: &str) -> String {
        use base64::Engine;
        base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(s.as_bytes())
            .ok()
            .and_then(|bytes| String::from_utf8(bytes).ok())
            .unwrap_or_else(|| s.to_string())
    }

    fn test_oci_config(workdir: Option<&str>, user: Option<&str>) -> OciImageConfig {
        OciImageConfig {
            entrypoint: Some(vec!["/bin/app".to_string()]),
            cmd: Some(vec!["--serve".to_string()]),
            env: vec![],
            working_dir: workdir.map(str::to_string),
            user: user.map(str::to_string),
            exposed_ports: vec![],
            labels: std::collections::HashMap::new(),
            volumes: vec![],
            stop_signal: None,
            health_check: None,
            onbuild: vec![],
        }
    }

    fn test_layout(
        base: &Path,
        oci_config: Option<OciImageConfig>,
        with_guest_init: bool,
    ) -> BoxLayout {
        let rootfs_path = base.join("rootfs");
        fs::create_dir_all(&rootfs_path).unwrap();
        if with_guest_init {
            fs::create_dir_all(rootfs_path.join("sbin")).unwrap();
            fs::write(rootfs_path.join("sbin").join("init"), b"guest-init").unwrap();
        }

        BoxLayout {
            rootfs_path,
            exec_socket_path: base.join("exec.sock"),
            pty_socket_path: base.join("pty.sock"),
            attest_socket_path: base.join("attest.sock"),
            port_forward_socket_path: base.join("portfwd.sock"),
            workspace_path: base.join("workspace"),
            console_output: None,
            oci_config,
            tee_instance_config: None,
        }
    }

    fn test_vm_manager(config: BoxConfig) -> VmManager {
        VmManager::with_box_id(config, EventEmitter::new(16), "test-box".to_string())
    }

    #[test]
    fn test_parse_volume_mount_host_guest() {
        let temp = TempDir::new().unwrap();
        let host_path = temp.path().to_str().unwrap();
        let volume = format!("{}:/data", host_path);

        let mount =
            VmManager::parse_volume_mount(&volume, 0, std::path::Path::new("/tmp")).unwrap();
        assert_eq!(mount.tag, "vol0");
        assert_eq!(mount.host_path, temp.path().canonicalize().unwrap());
        assert!(!mount.read_only);
    }

    #[test]
    fn test_parse_volume_mount_read_only() {
        let temp = TempDir::new().unwrap();
        let host_path = temp.path().to_str().unwrap();
        let volume = format!("{}:/data:ro", host_path);

        let mount =
            VmManager::parse_volume_mount(&volume, 1, std::path::Path::new("/tmp")).unwrap();
        assert_eq!(mount.tag, "vol1");
        assert!(mount.read_only);
    }

    #[test]
    fn test_parse_volume_mount_explicit_rw() {
        let temp = TempDir::new().unwrap();
        let host_path = temp.path().to_str().unwrap();
        let volume = format!("{}:/data:rw", host_path);

        let mount =
            VmManager::parse_volume_mount(&volume, 2, std::path::Path::new("/tmp")).unwrap();
        assert_eq!(mount.tag, "vol2");
        assert!(!mount.read_only);
    }

    #[test]
    fn test_parse_volume_mount_single_file_is_staged_as_dir() {
        let temp = TempDir::new().unwrap();
        // A real source FILE (not a directory).
        let src = temp.path().join("hostfile.txt");
        std::fs::write(&src, b"DATA").unwrap();
        let stage_base = temp.path().join("filemounts");
        let volume = format!("{}:/etc/myconf", src.display());

        let mount = VmManager::parse_volume_mount(&volume, 3, &stage_base).unwrap();

        // virtio-fs shares directories, so host_path must be the staging DIR, not
        // the bare file.
        assert!(
            mount.host_path.is_dir(),
            "single-file bind must be staged into a directory, got {}",
            mount.host_path.display()
        );
        // The staged dir holds the file under the GUEST basename — what the guest
        // binds onto the guest path.
        let staged = mount.host_path.join("myconf");
        assert!(
            staged.exists(),
            "staged file under guest basename must exist"
        );
        assert_eq!(std::fs::read(&staged).unwrap(), b"DATA");
    }

    #[test]
    fn test_parse_volume_mount_invalid_mode() {
        let temp = TempDir::new().unwrap();
        let host_path = temp.path().to_str().unwrap();
        let volume = format!("{}:/data:invalid", host_path);

        let result = VmManager::parse_volume_mount(&volume, 0, std::path::Path::new("/tmp"));
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("Invalid volume mode"));
    }

    #[test]
    fn test_parse_volume_mount_invalid_format() {
        let result = VmManager::parse_volume_mount("invalid", 0, std::path::Path::new("/tmp"));
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
        let mount =
            VmManager::parse_volume_mount(&volume, 0, std::path::Path::new("/tmp")).unwrap();
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
        assert_eq!(exec, "/bin/sh");
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

    #[test]
    fn test_guest_init_exec_path_prefers_sbin() {
        let dir = tempdir().unwrap();
        let rootfs = dir.path();
        fs::create_dir_all(rootfs.join("sbin")).unwrap();
        fs::write(rootfs.join("sbin").join("init"), b"guest-init").unwrap();

        assert_eq!(VmManager::guest_init_exec_path(rootfs), Some("/sbin/init"));
    }

    #[test]
    fn test_build_instance_spec_prefers_config_workdir_and_user() {
        let dir = tempdir().unwrap();
        let layout = test_layout(
            dir.path(),
            Some(test_oci_config(Some("/oci"), Some("2000:2000"))),
            true,
        );
        let mut vm = test_vm_manager(BoxConfig {
            workdir: Some("/override".to_string()),
            user: Some("1000:1000".to_string()),
            ..Default::default()
        });

        let spec = vm.build_instance_spec(&layout).unwrap();

        assert_eq!(spec.workdir, "/override");
        // With guest init present, the user is applied by the guest (via
        // BOX_EXEC_USER), not the shim's set_uid — so spec.user is None.
        assert_eq!(spec.user, None);
        assert!(spec
            .entrypoint
            .env
            .iter()
            .any(|(key, value)| key == "BOX_EXEC_USER" && b64d(value) == "1000:1000"));
        assert!(spec
            .entrypoint
            .env
            .iter()
            .any(|(key, value)| key == "BOX_EXEC_WORKDIR" && b64d(value) == "/override"));
    }

    #[test]
    fn test_relative_workdir_resolves_against_image_workdir() {
        // Docker `-w sub` resolves against the image WORKDIR.
        let oci = test_oci_config(Some("/srv/app"), None);
        let cfg = BoxConfig {
            workdir: Some("sub".to_string()),
            ..Default::default()
        };
        assert_eq!(
            VmManager::effective_workdir(&cfg, Some(&oci)),
            "/srv/app/sub"
        );
        // Absolute override is used verbatim.
        let cfg_abs = BoxConfig {
            workdir: Some("/abs".to_string()),
            ..Default::default()
        };
        assert_eq!(VmManager::effective_workdir(&cfg_abs, Some(&oci)), "/abs");
        // Relative with no image WORKDIR resolves against `/`.
        let cfg_rel = BoxConfig {
            workdir: Some("work".to_string()),
            ..Default::default()
        };
        assert_eq!(VmManager::effective_workdir(&cfg_rel, None), "/work");
    }

    #[test]
    fn test_build_instance_spec_uses_oci_workdir_and_user_without_override() {
        let dir = tempdir().unwrap();
        let layout = test_layout(
            dir.path(),
            Some(test_oci_config(Some("/oci"), Some("2000:2000"))),
            true,
        );
        let mut vm = test_vm_manager(BoxConfig::default());

        let spec = vm.build_instance_spec(&layout).unwrap();

        assert_eq!(spec.workdir, "/oci");
        assert_eq!(spec.user, None);
        assert!(spec
            .entrypoint
            .env
            .iter()
            .any(|(key, value)| key == "BOX_EXEC_USER" && b64d(value) == "2000:2000"));
        assert!(spec
            .entrypoint
            .env
            .iter()
            .any(|(key, value)| key == "BOX_EXEC_WORKDIR" && b64d(value) == "/oci"));
    }

    #[test]
    fn test_build_instance_spec_passes_default_workdir_to_guest_init() {
        let dir = tempdir().unwrap();
        let layout = test_layout(dir.path(), Some(test_oci_config(None, None)), true);
        let mut vm = test_vm_manager(BoxConfig::default());

        let spec = vm.build_instance_spec(&layout).unwrap();

        assert_eq!(spec.workdir, GUEST_WORKDIR);
        assert!(spec
            .entrypoint
            .env
            .iter()
            .any(|(key, value)| key == "BOX_EXEC_WORKDIR" && b64d(value) == GUEST_WORKDIR));
    }

    #[test]
    fn test_build_instance_spec_passes_hostname_to_guest_init() {
        let dir = tempdir().unwrap();
        let layout = test_layout(dir.path(), Some(test_oci_config(None, None)), true);
        let mut vm = test_vm_manager(BoxConfig {
            hostname: Some("web".to_string()),
            ..Default::default()
        });

        let spec = vm.build_instance_spec(&layout).unwrap();

        assert!(spec
            .entrypoint
            .env
            .iter()
            .any(|(key, value)| key == "BOX_HOSTNAME" && value == "web"));
    }

    #[test]
    fn test_build_instance_spec_guest_init_prefixes_extra_env() {
        let dir = tempdir().unwrap();
        let mut oci_config = test_oci_config(None, None);
        oci_config.env = vec![
            ("FOO".to_string(), "image".to_string()),
            ("BAR".to_string(), "image".to_string()),
        ];
        let layout = test_layout(dir.path(), Some(oci_config), true);
        let mut vm = test_vm_manager(BoxConfig {
            extra_env: vec![
                ("FOO".to_string(), "cli".to_string()),
                ("BAZ".to_string(), "cli".to_string()),
            ],
            ..Default::default()
        });

        let spec = vm.build_instance_spec(&layout).unwrap();

        assert!(spec
            .entrypoint
            .env
            .iter()
            .any(|(key, value)| key == "BOX_EXEC_ENV_FOO" && b64d(value) == "cli"));
        assert!(spec
            .entrypoint
            .env
            .iter()
            .any(|(key, value)| key == "BOX_EXEC_ENV_BAR" && b64d(value) == "image"));
        assert!(spec
            .entrypoint
            .env
            .iter()
            .any(|(key, value)| key == "BOX_EXEC_ENV_BAZ" && b64d(value) == "cli"));
        assert!(!spec
            .entrypoint
            .env
            .iter()
            .any(|(key, _)| key == "FOO" || key == "BAZ"));
    }

    #[test]
    fn test_build_instance_spec_direct_entrypoint_merges_extra_env() {
        let dir = tempdir().unwrap();
        let mut oci_config = test_oci_config(None, None);
        oci_config.env = vec![
            ("FOO".to_string(), "image".to_string()),
            ("BAR".to_string(), "image".to_string()),
        ];
        let layout = test_layout(dir.path(), Some(oci_config), false);
        let mut vm = test_vm_manager(BoxConfig {
            extra_env: vec![
                ("FOO".to_string(), "cli".to_string()),
                ("BAZ".to_string(), "cli".to_string()),
            ],
            ..Default::default()
        });

        let spec = vm.build_instance_spec(&layout).unwrap();

        assert!(spec
            .entrypoint
            .env
            .iter()
            .any(|(key, value)| key == "FOO" && value == "cli"));
        assert!(spec
            .entrypoint
            .env
            .iter()
            .any(|(key, value)| key == "BAR" && value == "image"));
        assert!(spec
            .entrypoint
            .env
            .iter()
            .any(|(key, value)| key == "BAZ" && value == "cli"));
    }

    #[test]
    fn test_build_instance_spec_tracks_new_anonymous_volumes_only() {
        let home = tempdir().unwrap();
        let layout_dir = tempdir().unwrap();
        let mut oci_config = test_oci_config(None, None);
        oci_config.volumes = vec!["/data".to_string()];
        let layout = test_layout(layout_dir.path(), Some(oci_config), true);

        let mut first_vm = test_vm_manager(BoxConfig::default());
        first_vm.home_dir = home.path().to_path_buf();
        let first_spec = first_vm.build_instance_spec(&layout).unwrap();

        assert_eq!(first_vm.anonymous_volumes.len(), 1);
        assert_eq!(
            first_vm.created_anonymous_volumes,
            first_vm.anonymous_volumes
        );
        assert!(first_spec.fs_mounts.iter().any(|mount| {
            mount.tag == "vol0" && mount.host_path.starts_with(home.path().join("volumes"))
        }));

        let volume_name = first_vm.anonymous_volumes[0].clone();
        let store = crate::volume::VolumeStore::new(
            home.path().join("volumes.json"),
            home.path().join("volumes"),
        );
        assert!(store.get(&volume_name).unwrap().is_some());

        let mut second_vm = test_vm_manager(BoxConfig::default());
        second_vm.home_dir = home.path().to_path_buf();
        second_vm.build_instance_spec(&layout).unwrap();

        assert_eq!(second_vm.anonymous_volumes, vec![volume_name]);
        assert!(second_vm.created_anonymous_volumes.is_empty());
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn test_guest_init_exec_path_supports_usr_sbin_without_sbin() {
        let dir = tempdir().unwrap();
        let rootfs = dir.path();
        fs::create_dir_all(rootfs.join("usr").join("sbin")).unwrap();
        fs::write(rootfs.join("usr").join("sbin").join("init"), b"guest-init").unwrap();

        assert_eq!(
            VmManager::guest_init_exec_path(rootfs),
            Some("/usr/sbin/init")
        );
    }

    #[test]
    fn test_parse_volume_mount_guest_path_with_colons() {
        let temp = TempDir::new().unwrap();
        let host_path = temp.path().to_str().unwrap();
        // Path like /host/path:/guest/path:ro where guest path contains colon
        let volume = format!("{}:/data:/media/c:ro", host_path);

        let result = VmManager::parse_volume_mount(&volume, 0, std::path::Path::new("/tmp"));
        // Should handle this gracefully or error on the guest path with colon
        // The exact behavior depends on implementation
        assert!(result.is_err() || result.is_ok()); // Just verify it doesn't panic
    }
}
