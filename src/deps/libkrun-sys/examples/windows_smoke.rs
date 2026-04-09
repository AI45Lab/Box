//! Windows smoke test for a3s-libkrun-sys
//!
//! Verifies that krun.dll can be loaded and basic API calls work.
//!
//! Run with:
//!   cargo run --target x86_64-pc-windows-msvc --example windows_smoke

use a3s_libkrun_sys::*;

#[cfg(windows)]
fn main() {
    println!("=== a3s-libkrun-sys Windows Smoke Test ===\n");

    unsafe {
        // Test 1: Set log level
        println!("Test 1: krun_set_log_level");
        let ret = krun_set_log_level(KRUN_LOG_LEVEL_INFO);
        assert_eq!(ret, 0, "krun_set_log_level failed: {}", ret);
        println!("  ✓ krun_set_log_level(INFO) = {}\n", ret);

        // Test 2: Create context
        println!("Test 2: krun_create_ctx");
        let ctx = krun_create_ctx();
        assert!(ctx >= 0, "krun_create_ctx failed: {}", ctx);
        let ctx_id = ctx as u32;
        println!("  ✓ krun_create_ctx() = {}\n", ctx_id);

        // Test 3: Set VM config
        println!("Test 3: krun_set_vm_config");
        let ret = krun_set_vm_config(ctx_id, 2, 512);
        assert_eq!(ret, 0, "krun_set_vm_config failed: {}", ret);
        println!("  ✓ krun_set_vm_config(2 vCPUs, 512 MiB) = {}\n", ret);

        // Test 4: Disable implicit console
        println!("Test 4: krun_disable_implicit_console");
        let ret = krun_disable_implicit_console(ctx_id);
        assert_eq!(ret, 0, "krun_disable_implicit_console failed: {}", ret);
        println!("  ✓ krun_disable_implicit_console() = {}\n", ret);

        // Test 5: Disable implicit vsock
        println!("Test 5: krun_disable_implicit_vsock");
        let ret = krun_disable_implicit_vsock(ctx_id);
        assert_eq!(ret, 0, "krun_disable_implicit_vsock failed: {}", ret);
        println!("  ✓ krun_disable_implicit_vsock() = {}\n", ret);

        // Test 6: Free context
        println!("Test 6: krun_free_ctx");
        let ret = krun_free_ctx(ctx_id);
        assert_eq!(ret, 0, "krun_free_ctx failed: {}", ret);
        println!("  ✓ krun_free_ctx() = {}\n", ret);
    }

    println!("=== All tests passed! ===");
}
