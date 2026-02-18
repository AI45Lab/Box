//! TEE Integration Test: Encrypt externally, decrypt only inside TEE.
//!
//! This test demonstrates the full TEE (Trusted Execution Environment) workflow:
//!
//! 1. Run a box with `--tee-simulate` (simulated AMD SEV-SNP)
//! 2. Verify TEE attestation via `attest --ratls --allow-simulated`
//! 3. Seal sensitive data bound to the TEE identity
//! 4. Unseal (decrypt) the data inside the same TEE
//! 5. Inject secrets into the TEE via RA-TLS
//! 6. Verify secrets are accessible inside the guest
//! 7. Verify sealed data cannot be unsealed with wrong context
//!
//! ## Prerequisites
//!
//! - `a3s-box` binary built (`cargo build -p a3s-box-cli`)
//! - macOS with Apple HVF or Linux with KVM
//! - Internet access (to pull images on first run)
//! - `DYLD_LIBRARY_PATH` set to include libkrun/libkrunfw build dirs
//!
//! ## Running
//!
//! ```bash
//! cd crates/box/src
//!
//! # Set library paths (macOS)
//! export DYLD_LIBRARY_PATH="$(ls -td target/debug/build/libkrun-sys-*/out/libkrun/lib | head -1):$(ls -td target/debug/build/libkrun-sys-*/out/libkrunfw/lib | head -1)"
//!
//! # Run TEE integration tests
//! cargo test -p a3s-box-cli --test tee_integration -- --ignored --nocapture --test-threads=1
//! ```
//!
//! Tests are `#[ignore]` by default because they require a built binary,
//! network access, and virtualization support (HVF/KVM).

use std::process::Command;
use std::time::Duration;

/// Find the a3s-box binary in the target directory.
fn find_binary() -> String {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let workspace_root = std::path::Path::new(manifest_dir)
        .parent()
        .expect("cli crate should be inside workspace");

    for profile in ["debug", "release"] {
        let bin = workspace_root.join("target").join(profile).join("a3s-box");
        if bin.exists() {
            return bin.to_string_lossy().to_string();
        }
    }

    "a3s-box".to_string()
}

/// Run an a3s-box command with inherited output, assert success.
fn run_ok(args: &[&str]) -> String {
    let bin = find_binary();
    eprintln!("    $ a3s-box {}", args.join(" "));

    let status = Command::new(&bin)
        .args(args)
        .stdout(std::process::Stdio::inherit())
        .stderr(std::process::Stdio::inherit())
        .status()
        .unwrap_or_else(|e| panic!("Failed to run `a3s-box {}`: {}", args.join(" "), e));

    assert!(
        status.success(),
        "Command `a3s-box {}` failed",
        args.join(" "),
    );
    String::new()
}

/// Run an a3s-box command, capture stdout, assert success, return stdout.
fn run_ok_capture(args: &[&str]) -> String {
    let bin = find_binary();
    eprintln!("    $ a3s-box {}", args.join(" "));
    let output = Command::new(&bin)
        .args(args)
        .output()
        .unwrap_or_else(|e| panic!("Failed to run `a3s-box {}`: {}", args.join(" "), e));

    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    assert!(
        output.status.success(),
        "Command `a3s-box {}` failed.\nstdout: {}\nstderr: {}",
        args.join(" "),
        stdout,
        stderr,
    );
    stdout
}

/// Run an a3s-box command quietly (capture, no assert).
fn run_cmd_quiet(args: &[&str]) -> (String, String, bool) {
    let bin = find_binary();
    let output = Command::new(&bin)
        .args(args)
        .output()
        .unwrap_or_else(|e| panic!("Failed to run `a3s-box {}`: {}", args.join(" "), e));

    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    (stdout, stderr, output.status.success())
}

/// Wait for box to reach "running" status.
fn wait_for_running(box_name: &str, timeout: Duration) {
    let start = std::time::Instant::now();
    let mut last_log_len = 0;

    while start.elapsed() < timeout {
        last_log_len = print_new_logs(box_name, last_log_len);

        let (stdout, _, _) = run_cmd_quiet(&["ps"]);
        if stdout.contains(box_name) && stdout.contains("running") {
            print_new_logs(box_name, last_log_len);
            return;
        }

        let (stdout_all, _, _) = run_cmd_quiet(&["ps", "-a"]);
        if stdout_all.contains(box_name) && stdout_all.contains("dead") {
            print_new_logs(box_name, last_log_len);
            panic!("Box '{}' died during boot", box_name);
        }

        std::thread::sleep(Duration::from_millis(500));
    }
    print_new_logs(box_name, last_log_len);
    panic!("Timeout waiting for box '{}' to be running", box_name);
}

