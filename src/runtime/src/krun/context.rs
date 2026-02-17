//! High-level context wrapper for libkrun interactions.
//!
//! All unsafe functions in this module wrap libkrun FFI calls.
//! They are marked unsafe because they call into C code and require
//! the caller to ensure the KrunContext is valid.

#![allow(clippy::missing_safety_doc)]

use std::{ffi::CString, ptr};

use super::check_status;
use a3s_box_core::error::{BoxError, Result};
use libkrun_sys::{
    krun_add_net_unixstream, krun_add_virtiofs, krun_add_vsock_port2, krun_create_ctx,
    krun_free_ctx, krun_init_log, krun_set_console_output, krun_set_env, krun_set_exec,
    krun_set_port_map, krun_set_rlimits, krun_set_root, krun_set_vm_config, krun_set_workdir,
    krun_setgid, krun_setuid, krun_split_irqchip, krun_start_enter,
};

/// Thin wrapper that owns a libkrun context.
pub struct KrunContext {
    ctx_id: u32,
}

impl KrunContext {
    /// Initialize libkrun logging system based on RUST_LOG environment variable.
    /// Must be called before creating any context.
    pub unsafe fn init_logging() -> Result<()> {
        use libkrun_sys::{
            KRUN_LOG_LEVEL_DEBUG, KRUN_LOG_LEVEL_ERROR, KRUN_LOG_LEVEL_INFO, KRUN_LOG_LEVEL_TRACE,
            KRUN_LOG_STYLE_AUTO, KRUN_LOG_TARGET_STDERR,
        };

        // Determine log level from RUST_LOG environment variable
        let log_level = match std::env::var("RUST_LOG").as_deref() {
            Ok("trace") | Ok("a3s_box=trace") => {
                tracing::debug!("Initializing libkrun with TRACE log level");
                KRUN_LOG_LEVEL_TRACE
            }
            Ok("debug") | Ok("a3s_box=debug") => {
                tracing::debug!("Initializing libkrun with DEBUG log level");
                KRUN_LOG_LEVEL_DEBUG
            }
            Ok("info") | Ok("a3s_box=info") => {
                tracing::debug!("Initializing libkrun with INFO log level");
                KRUN_LOG_LEVEL_INFO
            }
            _ => KRUN_LOG_LEVEL_ERROR, // Default: only show errors
        };

        let log_target = KRUN_LOG_TARGET_STDERR;
        let log_style = KRUN_LOG_STYLE_AUTO;
        let flags = 0;

        tracing::trace!(
            log_target,
            log_level,
            log_style,
            flags,
            "Calling krun_init_log"
        );

        check_status(
            "krun_init_log",
            krun_init_log(log_target, log_level, log_style, flags),
        )
    }

    /// Create a new libkrun context.
    #[allow(unsafe_op_in_unsafe_fn)]
    pub unsafe fn create() -> Result<Self> {
        tracing::trace!("Calling krun_create_ctx()");
        let ctx = krun_create_ctx();
        if ctx < 0 {
            tracing::error!(status = ctx, "krun_create_ctx failed");
            return Err(BoxError::BoxBootError {
                message: format!("krun_create_ctx failed with status {}", ctx),
                hint: Some("Ensure libkrun is properly installed".to_string()),
            });
        }
        tracing::trace!(ctx_id = ctx, "krun_create_ctx succeeded");
        Ok(Self { ctx_id: ctx as u32 })
    }

    /// Configure VM resources (vCPUs and memory).
    pub unsafe fn set_vm_config(&self, cpus: u8, memory_mib: u32) -> Result<()> {
        tracing::debug!(cpus, memory_mib, "Setting VM config");
        check_status(
            "krun_set_vm_config",
            krun_set_vm_config(self.ctx_id, cpus, memory_mib),
        )
    }

    /// Set the root filesystem path for the VM.
    pub unsafe fn set_root(&self, rootfs: &str) -> Result<()> {
        tracing::trace!(rootfs, "Setting rootfs");
        let rootfs_c = CString::new(rootfs).map_err(|e| BoxError::BoxBootError {
            message: format!("invalid rootfs path: {}", e),
            hint: None,
        })?;
        check_status(
            "krun_set_root",
            krun_set_root(self.ctx_id, rootfs_c.as_ptr()),
        )
    }

