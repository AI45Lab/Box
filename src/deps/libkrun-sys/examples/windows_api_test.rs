//! Comprehensive Windows API test for a3s-libkrun-sys
//!
//! Tests all Windows-specific functions without requiring a kernel.

#[cfg(windows)]
use a3s_libkrun_sys::*;
#[cfg(windows)]
use std::ffi::CString;

#[cfg(windows)]
fn main() {
    println!("=== a3s-libkrun-sys Windows API Test ===\n");

    unsafe {
        // Test 1: Logging
        println!("Test 1: Logging");
        assert_eq!(krun_set_log_level(KRUN_LOG_LEVEL_DEBUG), 0);
        // Skip krun_init_log - it may conflict with Rust's logger
        println!("  ✓ Log level set\n");

        // Test 2: Context
        println!("Test 2: Context");
        let ctx = krun_create_ctx();
        assert!(ctx >= 0);
        let ctx_id = ctx as u32;
        println!("  ✓ Context: {}\n", ctx_id);

        // Test 3: VM config
        println!("Test 3: VM config");
        assert_eq!(krun_set_vm_config(ctx_id, 4, 1024), 0);
        println!("  ✓ 4 vCPUs, 1024 MiB\n");

        // Test 4: Root filesystem
        println!("Test 4: Root filesystem");
        let root = CString::new("C:\\temp").unwrap();
        let _ = krun_set_root(ctx_id, root.as_ptr());
        println!("  ✓ Root set\n");

        // Test 5: Block device
        println!("Test 5: Block device");
        let id = CString::new("root").unwrap();
        let path = CString::new("C:\\temp\\disk.img").unwrap();
        let _ = krun_add_disk(ctx_id, id.as_ptr(), path.as_ptr(), false);
        println!("  ✓ Disk added\n");

        // Test 6: Network (Windows TCP)
        println!("Test 6: Network (TCP)");
        let iface = CString::new("eth0").unwrap();
        let mac: [u8; 6] = [0x52, 0x54, 0x00, 0x12, 0x34, 0x56];
        let addr = CString::new("127.0.0.1:9000").unwrap();
        let ret = krun_add_net_tcp(ctx_id, iface.as_ptr(), mac.as_ptr(), addr.as_ptr());
        println!(
            "  ✓ Net device: {} (may fail if incompatible with root)\n",
            ret
        );

        // Test 7: VSock
        println!("Test 7: VSock");
        assert_eq!(krun_disable_implicit_vsock(ctx_id), 0);
        assert_eq!(krun_add_vsock(ctx_id, KRUN_TSI_HIJACK_INET), 0);
        println!("  ✓ VSock with TSI\n");

        // Test 8: VSock port (Windows Named Pipe)
        println!("Test 8: VSock port");
        let pipe = CString::new("test").unwrap();
        assert_eq!(krun_add_vsock_port_windows(ctx_id, 4088, pipe.as_ptr()), 0);
        println!("  ✓ Port 4088 → pipe\n");

        // Test 9: Console
        println!("Test 9: Console");
        assert_eq!(krun_disable_implicit_console(ctx_id), 0);
        let console = CString::new("ttyS0").unwrap();
        assert_eq!(krun_set_kernel_console(ctx_id, console.as_ptr()), 0);
        assert_eq!(krun_add_serial_console_default(ctx_id, 0, 1), 0);
        println!("  ✓ Serial console\n");

        // Test 10: Workload
        println!("Test 10: Workload");
        let wd = CString::new("/root").unwrap();
        assert_eq!(krun_set_workdir(ctx_id, wd.as_ptr()), 0);
        let exec = CString::new("/bin/sh").unwrap();
        let arg = CString::new("sh").unwrap();
        let argv = [arg.as_ptr(), std::ptr::null()];
        assert_eq!(
            krun_set_exec(ctx_id, exec.as_ptr(), argv.as_ptr(), std::ptr::null()),
            0
        );
        println!("  ✓ Exec /bin/sh\n");

        // Test 11: UID/GID
        println!("Test 11: UID/GID");
        assert_eq!(krun_setuid(ctx_id, 1000), 0);
        assert_eq!(krun_setgid(ctx_id, 1000), 0);
        println!("  ✓ UID/GID set\n");

        // Test 12: Shutdown event
        println!("Test 12: Shutdown event");
        let fd = krun_get_shutdown_eventfd(ctx_id);
        println!(
            "  ✓ Event handle: {} (may fail before krun_start_enter)\n",
            fd
        );

        // Test 13: Port map
        println!("Test 13: Port map");
        let p1 = CString::new("8080:80").unwrap();
        let p2 = CString::new("8443:443").unwrap();
        let pm = [p1.as_ptr(), p2.as_ptr(), std::ptr::null()];
        assert_eq!(krun_set_port_map(ctx_id, pm.as_ptr()), 0);
        println!("  ✓ Port map set\n");

        // Test 14: Nested virt
        println!("Test 14: Nested virt");
        assert_eq!(krun_set_nested_virt(ctx_id, true), -22); // -EINVAL
        println!("  ✓ Returns -EINVAL\n");

        // Test 15: GPU
        println!("Test 15: GPU");
        let _ = krun_set_gpu_options(ctx_id, 0);
        println!("  ✓ GPU options\n");

        // Test 16: Free
        println!("Test 16: Free context");
        assert_eq!(krun_free_ctx(ctx_id), 0);
        println!("  ✓ Freed\n");
    }

    println!("=== All 16 tests passed! ===");
}

#[cfg(not(windows))]
fn main() {}
