// Allow unused code - these are used conditionally based on platform and build mode
#![allow(dead_code)]

use std::collections::HashMap;
use std::env;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

// ============================================================================
// Prebuilt libkrunfw configuration
// Using boxlite-ai/libkrunfw releases (fork with prebuilt releases)
// ============================================================================

// macOS: Download prebuilt kernel.c, compile locally to .dylib
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
const LIBKRUNFW_PREBUILT_URL: &str =
    "https://github.com/boxlite-ai/libkrunfw/releases/download/v5.1.0/libkrunfw-prebuilt-aarch64.tgz";
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
const LIBKRUNFW_SHA256: &str = "2b2801d2e414140d8d0a30d7e30a011077b7586eabbbecdca42aea804b59de8b";

// Linux x86_64: Download pre-compiled .so directly (no build needed)
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
const LIBKRUNFW_SO_URL: &str =
    "https://github.com/boxlite-ai/libkrunfw/releases/download/v5.1.0/libkrunfw-x86_64.tgz";
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
const LIBKRUNFW_SHA256: &str = "faca64a3581ce281498b8ae7eccc6bd0da99b167984f9ee39c47754531d4b37d";

// Linux aarch64: Download pre-compiled .so directly (no build needed)
#[cfg(all(target_os = "linux", target_arch = "aarch64"))]
const LIBKRUNFW_SO_URL: &str =
    "https://github.com/boxlite-ai/libkrunfw/releases/download/v5.1.0/libkrunfw-aarch64.tgz";
#[cfg(all(target_os = "linux", target_arch = "aarch64"))]
const LIBKRUNFW_SHA256: &str = "e254bc3fb07b32e26a258d9958967b2f22eb6c3136cfedf358c332308b6d35ea";

// libkrun build features (NET=1 BLK=1 enables network and block device support)
// Note: TEE support (krun_set_tee_config_file) is loaded via dlsym at runtime
// since the `tee` feature in libkrun requires amd-sev or tdx to compile properly.
const LIBKRUN_BUILD_FEATURES: &[(&str, &str)] = &[("NET", "1"), ("BLK", "1")];

// Library directory name differs by platform
#[cfg(target_os = "macos")]
const LIB_DIR: &str = "lib";
#[cfg(target_os = "linux")]
const LIB_DIR: &str = "lib64";
#[cfg(not(any(target_os = "macos", target_os = "linux")))]
const LIB_DIR: &str = "lib";

fn main() {
    // Rebuild if vendored sources change
    println!("cargo:rerun-if-changed=vendor/libkrun");

    // Check for stub mode (for CI linting without building)
    // Set A3S_DEPS_STUB=1 to skip building and emit stub link directives
    if env::var("A3S_DEPS_STUB").is_ok() {
        println!("cargo:warning=A3S_DEPS_STUB mode: skipping libkrun build");
        println!("cargo:rustc-link-lib=dylib=krun");
        println!("cargo:LIBKRUN_A3S_DEP=/nonexistent");
        println!("cargo:LIBKRUNFW_A3S_DEP=/nonexistent");
        return;
    }

    // Try to find system-installed libkrun first (unless A3S_BUILD_LIBKRUN is set)
    if env::var("A3S_BUILD_LIBKRUN").is_err() {
        if let Ok(lib_dir) = find_system_libkrun() {
            println!(
                "cargo:warning=Using system-installed libkrun from {}",
                lib_dir.display()
            );
            configure_linking(&lib_dir, &lib_dir);
            return;
        }
        if let Some((libkrun_dir, libkrunfw_dir)) = find_cached_libkrun() {
            println!(
                "cargo:warning=Using cached libkrun from {} and libkrunfw from {}",
                libkrun_dir.display(),
                libkrunfw_dir.display()
            );
            configure_linking(&libkrun_dir, &libkrunfw_dir);
            return;
        }
    } else {
        println!("cargo:warning=A3S_BUILD_LIBKRUN set: forcing build from source");
    }

    // Fall back to building from source (with prebuilt libkrunfw)
    build();
}