    /// Set the executable to run inside the VM.
    pub unsafe fn set_exec(
        &self,
        exec: &str,
        args: &[String],
        env: &[(String, String)],
    ) -> Result<()> {
        let exec_c = CString::new(exec).map_err(|e| BoxError::BoxBootError {
            message: format!("invalid exec path: {}", e),
            hint: None,
        })?;

        tracing::trace!(exec, args_count = args.len(), "Building argv array");
        for (i, arg) in args.iter().enumerate() {
            tracing::trace!(index = i, arg = ?arg, "Entrypoint argument");
        }

        let arg_storage: Vec<CString> = args
            .iter()
            .map(|arg| {
                CString::new(arg.as_str()).map_err(|e| BoxError::BoxBootError {
                    message: format!("invalid arg: {}", e),
                    hint: None,
                })
            })
            .collect::<Result<_>>()?;
        let mut arg_ptrs: Vec<*const std::ffi::c_char> =
            arg_storage.iter().map(|arg| arg.as_ptr()).collect();
        arg_ptrs.push(ptr::null());

        tracing::trace!(env_count = env.len(), "Building env array");
        for (k, v) in env.iter() {
            tracing::trace!(key = k, value = v, "Environment variable");
        }

        let env_storage = Self::env_to_cstring(env)?;
        let mut env_ptrs: Vec<*const std::ffi::c_char> =
            env_storage.iter().map(|entry| entry.as_ptr()).collect();
        env_ptrs.push(ptr::null());

        check_status(
            "krun_set_exec",
            krun_set_exec(
                self.ctx_id,
                exec_c.as_ptr(),
                arg_ptrs.as_ptr(),
                env_ptrs.as_ptr(),
            ),
        )
    }

    /// Set environment variables for the VM.
    pub unsafe fn set_env(&self, env: &[(String, String)]) -> Result<()> {
        if env.is_empty() {
            let empty: [*const std::ffi::c_char; 1] = [ptr::null()];
            return check_status("krun_set_env", krun_set_env(self.ctx_id, empty.as_ptr()));
        }

        let env_storage = Self::env_to_cstring(env)?;
        let mut ptrs: Vec<*const std::ffi::c_char> =
            env_storage.iter().map(|c| c.as_ptr()).collect();
        ptrs.push(ptr::null());

        check_status("krun_set_env", krun_set_env(self.ctx_id, ptrs.as_ptr()))
    }

    fn env_to_cstring(env: &[(String, String)]) -> Result<Vec<CString>> {
        env.iter()
            .map(|(k, v)| {
                CString::new(format!("{}={}", k, v)).map_err(|e| BoxError::BoxBootError {
                    message: format!("invalid env: {}", e),
                    hint: None,
                })
            })
            .collect()
    }

    /// Set the working directory inside the VM.
    pub unsafe fn set_workdir(&self, workdir: &str) -> Result<()> {
        let workdir_c = CString::new(workdir).map_err(|e| BoxError::BoxBootError {
            message: format!("invalid workdir path: {}", e),
            hint: None,
        })?;
        check_status(
            "krun_set_workdir",
            krun_set_workdir(self.ctx_id, workdir_c.as_ptr()),
        )
    }

    /// Set resource limits for the VM.
    pub unsafe fn set_rlimits(&self, rlimits: &[String]) -> Result<()> {
        tracing::trace!(rlimits = ?rlimits, "Setting rlimits");
        if rlimits.is_empty() {
            let empty: [*const std::ffi::c_char; 1] = [ptr::null()];
            return check_status(
                "krun_set_rlimits",
                krun_set_rlimits(self.ctx_id, empty.as_ptr()),
            );
        }

        let entries: Vec<CString> = rlimits
            .iter()
            .map(|rlimit| {
                CString::new(rlimit.as_str()).map_err(|e| BoxError::BoxBootError {
                    message: format!("invalid rlimit: {}", e),
                    hint: None,
                })
            })
            .collect::<Result<_>>()?;
        let mut ptrs: Vec<*const std::ffi::c_char> = entries.iter().map(|c| c.as_ptr()).collect();
        ptrs.push(ptr::null());

        check_status(
            "krun_set_rlimits",
            krun_set_rlimits(self.ctx_id, ptrs.as_ptr()),
        )
    }