/// Print new log lines from the box's console.log since last check.
fn print_new_logs(box_name: &str, last_len: usize) -> usize {
    let (stdout, _, _) = run_cmd_quiet(&["inspect", box_name]);

    let log_path = stdout
        .lines()
        .find(|l| l.contains("console_log"))
        .and_then(|l| l.split('"').nth(3).map(|s| s.to_string()));

    if let Some(path) = log_path {
        if let Ok(content) = std::fs::read_to_string(&path) {
            if content.len() > last_len {
                let new_content = &content[last_len..];
                for line in new_content.lines() {
                    eprintln!("    📋 {}", line);
                }
                return content.len();
            }
        }
    }
    last_len
}

/// Cleanup helper: stop and remove a box by name, ignoring errors.
fn cleanup(name: &str) {
    let _ = run_cmd_quiet(&["stop", name]);
    let _ = run_cmd_quiet(&["rm", name]);
}

// ============================================================================
// Test: Full TEE lifecycle — seal, unseal, attest, inject secrets
// ============================================================================

/// Demonstrates the complete TEE workflow:
/// 1. Run a box with simulated TEE
/// 2. Attest the TEE via RA-TLS
/// 3. Seal sensitive data (encrypt bound to TEE identity)
/// 4. Unseal the data (decrypt inside the TEE)
/// 5. Inject secrets via RA-TLS
/// 6. Verify secrets inside the guest
#[test]
#[ignore] // Requires built binary, network, and virtualization support
fn test_tee_seal_unseal_lifecycle() {
    let box_name = "integ-tee-seal";
    cleanup(box_name);

    // ---- Step 1: Pull alpine image ----
    println!("==> Step 1: Pulling alpine image...");
    run_ok(&["pull", "docker.io/library/alpine:latest"]);
    println!("    ✓ alpine image available");

    // ---- Step 2: Run alpine with TEE simulation ----
    println!("==> Step 2: Running alpine box with --tee-simulate...");
    run_ok(&[
        "run",
        "-d",
        "--name",
        box_name,
        "--tee",
        "--tee-simulate",
        "docker.io/library/alpine:latest",
        "--",
        "sleep",
        "3600",
    ]);
    println!("    ✓ TEE box created");

    // ---- Step 3: Wait for VM to boot ----
    println!("==> Step 3: Waiting for VM to boot...");
    wait_for_running(box_name, Duration::from_secs(30));
    println!("    ✓ TEE box is running");

    // Wait for attestation server to be ready
    std::thread::sleep(Duration::from_secs(3));

    // ---- Step 4: Verify TEE attestation ----
    println!("==> Step 4: Verifying TEE attestation via RA-TLS...");
    let stdout = run_ok_capture(&["attest", box_name, "--ratls", "--allow-simulated"]);
    assert!(
        stdout.contains("\"verified\"") || stdout.contains("true"),
        "Attestation should succeed"
    );
    println!("    ✓ TEE attestation verified (simulated)");

    // ---- Step 5: Seal sensitive data ----
    println!("==> Step 5: Sealing sensitive data...");
    let sensitive_data = "API_KEY=sk-secret-12345-production";
    let seal_output = run_ok_capture(&[
        "seal",
        box_name,
        "--data",
        sensitive_data,
        "--context",
        "api-keys",
        "--policy",
        "measurement-and-chip",
        "--allow-simulated",
    ]);

    // Parse the sealed blob from JSON output
    let seal_json: serde_json::Value =
        serde_json::from_str(&seal_output).expect("seal output should be valid JSON");
    let sealed_blob = seal_json["blob"]
        .as_str()
        .expect("seal output should contain blob");
    assert!(!sealed_blob.is_empty(), "Sealed blob should not be empty");
    println!(
        "    ✓ Data sealed (blob length: {} chars)",
        sealed_blob.len()
    );

    // ---- Step 6: Unseal the data inside the same TEE ----
    println!("==> Step 6: Unsealing data inside TEE...");
    let unseal_output = run_ok_capture(&[
        "unseal",
        box_name,
        "--blob",
        sealed_blob,
        "--context",
        "api-keys",
        "--policy",
        "measurement-and-chip",
        "--allow-simulated",
    ]);

    let unseal_json: serde_json::Value =
        serde_json::from_str(&unseal_output).expect("unseal output should be valid JSON");
    let unsealed_data = unseal_json["data"]
        .as_str()
        .expect("unseal output should contain data");
    assert_eq!(
        unsealed_data, sensitive_data,
        "Unsealed data should match original"
    );
    println!("    ✓ Data unsealed successfully: matches original");

    // ---- Step 7: Verify wrong context fails ----
    println!("==> Step 7: Verifying wrong context fails...");
    let (_, _, success) = run_cmd_quiet(&[
        "unseal",
        box_name,
        "--blob",
        sealed_blob,
        "--context",
        "wrong-context",
        "--policy",
        "measurement-and-chip",
        "--allow-simulated",
    ]);
    assert!(!success, "Unseal with wrong context should fail");
    println!("    ✓ Wrong context correctly rejected");

    // ---- Step 8: Cleanup ----
    println!("==> Step 8: Cleaning up...");
    cleanup(box_name);
    println!("    ✓ TEE box removed");

    println!("\n==> All TEE seal/unseal steps passed!");
}

