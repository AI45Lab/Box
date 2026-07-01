# a3s-libkrun-sys

[![Crates.io](https://img.shields.io/crates/v/a3s-libkrun-sys.svg)](https://crates.io/crates/a3s-libkrun-sys)
[![Documentation](https://docs.rs/a3s-libkrun-sys/badge.svg)](https://docs.rs/a3s-libkrun-sys)
[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](LICENSE)

FFI bindings to [libkrun](https://github.com/containers/libkrun) with **Windows WHPX backend support**.

## Features

- ✅ **Cross-platform**: Linux (KVM), macOS (Hypervisor.framework), Windows (WHPX)
- ✅ **Windows WHPX Backend**: Full Windows Hypervisor Platform support
- ✅ **virtiofs**: Passthrough filesystem on all platforms
- ✅ **virtio-net**: Network device with TCP backend on Windows
- ✅ **virtio-blk**: Block device support
- ✅ **virtio-console**: Serial console
- ✅ **TSI**: Transparent Socket Impersonation for vsock

## Platform Support

| Platform | Backend | Status |
|----------|---------|--------|
| Linux x86_64 | KVM | ✅ Supported |
| Linux aarch64 | KVM | ✅ Supported |
| macOS arm64 | Hypervisor.framework | ✅ Supported |
| **Windows x86_64** | **WHPX** | ✅ **Supported** |

## Installation

Add to your `Cargo.toml`:

```toml
[dependencies]
a3s-libkrun-sys = "0.1.5"
```

### Windows Requirements

On Windows, you need to enable the Windows Hypervisor Platform:

```powershell
# Run as Administrator
Enable-WindowsOptionalFeature -Online -FeatureName HypervisorPlatform

# Reboot required
Restart-Computer
```

Verify it's enabled:

```powershell
Get-WindowsOptionalFeature -Online -FeatureName HypervisorPlatform
```

## Usage

```rust
use a3s_libkrun_sys::*;
use std::ffi::CString;

unsafe {
    // Create VM context
    let ctx = krun_create_ctx();
    let ctx_id = ctx as u32;

    // Configure VM (2 vCPUs, 512 MiB RAM)
    krun_set_vm_config(ctx_id, 2, 512);

    // Set kernel
    let kernel = CString::new("/path/to/vmlinux").unwrap();
    let cmdline = CString::new("console=ttyS0 root=/dev/vda rw").unwrap();
    krun_set_kernel(
        ctx_id,
        kernel.as_ptr(),
        KRUN_KERNEL_FORMAT_ELF,
        std::ptr::null(),
        cmdline.as_ptr(),
    );

    // Set root filesystem (virtiofs)
    let root = CString::new("/path/to/rootfs").unwrap();
    krun_set_root(ctx_id, root.as_ptr());

    // Configure network (Windows: TCP backend)
    #[cfg(target_os = "windows")]
    {
        let iface = CString::new("eth0").unwrap();
        let mac: [u8; 6] = [0x52, 0x54, 0x00, 0x12, 0x34, 0x56];
        krun_add_net_tcp(ctx_id, iface.as_ptr(), mac.as_ptr(), std::ptr::null());
    }

    // Set workload
    let exec = CString::new("/bin/sh").unwrap();
    let arg0 = CString::new("sh").unwrap();
    let argv = [arg0.as_ptr(), std::ptr::null()];
    krun_set_exec(ctx_id, exec.as_ptr(), argv.as_ptr(), std::ptr::null());

    // Start VM (does not return on success)
    krun_start_enter(ctx_id);
}
```

## Windows-Specific APIs

### Network Device (TCP Backend)

```rust
#[cfg(target_os = "windows")]
unsafe {
    let iface = CString::new("eth0").unwrap();
    let mac: [u8; 6] = [0x52, 0x54, 0x00, 0x12, 0x34, 0x56];
    let tcp_addr = CString::new("127.0.0.1:9000").unwrap();

    // Add virtio-net with TCP backend
    krun_add_net_tcp(ctx_id, iface.as_ptr(), mac.as_ptr(), tcp_addr.as_ptr());

    // Or disconnected device (null backend)
    krun_add_net_tcp(ctx_id, iface.as_ptr(), mac.as_ptr(), std::ptr::null());
}
```

### Block Device

```rust
#[cfg(target_os = "windows")]
unsafe {
    let disk_id = CString::new("vda").unwrap();
    let disk_path = CString::new("C:\\\\path\\\\to\\\\disk.img").unwrap();

    krun_add_disk(ctx_id, disk_id.as_ptr(), disk_path.as_ptr(), false);
}
```

### VSock with Named Pipes

```rust
#[cfg(target_os = "windows")]
unsafe {
    // Add vsock device
    krun_add_vsock(ctx_id, 3);

    // Map vsock port to Named Pipe
    let pipe_name = CString::new("myservice").unwrap();
    krun_add_vsock_port_windows(ctx_id, 8080, pipe_name.as_ptr());
    // Creates: \\.\pipe\myservice
}
```

## Examples

See the [examples](examples/) directory:

- [`windows_vm_test.rs`](examples/windows_vm_test.rs) - Basic Windows VM test
- [`nginx_test.rs`](examples/nginx_test.rs) - Full nginx container test

Run examples:

```powershell
# Windows
cargo run --example windows_vm_test --target x86_64-pc-windows-msvc
cargo run --example nginx_test --target x86_64-pc-windows-msvc
```

## Building from Source

### Windows

```powershell
# Clone with submodules
git clone --recursive https://github.com/AI45Lab/Box.git
cd Box/src/deps/libkrun-sys

# Build libkrun
cd vendor/libkrun
cargo build --release --target x86_64-pc-windows-msvc

# Copy DLL
Copy-Item target\x86_64-pc-windows-msvc\release\krun.dll ..\..\prebuilt\x86_64-pc-windows-msvc\

# Run tests
cd ..\..
cargo test --target x86_64-pc-windows-msvc --lib -- --test-threads=1
```

## Architecture

The Windows WHPX backend includes:

- **WHPX VM/vCPU Management** (`src/vmm/src/windows/vstate.rs`, `whpx_vcpu.rs`)
- **virtio Devices**:
  - `virtio-fs` - virtiofs passthrough (`src/devices/src/virtio/fs/windows/`)
  - `virtio-net` - TCP backend (`src/devices/src/virtio/net_windows.rs`)
  - `virtio-blk` - File-backed block device (`src/devices/src/virtio/block_windows.rs`)
  - `virtio-console` - Serial console (`src/devices/src/virtio/console_windows.rs`)
  - `virtio-vsock` - TSI implementation (`src/devices/src/virtio/vsock/tsi/windows/`)
- **EventFd** - Windows event wrapper (`src/utils/src/windows/eventfd.rs`)

## Testing

```powershell
# All tests (Windows)
cargo test --target x86_64-pc-windows-msvc --lib -- --test-threads=1

# Specific test
cargo test --target x86_64-pc-windows-msvc --lib test_krun_create_ctx -- --test-threads=1
```

**Note**: Use `--test-threads=1` on Windows due to WHPX partition limits.

## Documentation

- [Windows Integration Guide](../../WINDOWS_INTEGRATION_TEST.md)
- [API Documentation](https://docs.rs/a3s-libkrun-sys)
- [libkrun Documentation](https://github.com/containers/libkrun)

## License

MIT License - see [LICENSE](../../LICENSE) for details.

## Contributing

Contributions welcome! Please see [CONTRIBUTING.md](../../CONTRIBUTING.md).

## Credits

- Based on [libkrun](https://github.com/containers/libkrun) by Red Hat
- Windows WHPX backend by A3S Lab Team