fn find_cached_libkrun() -> Option<(PathBuf, PathBuf)> {
    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").ok()?);
    let workspace_root = manifest_dir.join("../..").canonicalize().ok()?;
    let target_roots = candidate_target_roots(&workspace_root);

    let mut libkrun_candidates = vec![
        workspace_root.join("deps/libkrun-sys/vendor/libkrun/target/release"),
        workspace_root.join("deps/libkrun-sys/vendor/libkrun/target/release/deps"),
        workspace_root.join("target/release"),
        workspace_root.join("target/release/deps"),
        workspace_root.join("target/debug"),
        workspace_root.join("target/debug/deps"),
    ];
    for target_root in &target_roots {
        libkrun_candidates.push(target_root.join("release"));
        libkrun_candidates.push(target_root.join("release/deps"));
        libkrun_candidates.push(target_root.join("debug"));
        libkrun_candidates.push(target_root.join("debug/deps"));
    }

    let libkrun_dir = libkrun_candidates
        .into_iter()
        .find(|dir| has_library(dir, "libkrun"))?;
    let libkrunfw_dir = target_roots
        .into_iter()
        .find_map(find_libkrunfw_under_target)?;
    #[cfg(target_os = "macos")]
    ensure_macos_lib_alias(&libkrun_dir, "libkrun.dylib", "libkrun.1.dylib");
    Some((libkrun_dir, libkrunfw_dir))
}

fn candidate_target_roots(workspace_root: &Path) -> Vec<PathBuf> {
    let mut roots = Vec::new();

    if let Ok(out_dir) = env::var("OUT_DIR") {
        let out_dir = PathBuf::from(out_dir);
        if let Some(target_root) = target_root_from_out_dir(&out_dir) {
            roots.push(target_root);
        }
    }

    if let Some(target_dir) = env::var_os("CARGO_TARGET_DIR").map(PathBuf::from) {
        roots.push(target_dir);
    }

    roots.push(workspace_root.join("target"));

    let mut unique = Vec::new();
    for root in roots {
        if root.exists() && !unique.iter().any(|existing: &PathBuf| existing == &root) {
            unique.push(root);
        }
    }
    unique
}

fn target_root_from_out_dir(out_dir: &Path) -> Option<PathBuf> {
    for ancestor in out_dir.ancestors() {
        if ancestor.file_name().is_some_and(|name| name == "build") {
            return ancestor.parent()?.parent().map(Path::to_path_buf);
        }
    }
    None
}

fn find_libkrunfw_under_target(target_root: PathBuf) -> Option<PathBuf> {
    let direct_candidates = [
        target_root.join("release/build"),
        target_root.join("debug/build"),
        target_root.join("release"),
        target_root.join("debug"),
    ];

    for candidate in direct_candidates {
        if let Some(dir) = find_libkrunfw_dir(&candidate) {
            return Some(dir);
        }
    }

    None
}

fn find_libkrunfw_dir(root: &Path) -> Option<PathBuf> {
    if has_library(root, "libkrunfw") {
        return Some(root.to_path_buf());
    }

    for entry in fs::read_dir(root).ok()? {
        let path = entry.ok()?.path();
        if !path.is_dir() {
            continue;
        }
        let candidate = path.join("out").join("libkrunfw").join(LIB_DIR);
        if has_library(&candidate, "libkrunfw") {
            return Some(candidate);
        }
    }

    None
}

#[cfg(target_os = "macos")]
fn ensure_macos_lib_alias(dir: &Path, source: &str, alias: &str) {
    let source_path = dir.join(source);
    let alias_path = dir.join(alias);
    if !source_path.exists() || alias_path.exists() {
        return;
    }
    std::os::unix::fs::symlink(source, &alias_path).ok();
}

/// Try to find system-installed libkrun via pkg-config or common paths.
fn find_system_libkrun() -> Result<PathBuf, String> {
    if let Ok(lib) = pkg_config::Config::new()
        .atleast_version("1.0")
        .probe("libkrun")
    {
        if let Some(path) = lib.link_paths.first() {
            return Ok(path.clone());
        }
    }

    #[cfg(target_os = "macos")]
    let common_paths = ["/opt/homebrew/lib", "/usr/local/lib", "/usr/lib"];
    #[cfg(not(target_os = "macos"))]
    let common_paths = ["/usr/local/lib", "/usr/lib", "/usr/lib64"];

    for path in common_paths {
        let lib_path = Path::new(path);
        if has_library(lib_path, "libkrun") {
            return Ok(lib_path.to_path_buf());
        }
    }

    Err("libkrun not found in system paths".to_string())
}