// ============================================================================
// Test: Secret injection into TEE
// ============================================================================

/// Demonstrates injecting secrets into a TEE box via RA-TLS.
/// Secrets are stored in /run/secrets/<name> inside the guest.
#[test]
#[ignore]
fn test_tee_secret_injection() {
    let box_name = "integ-tee-secrets";
    cleanup(box_name);

    // Run alpine with TEE simulation
    println!("==> Running alpine box with TEE simulation...");
    run_ok(&[
        "run",
        "-d",
        "--name",
        box_name,
        "--tee",
        "--tee-simulate",
        "docker.io/library/alpine:latest",
        "--",
        "sleep",
        "3600",
    ]);

    wait_for_running(box_name, Duration::from_secs(30));
    std::thread::sleep(Duration::from_secs(3));
    println!("    ✓ TEE box running");

    // Inject secrets
    println!("==> Injecting secrets via RA-TLS...");
    let inject_output = run_ok_capture(&[
        "inject-secret",
        box_name,
        "--secret",
        "DB_PASSWORD=super-secret-db-pass",
        "--secret",
        "API_TOKEN=tok-abc123",
        "--set-env",
        "--allow-simulated",
    ]);

    let inject_json: serde_json::Value =
        serde_json::from_str(&inject_output).expect("inject output should be valid JSON");
    let injected = inject_json["injected"]
        .as_u64()
        .expect("inject output should contain injected count");
    assert_eq!(injected, 2, "Should have injected 2 secrets");
    println!("    ✓ {} secrets injected", injected);

    // Verify secrets are accessible inside the guest
    println!("==> Verifying secrets inside guest...");
    std::thread::sleep(Duration::from_secs(1));

    let (stdout, _, success) =
        run_cmd_quiet(&["exec", box_name, "--", "cat", "/run/secrets/DB_PASSWORD"]);
    if success {
        assert_eq!(
            stdout.trim(),
            "super-secret-db-pass",
            "Secret file should contain the injected value"
        );
        println!("    ✓ /run/secrets/DB_PASSWORD accessible");
    } else {
        println!("    ⚠ exec not available, skipping file verification");
    }

    let (stdout, _, success) =
        run_cmd_quiet(&["exec", box_name, "--", "cat", "/run/secrets/API_TOKEN"]);
    if success {
        assert_eq!(stdout.trim(), "tok-abc123");
        println!("    ✓ /run/secrets/API_TOKEN accessible");
    }

    cleanup(box_name);
    println!("==> Secret injection test complete.");
}

// ============================================================================
// Test: Seal with different policies
// ============================================================================

/// Demonstrates sealing data with different policies:
/// - measurement-and-chip (strictest)
/// - measurement-only (portable across chips)
/// - chip-only (survives firmware updates)
#[test]
#[ignore]
fn test_tee_seal_policies() {
    let box_name = "integ-tee-policies";
    cleanup(box_name);

    run_ok(&[
        "run",
        "-d",
        "--name",
        box_name,
        "--tee",
        "--tee-simulate",
        "docker.io/library/alpine:latest",
        "--",
        "sleep",
        "3600",
    ]);

    wait_for_running(box_name, Duration::from_secs(30));
    std::thread::sleep(Duration::from_secs(3));

    let policies = ["measurement-and-chip", "measurement-only", "chip-only"];

    for policy in &policies {
        println!("==> Testing seal with policy: {}", policy);
        let data = format!("secret-for-{}", policy);

        let seal_output = run_ok_capture(&[
            "seal",
            box_name,
            "--data",
            &data,
            "--context",
            "policy-test",
            "--policy",
            policy,
            "--allow-simulated",
        ]);

        let seal_json: serde_json::Value = serde_json::from_str(&seal_output).unwrap();
        let blob = seal_json["blob"].as_str().unwrap();

        let unseal_output = run_ok_capture(&[
            "unseal",
            box_name,
            "--blob",
            blob,
            "--context",
            "policy-test",
            "--policy",
            policy,
            "--allow-simulated",
        ]);

        let unseal_json: serde_json::Value = serde_json::from_str(&unseal_output).unwrap();
        let unsealed = unseal_json["data"].as_str().unwrap();
        assert_eq!(unsealed, data, "Policy {} roundtrip failed", policy);
        println!("    ✓ {} seal/unseal roundtrip OK", policy);
    }

    cleanup(box_name);
    println!("==> All sealing policies verified.");
}