    /// Add a virtiofs mount, sharing a host directory with the guest.
    ///
    /// # Arguments
    /// * `mount_tag` - Tag used by guest to mount this share (e.g., "workspace", "skills")
    /// * `host_path` - Path to directory on host to share
    pub unsafe fn add_virtiofs(&self, mount_tag: &str, host_path: &str) -> Result<()> {
        tracing::debug!(mount_tag, host_path, "Adding virtiofs mount");

        let host_path_c = CString::new(host_path).map_err(|e| BoxError::BoxBootError {
            message: format!("invalid host path: {}", e),
            hint: None,
        })?;
        let mount_tag_c = CString::new(mount_tag).map_err(|e| BoxError::BoxBootError {
            message: format!("invalid mount tag: {}", e),
            hint: None,
        })?;

        check_status(
            "krun_add_virtiofs",
            krun_add_virtiofs(self.ctx_id, mount_tag_c.as_ptr(), host_path_c.as_ptr()),
        )
    }

    /// Configure vsock port with Unix socket bridge.
    ///
    /// # Arguments
    /// * `port` - Guest vsock port number
    /// * `socket_path` - Host Unix socket path for the bridge
    /// * `listen` - If true, libkrun creates the socket and listens (host connects).
    ///   If false, libkrun connects to an existing socket (host listens).
    pub unsafe fn add_vsock_port(&self, port: u32, socket_path: &str, listen: bool) -> Result<()> {
        tracing::debug!(port, socket_path, listen, "Configuring vsock port");
        let socket_path_c = CString::new(socket_path).map_err(|e| BoxError::BoxBootError {
            message: format!("invalid socket path: {}", e),
            hint: None,
        })?;
        check_status(
            "krun_add_vsock_port2",
            krun_add_vsock_port2(self.ctx_id, port, socket_path_c.as_ptr(), listen),
        )
    }

    /// Configure TSI port mappings for the VM.
    ///
    /// Maps host ports to guest ports via TSI (Transparent Socket Impersonation).
    /// When a guest process listens on a guest port, it becomes accessible on the
    /// corresponding host port.
    ///
    /// # Arguments
    /// * `port_map` - Slice of "host_port:guest_port" strings (e.g., ["8080:80", "3000:3000"])
    pub unsafe fn set_port_map(&self, port_map: &[String]) -> Result<()> {
        tracing::debug!(port_map = ?port_map, "Setting TSI port mappings");
        let entries: Vec<CString> = port_map
            .iter()
            .map(|entry| {
                CString::new(entry.as_str()).map_err(|e| BoxError::BoxBootError {
                    message: format!("invalid port map entry: {}", e),
                    hint: None,
                })
            })
            .collect::<Result<_>>()?;
        let mut ptrs: Vec<*const std::ffi::c_char> = entries.iter().map(|c| c.as_ptr()).collect();
        ptrs.push(ptr::null());

        check_status(
            "krun_set_port_map",
            krun_set_port_map(self.ctx_id, ptrs.as_ptr()),
        )
    }

    /// Add a virtio-net device connected to a passt Unix stream socket.
    ///
    /// This disables TSI and gives the guest a real network interface (eth0).
    /// Each call adds another interface (eth0, eth1, ...).
    ///
    /// # Arguments
    /// * `socket_path` - Path to the passt Unix stream socket
    /// * `mac` - MAC address as 6 bytes
    ///
    /// # Virtio-net features
    /// Uses the standard compat features: CSUM, GUEST_CSUM, GUEST_TSO4, GUEST_UFO,
    /// HOST_TSO4, HOST_UFO.
    pub unsafe fn add_net_unixstream(&self, socket_path: &str, mac: &[u8; 6]) -> Result<()> {
        tracing::debug!(socket_path, mac = ?mac, "Adding virtio-net via passt");

        let path_c = CString::new(socket_path)
            .map_err(|e| BoxError::NetworkError(format!("invalid passt socket path: {}", e)))?;

        // Standard compat features (same as COMPAT_NET_FEATURES in libkrun.h)
        let features: u32 = (1 << 0)   // NET_FEATURE_CSUM
            | (1 << 1)                   // NET_FEATURE_GUEST_CSUM
            | (1 << 7)                   // NET_FEATURE_GUEST_TSO4
            | (1 << 10)                  // NET_FEATURE_GUEST_UFO
            | (1 << 11)                  // NET_FEATURE_HOST_TSO4
            | (1 << 14); // NET_FEATURE_HOST_UFO

        check_status(
            "krun_add_net_unixstream",
            krun_add_net_unixstream(
                self.ctx_id,
                path_c.as_ptr(),
                -1, // use path, not fd
                mac.as_ptr(),
                features,
                0, // no flags
            ),
        )
    }