/// Returns libkrun build environment with features enabled.
fn libkrun_build_env(libkrunfw_install: &Path) -> HashMap<String, String> {
    let mut env = HashMap::new();
    env.insert(
        "PKG_CONFIG_PATH".to_string(),
        format!("{}/{}/pkgconfig", libkrunfw_install.display(), LIB_DIR),
    );
    for (key, value) in LIBKRUN_BUILD_FEATURES {
        env.insert(key.to_string(), value.to_string());
    }
    env
}

/// Verifies vendored libkrun source exists.
fn verify_libkrun_source(manifest_dir: &Path) {
    let libkrun_src = manifest_dir.join("vendor/libkrun");
    if !libkrun_src.exists() {
        eprintln!("ERROR: Vendored libkrun source not found");
        eprintln!();
        eprintln!("Initialize git submodule:");
        eprintln!("  git submodule update --init vendor/libkrun");
        std::process::exit(1);
    }
}

/// Runs a command and panics with a helpful message if it fails.
fn run_command(cmd: &mut Command, description: &str) {
    let status = cmd
        .status()
        .unwrap_or_else(|e| panic!("Failed to execute {}: {}", description, e));

    if !status.success() {
        panic!("{} failed with exit code: {:?}", description, status.code());
    }
}

/// Checks if a directory contains any library file matching the given prefix.
fn has_library(dir: &Path, prefix: &str) -> bool {
    let extensions = if cfg!(target_os = "macos") {
        vec!["dylib"]
    } else if cfg!(target_os = "linux") {
        vec!["so"]
    } else {
        vec!["dll"]
    };

    dir.read_dir()
        .ok()
        .map(|entries| {
            entries.filter_map(Result::ok).any(|entry| {
                let filename = entry.file_name().to_string_lossy().to_string();
                filename.starts_with(prefix)
                    && extensions
                        .iter()
                        .any(|ext| entry.path().extension().is_some_and(|e| e == *ext))
            })
        })
        .unwrap_or(false)
}

/// Creates a make command with common configuration.
fn make_command(
    source_dir: &Path,
    install_dir: &Path,
    extra_env: &HashMap<String, String>,
) -> Command {
    let mut cmd = Command::new("make");
    cmd.stdout(Stdio::inherit());
    cmd.stderr(Stdio::inherit());
    cmd.args(["-j", &num_cpus::get().to_string()])
        .arg("MAKEFLAGS=")
        .env("PREFIX", install_dir)
        .current_dir(source_dir);

    for (key, value) in extra_env {
        cmd.env(key, value);
    }

    cmd
}

/// Builds a library using Make with the specified parameters.
fn build_with_make(
    source_dir: &Path,
    install_dir: &Path,
    lib_name: &str,
    extra_env: HashMap<String, String>,
) {
    println!("cargo:warning=Building {} from source...", lib_name);

    std::fs::create_dir_all(install_dir)
        .unwrap_or_else(|e| panic!("Failed to create install directory: {}", e));

    let mut make_cmd = make_command(source_dir, install_dir, &extra_env);
    run_command(&mut make_cmd, &format!("make {}", lib_name));

    let mut install_cmd = make_command(source_dir, install_dir, &extra_env);
    install_cmd.arg("install");
    run_command(&mut install_cmd, &format!("make install {}", lib_name));
}

/// Configure linking for libkrun.
fn configure_linking(libkrun_dir: &Path, libkrunfw_dir: &Path) {
    println!("cargo:rustc-link-search=native={}", libkrun_dir.display());
    println!("cargo:rustc-link-lib=dylib=krun");
    #[cfg(target_os = "macos")]
    {
        println!("cargo:rustc-link-arg=-Wl,-rpath,{}", libkrun_dir.display());
        println!(
            "cargo:rustc-link-arg=-Wl,-rpath,{}",
            libkrunfw_dir.display()
        );
    }

    println!("cargo:LIBKRUN_A3S_DEP={}", libkrun_dir.display());
    println!("cargo:LIBKRUNFW_A3S_DEP={}", libkrunfw_dir.display());
}

/// Downloads a file from URL to the specified path.
fn download_file(url: &str, dest: &Path) -> io::Result<()> {
    println!("cargo:warning=Downloading {}...", url);

    let output = Command::new("curl")
        .args(["-fsSL", "-o", dest.to_str().unwrap(), url])
        .output()?;

    if !output.status.success() {
        return Err(io::Error::other(format!(
            "curl failed: {}",
            String::from_utf8_lossy(&output.stderr)
        )));
    }

    Ok(())
}

