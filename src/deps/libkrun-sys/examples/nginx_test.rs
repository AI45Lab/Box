//! Test nginx container with real kernel on Windows
//!
//! This test demonstrates running a real Linux VM with nginx using libkrun on Windows.

use a3s_libkrun_sys::*;
use std::ffi::CString;
use std::path::Path;
use std::env;

fn main() {
    println!("=== a3s-box nginx Container Test (Windows) ===\n");

    // Get kernel path
    let kernel_path = env::var("TEST_VMLINUX_PATH")
        .unwrap_or_else(|_| {
            let temp = env::var("TEMP").unwrap_or_else(|_| "C:\\Users\\18770\\AppData\\Local\\Temp".to_string());
            format!("{}\\libkrun-kernels\\vmlinux-5.10.225", temp)
        });

    if !Path::new(&kernel_path).exists() {
        eprintln!("Error: Kernel not found at: {}", kernel_path);
        eprintln!("Run: powershell -File tests\\windows\\download_test_kernel.ps1");
        std::process::exit(1);
    }

    println!("Using kernel: {}\n", kernel_path);

    // Create a minimal rootfs for testing
    let rootfs_path = "C:\\temp\\test-rootfs";
    if !Path::new(rootfs_path).exists() {
        eprintln!("Creating minimal rootfs at: {}", rootfs_path);
        std::fs::create_dir_all(rootfs_path).expect("Failed to create rootfs");

        // Create basic directory structure
        for dir in &["bin", "dev", "proc", "sys", "tmp", "etc", "var/log"] {
            let path = format!("{}\\{}", rootfs_path, dir);
            std::fs::create_dir_all(&path).ok();
        }

        println!("Note: This is a minimal rootfs. For nginx, you need:");
        println!("  1. Extract nginx:alpine Docker image");
        println!("  2. Or use: docker export $(docker create nginx:alpine) | tar -xC {}", rootfs_path);
        println!();
    }

    unsafe {
        // 1. Create context
        println!("1. Creating VM context...");
        let ctx = krun_create_ctx();
        if ctx < 0 {
            eprintln!("Failed to create context: {}", ctx);
            std::process::exit(1);
        }
        let ctx_id = ctx as u32;
        println!("   ✓ Context ID: {}\n", ctx_id);

        // 2. Configure VM (1 vCPU, 256 MiB for nginx)
        println!("2. Configuring VM...");
        let ret = krun_set_vm_config(ctx_id, 1, 256);
        if ret != 0 {
            eprintln!("Failed to set VM config: {}", ret);
            krun_free_ctx(ctx_id);
            std::process::exit(1);
        }
        println!("   ✓ 1 vCPU, 256 MiB RAM\n");

        // 3. Set kernel
        println!("3. Setting kernel...");
        let kernel = CString::new(kernel_path.as_str()).unwrap();
        let cmdline = CString::new("console=ttyS0 root=/dev/vda rw quiet").unwrap();
        let ret = krun_set_kernel(
            ctx_id,
            kernel.as_ptr(),
            KRUN_KERNEL_FORMAT_ELF,
            std::ptr::null(),
            cmdline.as_ptr(),
        );
        if ret != 0 {
            eprintln!("Failed to set kernel: {}", ret);
            krun_free_ctx(ctx_id);
            std::process::exit(1);
        }
        println!("   ✓ Kernel configured\n");

        // 4. Set root filesystem
        println!("4. Setting root filesystem...");
        let root = CString::new(rootfs_path).unwrap();
        let ret = krun_set_root(ctx_id, root.as_ptr());
        if ret != 0 {
            eprintln!("Warning: krun_set_root returned {}", ret);
        }
        println!("   ✓ Root: {}\n", rootfs_path);

        // 5. Configure networking
        println!("5. Configuring network...");
        let iface = CString::new("eth0").unwrap();
        let mac: [u8; 6] = [0x52, 0x54, 0x00, 0x12, 0x34, 0x56];
        // Use null TCP backend (disconnected device) for now
        let ret = krun_add_net_tcp(ctx_id, iface.as_ptr(), mac.as_ptr(), std::ptr::null());
        if ret != 0 {
            eprintln!("Warning: krun_add_net_tcp returned {}", ret);
        }
        println!("   ✓ Network: virtio-net device (disconnected)\n");

        // 6. Configure console
        println!("6. Configuring console...");
        let console_id = CString::new("ttyS0").unwrap();
        krun_set_kernel_console(ctx_id, console_id.as_ptr());
        krun_add_serial_console_default(ctx_id, 0, 1);
        println!("   ✓ Serial console on ttyS0\n");

        // 7. Set workload
        println!("7. Setting workload...");
        let workdir = CString::new("/").unwrap();
        krun_set_workdir(ctx_id, workdir.as_ptr());

        // Try to run nginx if available, otherwise just a shell
        let exec_path = CString::new("/usr/sbin/nginx").unwrap();
        let arg0 = CString::new("nginx").unwrap();
        let arg1 = CString::new("-g").unwrap();
        let arg2 = CString::new("daemon off;").unwrap();
        let argv = [
            arg0.as_ptr(),
            arg1.as_ptr(),
            arg2.as_ptr(),
            std::ptr::null(),
        ];

        let ret = krun_set_exec(ctx_id, exec_path.as_ptr(), argv.as_ptr(), std::ptr::null());
        if ret != 0 {
            println!("   Note: nginx not found, will try /bin/sh");
            let shell = CString::new("/bin/sh").unwrap();
            let sh_arg0 = CString::new("sh").unwrap();
            let sh_arg1 = CString::new("-c").unwrap();
            let sh_arg2 = CString::new("echo 'VM started successfully!'; sleep 5").unwrap();
            let sh_argv = [
                sh_arg0.as_ptr(),
                sh_arg1.as_ptr(),
                sh_arg2.as_ptr(),
                std::ptr::null(),
            ];
            krun_set_exec(ctx_id, shell.as_ptr(), sh_argv.as_ptr(), std::ptr::null());
        }
        println!("   ✓ Workload configured\n");

        // 8. Start VM
        println!("8. Starting VM...");
        println!("   Note: This will block. Press Ctrl+C to stop.\n");
        println!("{}", "=".repeat(60));
        println!();

        // This does not return on success
        let ret = krun_start_enter(ctx_id);

        // If we get here, something went wrong
        eprintln!("\nVM exited with code: {}", ret);
        krun_free_ctx(ctx_id);
    }
}
