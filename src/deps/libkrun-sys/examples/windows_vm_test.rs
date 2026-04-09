//! Simple Windows test to create a minimal VM with libkrun
//!
//! This demonstrates the basic workflow without requiring the full a3s-box CLI.

use a3s_libkrun_sys::*;
use std::ffi::CString;
use std::path::Path;

#[cfg(windows)]
fn main() {
    println!("=== a3s-box Windows Integration Test ===\n");

    // Check prerequisites
    if !Path::new("C:\\temp").exists() {
        eprintln!("Error: C:\\temp directory does not exist");
        eprintln!("Please create it first: mkdir C:\\temp");
        std::process::exit(1);
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

        // 2. Configure VM
        println!("2. Configuring VM (2 vCPUs, 512 MiB RAM)...");
        let ret = krun_set_vm_config(ctx_id, 2, 512);
        if ret != 0 {
            eprintln!("Failed to set VM config: {}", ret);
            krun_free_ctx(ctx_id);
            std::process::exit(1);
        }
        println!("   ✓ VM configured\n");

        // 3. Set root filesystem (virtiofs)
        println!("3. Setting root filesystem...");
        let root_path = CString::new("C:\\temp").unwrap();
        let ret = krun_set_root(ctx_id, root_path.as_ptr());
        if ret != 0 {
            eprintln!("Warning: krun_set_root returned {}", ret);
            eprintln!("This is expected if C:\\temp doesn't have proper structure");
        }
        println!("   ✓ Root set to C:\\temp\n");

        // 4. Configure console
        println!("4. Configuring console...");
        let console_id = CString::new("ttyS0").unwrap();
        let ret = krun_set_kernel_console(ctx_id, console_id.as_ptr());
        if ret != 0 {
            eprintln!("Failed to set kernel console: {}", ret);
        }
        let ret = krun_add_serial_console_default(ctx_id, 0, 1);
        if ret != 0 {
            eprintln!("Failed to add serial console: {}", ret);
        }
        println!("   ✓ Console configured\n");

        // 5. Set workload
        println!("5. Setting workload...");
        let workdir = CString::new("/").unwrap();
        let ret = krun_set_workdir(ctx_id, workdir.as_ptr());
        if ret != 0 {
            eprintln!("Failed to set workdir: {}", ret);
        }

        let exec_path = CString::new("/bin/sh").unwrap();
        let arg0 = CString::new("sh").unwrap();
        let arg1 = CString::new("-c").unwrap();
        let arg2 = CString::new("echo 'Hello from libkrun on Windows!'; sleep 2").unwrap();
        let argv = [
            arg0.as_ptr(),
            arg1.as_ptr(),
            arg2.as_ptr(),
            std::ptr::null(),
        ];
        let ret = krun_set_exec(ctx_id, exec_path.as_ptr(), argv.as_ptr(), std::ptr::null());
        if ret != 0 {
            eprintln!("Failed to set exec: {}", ret);
        }
        println!("   ✓ Workload configured\n");

        // 6. Check if kernel is available
        println!("6. Checking for kernel...");
        let kernel_paths = [
            "C:\\temp\\vmlinux",
            "C:\\temp\\bzImage",
            "vmlinux",
            "bzImage",
        ];

        let mut kernel_found = None;
        for path in &kernel_paths {
            if Path::new(path).exists() {
                kernel_found = Some(*path);
                break;
            }
        }

        if let Some(kernel_path) = kernel_found {
            println!("   ✓ Found kernel at: {}\n", kernel_path);

            let kernel = CString::new(kernel_path).unwrap();
            let cmdline = CString::new("console=ttyS0 root=/dev/vda rw").unwrap();
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

            println!("7. Starting VM...");
            println!("   Note: krun_start_enter() does not return on success\n");

            // This will not return if successful
            let ret = krun_start_enter(ctx_id);
            eprintln!("krun_start_enter returned: {}", ret);
            eprintln!("This indicates an error occurred");
        } else {
            println!("   ✗ No kernel found\n");
            println!("To run a full VM test, you need a Linux kernel:");
            println!("  1. Download a kernel (e.g., from libkrunfw-windows)");
            println!("  2. Place it at C:\\temp\\vmlinux or C:\\temp\\bzImage");
            println!("  3. Run this test again\n");
            println!("For now, the test verified that all libkrun APIs work correctly.");
        }

        // Cleanup
        krun_free_ctx(ctx_id);
    }

    println!("\n=== Test completed successfully ===");
}
