//! Low-level FFI bindings to libkrun
//!
//! This crate provides raw, unsafe bindings to the libkrun C library.
//! For a safe, idiomatic Rust API, use the higher-level wrapper in the runtime crate.
//!
//! ## Platform notes
//!
//! On **Windows** (WHPX backend):
//! - `krun_set_kernel()` **must** be called before `krun_start_enter()`.
//! - Unix-only functions (`krun_add_net_unixstream`, `krun_add_net_unixgram`,
//!   `krun_add_vsock_port2`, `krun_add_disk2`, `krun_set_root_disk_remount`) are
//!   not exported from `krun.dll` and are absent on this target.
//! - Windows-specific functions (`krun_add_net_tcp`, `krun_add_vsock_port_windows`)
//!   are only available on this target.
//! - `uid` / `gid` arguments are `u32` (no POSIX `uid_t` / `gid_t` on MSVC).

use std::os::raw::c_char;

// Log constants from libkrun.h
pub const KRUN_LOG_TARGET_DEFAULT: i32 = -1;
pub const KRUN_LOG_TARGET_STDOUT: i32 = 1;
pub const KRUN_LOG_TARGET_STDERR: i32 = 2;

pub const KRUN_LOG_LEVEL_OFF: u32 = 0;
pub const KRUN_LOG_LEVEL_ERROR: u32 = 1;
pub const KRUN_LOG_LEVEL_WARN: u32 = 2;
pub const KRUN_LOG_LEVEL_INFO: u32 = 3;
pub const KRUN_LOG_LEVEL_DEBUG: u32 = 4;
pub const KRUN_LOG_LEVEL_TRACE: u32 = 5;

pub const KRUN_LOG_STYLE_AUTO: u32 = 0;
pub const KRUN_LOG_STYLE_ALWAYS: u32 = 1;
pub const KRUN_LOG_STYLE_NEVER: u32 = 2;

pub const KRUN_LOG_OPTION_NO_ENV: u32 = 1;

// Disk format constants from libkrun.h
pub const KRUN_DISK_FORMAT_RAW: u32 = 0;
pub const KRUN_DISK_FORMAT_QCOW2: u32 = 1;

// Kernel format constants (used by krun_set_kernel on Windows)
pub const KRUN_KERNEL_FORMAT_RAW: u32 = 0;
pub const KRUN_KERNEL_FORMAT_ELF: u32 = 1;
pub const KRUN_KERNEL_FORMAT_PE_GZ: u32 = 2;
pub const KRUN_KERNEL_FORMAT_IMAGE_BZ2: u32 = 3;
pub const KRUN_KERNEL_FORMAT_IMAGE_GZ: u32 = 4;
pub const KRUN_KERNEL_FORMAT_IMAGE_ZSTD: u32 = 5;

// TSI feature flags
pub const KRUN_TSI_HIJACK_INET: u32 = 1 << 0;
pub const KRUN_TSI_HIJACK_UNIX: u32 = 1 << 1;

// ============================================================================
// Cross-platform FFI bindings (available on all supported targets)
// ============================================================================