    /// Set the user ID for the VM process.
    ///
    /// The UID is applied right before the microVM starts.
    pub unsafe fn set_uid(&self, uid: libc::uid_t) -> Result<()> {
        tracing::debug!(uid, "Setting VM uid");
        check_status("krun_setuid", krun_setuid(self.ctx_id, uid))
    }

    /// Set the group ID for the VM process.
    ///
    /// The GID is applied right before the microVM starts.
    pub unsafe fn set_gid(&self, gid: libc::gid_t) -> Result<()> {
        tracing::debug!(gid, "Setting VM gid");
        check_status("krun_setgid", krun_setgid(self.ctx_id, gid))
    }

    /// Redirect VM console output to a file.
    pub unsafe fn set_console_output(&self, filepath: &str) -> Result<()> {
        tracing::debug!(filepath, "Setting console output path");
        let filepath_c = CString::new(filepath).map_err(|e| BoxError::BoxBootError {
            message: format!("invalid console output path: {}", e),
            hint: None,
        })?;
        check_status(
            "krun_set_console_output",
            krun_set_console_output(self.ctx_id, filepath_c.as_ptr()),
        )
    }

    /// Enable split IRQ chip mode (required for TEE VMs).
    ///
    /// This must be called before starting a TEE-enabled VM.
    pub unsafe fn enable_split_irqchip(&self) -> Result<()> {
        tracing::debug!("Enabling split IRQ chip for TEE");
        let ret = krun_split_irqchip(self.ctx_id, true);
        if ret < 0 {
            return Err(BoxError::TeeConfig(format!(
                "Failed to enable split IRQ chip: error code {}",
                ret
            )));
        }
        Ok(())
    }

    /// Set the TEE configuration file path.
    ///
    /// This configures the VM to run in a Trusted Execution Environment
    /// using the specified configuration file (JSON format).
    ///
    /// # Arguments
    /// * `config_path` - Path to the TEE configuration JSON file
    ///
    /// # Note
    /// This function is only available on Linux when libkrun is built with SEV support.
    /// Call `enable_split_irqchip()` before this function.
    #[cfg(target_os = "linux")]
    pub unsafe fn set_tee_config(&self, config_path: &str) -> Result<()> {
        tracing::debug!(config_path, "Setting TEE configuration file");
        let path_c = CString::new(config_path)
            .map_err(|e| BoxError::TeeConfig(format!("Invalid TEE config path: {}", e)))?;
        let ret = libkrun_sys::krun_set_tee_config_file(self.ctx_id, path_c.as_ptr());
        if ret < 0 {
            return Err(BoxError::TeeConfig(format!(
                "Failed to set TEE config file '{}': error code {}",
                config_path, ret
            )));
        }
        Ok(())
    }

    /// Start the VM and enter it (process takeover).
    ///
    /// # Safety
    /// This function performs process takeover - on success, it never returns.
    /// The current process becomes the VM. This should only be called from
    /// a subprocess (shim) to isolate the VM from the host application.
    ///
    /// # Returns
    /// * On success: Never returns (process becomes VM)
    /// * On failure: Returns negative error code
    /// * On guest exit: Returns the guest's exit status (non-negative)
    pub unsafe fn start_enter(&self) -> i32 {
        tracing::trace!(ctx_id = self.ctx_id, "Calling krun_start_enter");
        let status = krun_start_enter(self.ctx_id);
        tracing::trace!(status, "krun_start_enter returned");
        if status < 0 {
            tracing::error!(status, "krun_start_enter failed");
        }
        status
    }
}

impl Drop for KrunContext {
    fn drop(&mut self) {
        unsafe {
            let _ = krun_free_ctx(self.ctx_id);
        }
    }
}