/// Verifies SHA256 checksum of a file.
fn verify_sha256(file: &Path, expected: &str) -> io::Result<()> {
    let actual = if cfg!(target_os = "windows") {
        // PowerShell is available on all modern Windows
        let script = format!(
            "(Get-FileHash -Algorithm SHA256 -LiteralPath '{}').Hash.ToLower()",
            file.display()
        );
        let output = Command::new("powershell.exe")
            .args(["-NoProfile", "-Command", &script])
            .output()?;
        if !output.status.success() {
            return Err(io::Error::other("PowerShell Get-FileHash failed"));
        }
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    } else {
        let (cmd, args): (&str, Vec<&str>) = if cfg!(target_os = "linux") {
            ("sha256sum", vec![file.to_str().unwrap()])
        } else {
            ("shasum", vec!["-a", "256", file.to_str().unwrap()])
        };
        let output = Command::new(cmd).args(&args).output()?;
        if !output.status.success() {
            return Err(io::Error::other(format!("{} failed", cmd)));
        }
        String::from_utf8_lossy(&output.stdout)
            .split_whitespace()
            .next()
            .unwrap_or("")
            .to_string()
    };

    if actual != expected {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("SHA256 mismatch: expected {}, got {}", expected, actual),
        ));
    }

    println!("cargo:warning=SHA256 verified: {}", expected);
    Ok(())
}

/// Extracts a tarball to the specified directory.
fn extract_tarball(tarball: &Path, dest: &Path) -> io::Result<()> {
    fs::create_dir_all(dest)?;

    let status = Command::new("tar")
        .args([
            "-xzf",
            tarball.to_str().unwrap(),
            "-C",
            dest.to_str().unwrap(),
        ])
        .status()?;

    if !status.success() {
        return Err(io::Error::other("tar extraction failed"));
    }

    Ok(())
}

/// Fixes the install_name on macOS to use @rpath.
#[cfg(target_os = "macos")]
fn fix_install_name(lib_name: &str, lib_path: &Path) {
    let status = Command::new("install_name_tool")
        .args([
            "-id",
            &format!("@rpath/{}", lib_name),
            lib_path.to_str().unwrap(),
        ])
        .status()
        .expect("Failed to execute install_name_tool");

    if !status.success() {
        panic!("Failed to set install_name for {}", lib_name);
    }
}