extern "C" {
    /// Initialize libkrun logging system.
    pub fn krun_init_log(target: i32, level: u32, style: u32, flags: u32) -> i32;

    /// Set the log level for libkrun.
    pub fn krun_set_log_level(level: u32) -> i32;

    /// Create a new libkrun context.
    /// Returns context ID on success, negative error code on failure.
    pub fn krun_create_ctx() -> i32;

    /// Free a libkrun context.
    pub fn krun_free_ctx(ctx_id: u32) -> i32;

    /// Configure VM resources (vCPUs and memory).
    pub fn krun_set_vm_config(ctx_id: u32, num_vcpus: u8, ram_mib: u32) -> i32;

    /// Set the root filesystem path for the VM.
    pub fn krun_set_root(ctx_id: u32, root_path: *const c_char) -> i32;

    /// Add a virtiofs mount to share a host directory with the guest.
    pub fn krun_add_virtiofs(
        ctx_id: u32,
        mount_tag: *const c_char,
        host_path: *const c_char,
    ) -> i32;

    /// Set the kernel to load in the microVM.
    ///
    /// On Windows this function **must** be called before `krun_start_enter`.
    /// Use `KRUN_KERNEL_FORMAT_ELF` for a raw ELF vmlinux image.
    /// `initramfs` and `cmdline` may be null.
    pub fn krun_set_kernel(
        ctx_id: u32,
        kernel_path: *const c_char,
        kernel_format: u32,
        initramfs: *const c_char,
        cmdline: *const c_char,
    ) -> i32;

    /// Set the executable to run inside the VM.
    pub fn krun_set_exec(
        ctx_id: u32,
        exec_path: *const c_char,
        argv: *const *const c_char,
        envp: *const *const c_char,
    ) -> i32;

    /// Set environment variables for the VM.
    pub fn krun_set_env(ctx_id: u32, envp: *const *const c_char) -> i32;

    /// Set the working directory inside the VM.
    pub fn krun_set_workdir(ctx_id: u32, workdir_path: *const c_char) -> i32;

    /// Enable or disable split IRQ chip mode.
    pub fn krun_split_irqchip(ctx_id: u32, enable: bool) -> i32;

    /// Enable or disable nested virtualization.
    pub fn krun_set_nested_virt(ctx_id: u32, enabled: bool) -> i32;

    /// Set GPU options (virgl flags).
    pub fn krun_set_gpu_options(ctx_id: u32, virgl_flags: u32) -> i32;

    /// Set resource limits for the VM.
    pub fn krun_set_rlimits(ctx_id: u32, rlimits: *const *const c_char) -> i32;

    /// Set port mappings for the TSI backend.
    pub fn krun_set_port_map(ctx_id: u32, port_map: *const *const c_char) -> i32;

    /// Add a raw disk image to the VM.
    pub fn krun_add_disk(
        ctx_id: u32,
        block_id: *const c_char,
        disk_path: *const c_char,
        read_only: bool,
    ) -> i32;

    /// Start the VM and enter it (process takeover).
    /// On success, this function never returns.
    pub fn krun_start_enter(ctx_id: u32) -> i32;

    /// Redirect VM console output to a file.
    pub fn krun_set_console_output(ctx_id: u32, filepath: *const c_char) -> i32;

    /// Disable the implicit console device created by libkrun automatically.
    pub fn krun_disable_implicit_console(ctx_id: u32) -> i32;

    /// Disable the implicit vsock device created by libkrun automatically.
    pub fn krun_disable_implicit_vsock(ctx_id: u32) -> i32;

    /// Set the `console=` kernel command-line argument.
    pub fn krun_set_kernel_console(ctx_id: u32, console_id: *const c_char) -> i32;

    /// Add a legacy serial device (ttyS0) with the given input/output fds.
    pub fn krun_add_serial_console_default(
        ctx_id: u32,
        input_fd: std::os::raw::c_int,
        output_fd: std::os::raw::c_int,
    ) -> i32;

    /// Add a virtio-console device (single-port, auto-configured).
    pub fn krun_add_virtio_console_default(
        ctx_id: u32,
        input_fd: std::os::raw::c_int,
        output_fd: std::os::raw::c_int,
        err_fd: std::os::raw::c_int,
    ) -> i32;

    /// Add a multi-port virtio-console device. Returns a console_id (≥ 0).
    pub fn krun_add_virtio_console_multiport(ctx_id: u32) -> i32;

    /// Add a TTY port to a multi-port virtio-console device.
    pub fn krun_add_console_port_tty(
        ctx_id: u32,
        console_id: u32,
        name: *const c_char,
        tty_fd: std::os::raw::c_int,
    ) -> i32;

    /// Add a bidirectional I/O port to a multi-port virtio-console device.
    pub fn krun_add_console_port_inout(
        ctx_id: u32,
        console_id: u32,
        name: *const c_char,
        input_fd: std::os::raw::c_int,
        output_fd: std::os::raw::c_int,
    ) -> i32;

    /// Add a vsock device with the specified TSI features.
    ///
    /// `tsi_features`: bitmask of `KRUN_TSI_HIJACK_*`; use 0 for plain vsock.
    pub fn krun_add_vsock(ctx_id: u32, tsi_features: u32) -> i32;

    /// Returns an event fd (Linux) or Windows HANDLE as i32 for graceful shutdown.
    pub fn krun_get_shutdown_eventfd(ctx_id: u32) -> i32;
}

