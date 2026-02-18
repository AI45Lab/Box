//! Build script for a3s-box-sdk.
//!
//! When the `embed-shim` feature is enabled:
//! - If `A3S_SHIM_BINARY_PATH` is set, use that path directly.
//! - Otherwise, compile `a3s-box-shim` in release mode and set the env var.

fn main() {
    // Only compile the shim when embed-shim feature is enabled
    if std::env::var("CARGO_FEATURE_EMBED_SHIM").is_err() {
        return;
    }

    // If the user already provided a path, use it
    if std::env::var("A3S_SHIM_BINARY_PATH").is_ok() {
        println!("cargo:rerun-if-env-changed=A3S_SHIM_BINARY_PATH");
        return;
    }

    // Compile a3s-box-shim in release mode
    let shim_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../shim");
    if !shim_dir.exists() {
        panic!(
            "Shim source directory not found at {}. \
             Either set A3S_SHIM_BINARY_PATH or ensure the shim crate exists.",
            shim_dir.display()
        );
    }

    eprintln!("Building a3s-box-shim (release)...");
    let status = std::process::Command::new("cargo")
        .args(["build", "-p", "a3s-box-shim", "--release"])
        .status()
        .expect("Failed to run cargo build for a3s-box-shim");

    if !status.success() {
        panic!("Failed to compile a3s-box-shim");
    }

    // Find the built binary in target/release
    // Walk up from CARGO_MANIFEST_DIR to find the workspace target directory
    let manifest_dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let mut search_dir = manifest_dir.as_path();

    let shim_binary = loop {
        let candidate = search_dir.join("target/release/a3s-box-shim");
        if candidate.exists() {
            break candidate;
        }
        match search_dir.parent() {
            Some(parent) => search_dir = parent,
            None => panic!(
                "Could not find compiled a3s-box-shim binary in any target/release/ directory"
            ),
        }
    };

    println!(
        "cargo:rustc-env=A3S_SHIM_BINARY_PATH={}",
        shim_binary.display()
    );
    println!("cargo:rerun-if-changed=../shim/src/main.rs");
    println!("cargo:rerun-if-changed=../shim/Cargo.toml");
    println!("cargo:rerun-if-env-changed=A3S_SHIM_BINARY_PATH");
}