#[cfg(target_os = "linux")]
fn fix_install_name(lib_name: &str, lib_path: &Path) {
    let lib_path_str = lib_path.to_str().expect("Invalid library path");

    let result = Command::new("patchelf")
        .args(["--set-soname", lib_name, lib_path_str])
        .status();

    match result {
        Ok(status) if status.success() => {
            println!("cargo:warning=Fixed soname for {}", lib_name);
        }
        Ok(_) => {
            println!(
                "cargo:warning=patchelf failed for {}, continuing anyway",
                lib_name
            );
        }
        Err(_) => {
            println!(
                "cargo:warning=patchelf not found, skipping soname fix for {}",
                lib_name
            );
        }
    }
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn fix_install_name(_lib_name: &str, _lib_path: &Path) {}

/// Extract SONAME from versioned library filename
/// e.g., libkrunfw.so.4.9.0 -> Some("libkrunfw.so.4")
fn extract_major_soname(filename: &str) -> Option<String> {
    if let Some(so_pos) = filename.find(".so.") {
        let base = &filename[..so_pos + 3];
        let versions = &filename[so_pos + 4..];

        if let Some(major) = versions.split('.').next() {
            return Some(format!("{}.{}", base, major));
        }
    }
    None
}

// ============================================================================
// macOS-specific build functions
// ============================================================================

#[cfg(target_os = "macos")]
fn setup_libclang_path() {
    if env::var("LIBCLANG_PATH").is_ok() {
        return;
    }
    if Command::new("llvm-config")
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
    {
        return;
    }

    if let Ok(output) = Command::new("brew").args(["--prefix", "llvm"]).output() {
        if output.status.success() {
            let prefix = String::from_utf8_lossy(&output.stdout).trim().to_string();
            let lib_path = format!("{}/lib", prefix);
            if Path::new(&lib_path).join("libclang.dylib").exists() {
                env::set_var("LIBCLANG_PATH", &lib_path);
                println!("cargo:warning=Set LIBCLANG_PATH to {}", lib_path);
            }
        }
    }
}

#[cfg(target_os = "macos")]
fn fix_macos_libs(lib_dir: &Path, lib_prefix: &str) -> Result<(), String> {
    for entry in fs::read_dir(lib_dir).map_err(|e| format!("Failed to read lib dir: {}", e))? {
        let entry = entry.map_err(|e| format!("Failed to read entry: {}", e))?;
        let path = entry.path();
        let filename = path.file_name().unwrap().to_string_lossy().to_string();

        if filename.starts_with(lib_prefix) && filename.contains(".dylib") {
            let metadata = fs::symlink_metadata(&path)
                .map_err(|e| format!("Failed to get metadata: {}", e))?;

            if metadata.file_type().is_symlink() {
                continue;
            }

            fix_install_name(&filename, &path);

            let sign_status = Command::new("codesign")
                .args(["-s", "-", "--force"])
                .arg(&path)
                .status()
                .map_err(|e| format!("Failed to run codesign: {}", e))?;

            if !sign_status.success() {
                return Err(format!("codesign failed for {}", filename));
            }

            println!("cargo:warning=Fixed and signed {}", filename);
        }
    }

    Ok(())
}

/// Downloads and extracts the prebuilt libkrunfw tarball (macOS).
#[cfg(target_os = "macos")]
fn download_libkrunfw_prebuilt(out_dir: &Path) -> PathBuf {
    let tarball_path = out_dir.join("libkrunfw-prebuilt.tar.gz");
    let extract_dir = out_dir.join("libkrunfw-src");
    let src_dir = extract_dir.join("libkrunfw");

    if src_dir.join("kernel.c").exists() {
        println!("cargo:warning=Using cached libkrunfw source");
        return src_dir;
    }

    if !tarball_path.exists() {
        download_file(LIBKRUNFW_PREBUILT_URL, &tarball_path)
            .unwrap_or_else(|e| panic!("Failed to download libkrunfw: {}", e));

        verify_sha256(&tarball_path, LIBKRUNFW_SHA256)
            .unwrap_or_else(|e| panic!("Failed to verify libkrunfw checksum: {}", e));
    }

    if extract_dir.exists() {
        fs::remove_dir_all(&extract_dir).ok();
    }
    extract_tarball(&tarball_path, &extract_dir)
        .unwrap_or_else(|e| panic!("Failed to extract libkrunfw: {}", e));

    println!("cargo:warning=Extracted libkrunfw to {}", src_dir.display());
    src_dir
}

// Windows: prebuilt krun.dll + krun.lib + libkrunfw.dll bundle
const KRUN_WINDOWS_URL: &str =
    "https://github.com/A3S-Lab/libkrun/releases/download/v1.17.5-windows/krun-windows-x64.zip";
const KRUN_WINDOWS_SHA256: &str =
    "8d5bd3d2452bb6e36973b8b57d34e16fdee0af677d32f883aa689f22708dbc2a";

/// macOS: Build libkrun from source, use prebuilt libkrunfw
#[cfg(target_os = "macos")]
fn build() {
    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let out_dir = PathBuf::from(env::var("OUT_DIR").unwrap());

    let libkrunfw_install = out_dir.join("libkrunfw");
    let libkrun_install = out_dir.join("libkrun");
    let libkrunfw_lib = libkrunfw_install.join(LIB_DIR);
    let libkrun_lib = libkrun_install.join(LIB_DIR);

    // Skip build if outputs already exist (incremental build optimization)
    if has_library(&libkrunfw_lib, "libkrunfw") && has_library(&libkrun_lib, "libkrun") {
        configure_linking(&libkrun_lib, &libkrunfw_lib);
        return;
    }

    println!("cargo:warning=Building libkrun-sys for macOS (using prebuilt libkrunfw)");

    // Verify vendored libkrun source exists (libkrunfw is downloaded)
    verify_libkrun_source(&manifest_dir);

    let libkrun_src = manifest_dir.join("vendor/libkrun");

    // Setup LIBCLANG_PATH for bindgen if needed
    setup_libclang_path();

    // 1. Download and extract prebuilt libkrunfw
    let libkrunfw_src = download_libkrunfw_prebuilt(&out_dir);

    // 2. Build libkrunfw from prebuilt source (fast, just compiles kernel.c)
    build_with_make(
        &libkrunfw_src,
        &libkrunfw_install,
        "libkrunfw",
        HashMap::new(),
    );

    // 3. Build libkrun from vendored source
    build_with_make(
        &libkrun_src,
        &libkrun_install,
        "libkrun",
        libkrun_build_env(&libkrunfw_install),
    );

    // 4. Fix install names for @rpath and re-sign
    fix_macos_libs(&libkrunfw_lib, "libkrunfw")
        .unwrap_or_else(|e| panic!("Failed to fix libkrunfw: {}", e));

    fix_macos_libs(&libkrun_lib, "libkrun")
        .unwrap_or_else(|e| panic!("Failed to fix libkrun: {}", e));

    // 5. Configure linking
    configure_linking(&libkrun_lib, &libkrunfw_lib);
}

// ============================================================================
// Linux-specific build functions
// ============================================================================

#[cfg(target_os = "linux")]
fn fix_linux_libs(lib_dir: &Path, lib_prefix: &str) -> Result<(), String> {
    for entry in fs::read_dir(lib_dir).map_err(|e| format!("Failed to read directory: {}", e))? {
        let entry = entry.map_err(|e| format!("Failed to read entry: {}", e))?;
        let path = entry.path();
        let filename = path.file_name().unwrap().to_string_lossy().to_string();

        if filename.starts_with(lib_prefix) && filename.contains(".so") {
            let metadata = fs::symlink_metadata(&path)
                .map_err(|e| format!("Failed to get metadata: {}", e))?;

            if metadata.file_type().is_symlink() {
                continue;
            }

            // For libkrunfw only: rename to major version
            if lib_prefix == "libkrunfw" {
                if let Some(soname) = extract_major_soname(&filename) {
                    if soname != filename {
                        let new_path = lib_dir.join(&soname);
                        fs::rename(&path, &new_path)
                            .map_err(|e| format!("Failed to rename file: {}", e))?;
                        println!("cargo:warning=Renamed {} to {}", filename, soname);
                        fix_install_name(&soname, &new_path);
                        continue;
                    }
                }
            }

            fix_install_name(&filename, &path);
        }
    }

    Ok(())
}

/// Downloads pre-compiled libkrunfw .so files (Linux).
#[cfg(target_os = "linux")]
fn download_libkrunfw_so(install_dir: &Path) {
    let lib_dir = install_dir.join(LIB_DIR);

    if has_library(&lib_dir, "libkrunfw") {
        println!("cargo:warning=Using cached libkrunfw.so");
        return;
    }

    fs::create_dir_all(install_dir)
        .unwrap_or_else(|e| panic!("Failed to create install dir: {}", e));

    let tarball_path = install_dir.join("libkrunfw.tgz");

    if !tarball_path.exists() {
        download_file(LIBKRUNFW_SO_URL, &tarball_path)
            .unwrap_or_else(|e| panic!("Failed to download libkrunfw: {}", e));

        verify_sha256(&tarball_path, LIBKRUNFW_SHA256)
            .unwrap_or_else(|e| panic!("Failed to verify libkrunfw checksum: {}", e));
    }

    extract_tarball(&tarball_path, install_dir)
        .unwrap_or_else(|e| panic!("Failed to extract libkrunfw: {}", e));

    println!(
        "cargo:warning=Extracted libkrunfw.so to {}",
        lib_dir.display()
    );
}

/// Windows: Download and extract the prebuilt krun-windows-x64.zip.
/// Returns the directory containing krun.lib, krun.dll, and libkrunfw.dll.
#[cfg(target_os = "windows")]
fn download_krun_windows_prebuilt(out_dir: &Path) -> PathBuf {
    let zip_path = out_dir.join("krun-windows-x64.zip");
    let extract_dir = out_dir.join("krun-windows-x64");
    let lib_dir = extract_dir.join("krun-windows-x64");

    if lib_dir.join("krun.lib").exists() {
        println!("cargo:warning=Using cached krun Windows prebuilt");
        return lib_dir;
    }

    download_file(KRUN_WINDOWS_URL, &zip_path)
        .unwrap_or_else(|e| panic!("Failed to download krun Windows prebuilt: {}", e));

    verify_sha256(&zip_path, KRUN_WINDOWS_SHA256)
        .unwrap_or_else(|e| panic!("Failed to verify krun Windows prebuilt checksum: {}", e));

    fs::create_dir_all(&extract_dir)
        .unwrap_or_else(|e| panic!("Failed to create extract dir: {}", e));

    // Use PowerShell to unzip (available on all modern Windows)
    let status = Command::new("powershell.exe")
        .args([
            "-NoProfile",
            "-Command",
            &format!(
                "Expand-Archive -LiteralPath '{}' -DestinationPath '{}' -Force",
                zip_path.display(),
                extract_dir.display(),
            ),
        ])
        .status()
        .unwrap_or_else(|e| panic!("Failed to run PowerShell unzip: {}", e));

    assert!(status.success(), "PowerShell Expand-Archive failed");
    println!(
        "cargo:warning=Extracted krun Windows prebuilt to {}",
        lib_dir.display()
    );
    lib_dir
}

/// Windows: Download and link the prebuilt krun.dll + krun.lib bundle.
///
/// Search order:
///   1. LIBKRUN_DIR env var (local build override)
///   2. deps/libkrun-sys/prebuilt/x86_64-pc-windows-msvc/ (vendored)
///   3. Auto-download krun-windows-x64.zip from GitHub Releases into OUT_DIR
#[cfg(target_os = "windows")]
fn build() {
    let target_arch = env::var("CARGO_CFG_TARGET_ARCH").unwrap_or_else(|_| "x86_64".to_string());
    let triple = format!("{}-pc-windows-msvc", target_arch);
    let out_dir = PathBuf::from(env::var("OUT_DIR").unwrap());

    let lib_dir = if let Ok(dir) = env::var("LIBKRUN_DIR") {
        PathBuf::from(dir)
    } else {
        let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
        let prebuilt = manifest_dir.join("prebuilt").join(&triple);
        if prebuilt.join("krun.lib").exists() {
            prebuilt
        } else {
            download_krun_windows_prebuilt(&out_dir)
        }
    };

    println!("cargo:rustc-link-search=native={}", lib_dir.display());
    println!("cargo:rustc-link-lib=dylib=krun");
    println!("cargo:rustc-link-lib=WinHvPlatform");
    println!("cargo:LIBKRUN_A3S_DEP={}", lib_dir.display());
    println!("cargo:LIBKRUNFW_A3S_DEP={}", lib_dir.display());
    println!("cargo:rerun-if-env-changed=LIBKRUN_DIR");
}

/// Linux: Build libkrun from source, download prebuilt libkrunfw
#[cfg(target_os = "linux")]
fn build() {
    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let out_dir = PathBuf::from(env::var("OUT_DIR").unwrap());

    let libkrunfw_install = out_dir.join("libkrunfw");
    let libkrun_install = out_dir.join("libkrun");
    let libkrunfw_lib_dir = libkrunfw_install.join(LIB_DIR);
    let libkrun_lib_dir = libkrun_install.join(LIB_DIR);

    // Skip build if outputs already exist (incremental build optimization)
    if has_library(&libkrunfw_lib_dir, "libkrunfw") && has_library(&libkrun_lib_dir, "libkrun") {
        configure_linking(&libkrun_lib_dir, &libkrunfw_lib_dir);
        return;
    }

    println!("cargo:warning=Building libkrun-sys for Linux (using prebuilt libkrunfw)");

    // Verify vendored libkrun source exists (libkrunfw is downloaded)
    verify_libkrun_source(&manifest_dir);

    // 1. Download pre-compiled libkrunfw.so directly (no build needed)
    download_libkrunfw_so(&libkrunfw_install);

    // 2. Build libkrun from vendored source
    let libkrun_src = manifest_dir.join("vendor/libkrun");
    build_with_make(
        &libkrun_src,
        &libkrun_install,
        "libkrun",
        libkrun_build_env(&libkrunfw_install),
    );

    // 3. Fix library names
    fix_linux_libs(&libkrun_lib_dir, "libkrun")
        .unwrap_or_else(|e| panic!("Failed to fix libkrun: {}", e));

    fix_linux_libs(&libkrunfw_lib_dir, "libkrunfw")
        .unwrap_or_else(|e| panic!("Failed to fix libkrunfw: {}", e));

    // 4. Configure linking
    configure_linking(&libkrun_lib_dir, &libkrunfw_lib_dir);
}

/// Unsupported platform
#[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
fn build() {
    eprintln!("ERROR: libkrun is only supported on macOS, Linux, and Windows");
    eprintln!();
    eprintln!("Supported platforms:");
    eprintln!("  - macOS ARM64 (Apple Silicon)");
    eprintln!("  - Linux x86_64 / aarch64");
    eprintln!("  - Windows x86_64 (WHPX)");
    std::process::exit(1);
}