// ============================================================================
// UID / GID — different signatures per platform
// ============================================================================

#[cfg(not(target_os = "windows"))]
extern "C" {
    /// Set the uid before starting the microVM.
    pub fn krun_setuid(ctx_id: u32, uid: libc::uid_t) -> i32;

    /// Set the gid before starting the microVM.
    pub fn krun_setgid(ctx_id: u32, gid: libc::gid_t) -> i32;
}

/// On Windows `uid_t` / `gid_t` do not exist; libkrun uses `uint32_t` directly.
#[cfg(target_os = "windows")]
extern "C" {
    pub fn krun_setuid(ctx_id: u32, uid: u32) -> i32;
    pub fn krun_setgid(ctx_id: u32, gid: u32) -> i32;
}

// ============================================================================
// Unix-only bindings (not exported from krun.dll on Windows)
// ============================================================================

#[cfg(not(target_os = "windows"))]
extern "C" {
    /// Add a vsock port bridged to a Unix socket.
    pub fn krun_add_vsock_port2(
        ctx_id: u32,
        port: u32,
        filepath: *const c_char,
        listen: bool,
    ) -> i32;

    /// Add a disk image with explicit format specification.
    pub fn krun_add_disk2(
        ctx_id: u32,
        block_id: *const c_char,
        disk_path: *const c_char,
        disk_format: u32,
        read_only: bool,
    ) -> i32;

    /// Add a network backend via Unix stream socket (passt, socket_vmnet).
    pub fn krun_add_net_unixstream(
        ctx_id: u32,
        c_path: *const c_char,
        fd: i32,
        c_mac: *const u8,
        features: u32,
        flags: u32,
    ) -> i32;

    /// Add a network backend via Unix datagram socket (gvproxy, vmnet-helper).
    pub fn krun_add_net_unixgram(
        ctx_id: u32,
        c_path: *const c_char,
        fd: i32,
        c_mac: *const u8,
        features: u32,
        flags: u32,
    ) -> i32;

    /// Configure a root filesystem backed by a block device with automatic remount.
    pub fn krun_set_root_disk_remount(
        ctx_id: u32,
        device: *const c_char,
        fstype: *const c_char,
        options: *const c_char,
    ) -> i32;
}

// ============================================================================
// Windows-only bindings (WHPX backend)
// ============================================================================

/// Windows-only: add a virtio-net device backed by an optional TCP socket.
///
/// `c_mac` must point to a 6-byte MAC address array.
/// `c_tcp_addr` is a `"host:port"` string, or null for a disconnected device.
#[cfg(target_os = "windows")]
extern "C" {
    pub fn krun_add_net_tcp(
        ctx_id: u32,
        c_iface_id: *const c_char,
        c_mac: *const u8,
        c_tcp_addr: *const c_char,
    ) -> i32;

    /// Maps guest vsock `port` to a Windows Named Pipe (`\\.\pipe\<pipe_name>`).
    pub fn krun_add_vsock_port_windows(ctx_id: u32, port: u32, c_pipe_name: *const c_char) -> i32;
}

// ============================================================================
// TEE support — loaded at runtime via dlsym (Linux only)
// ============================================================================

/// Set the file path to the TEE configuration file.
///
/// Loaded at runtime via `dlsym` — only exists in libkrun builds with the
/// `tee` feature (amd-sev / tdx).  Returns `-ENOSYS` if the symbol is absent.
#[cfg(target_os = "linux")]
pub unsafe fn krun_set_tee_config_file(ctx_id: u32, filepath: *const c_char) -> i32 {
    type Func = unsafe extern "C" fn(u32, *const c_char) -> i32;

    static FUNC: std::sync::OnceLock<Option<Func>> = std::sync::OnceLock::new();

    let func = FUNC.get_or_init(|| {
        let sym = b"krun_set_tee_config_file\0";
        let ptr = libc::dlsym(libc::RTLD_DEFAULT, sym.as_ptr() as *const _);
        if ptr.is_null() {
            None
        } else {
            Some(std::mem::transmute::<*mut libc::c_void, Func>(ptr))
        }
    });

    match func {
        Some(f) => f(ctx_id, filepath),
        None => -libc::ENOSYS,
    }
}
