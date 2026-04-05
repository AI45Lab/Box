fn main() {
    // Read libkrun library paths from libkrun-sys build metadata.
    // These are set by the `links = "krun"` declaration in libkrun-sys.
    let libkrun_dir = std::env::var("DEP_KRUN_LIBKRUN_A3S_DEP").unwrap_or_default();
    let libkrunfw_dir = std::env::var("DEP_KRUN_LIBKRUNFW_A3S_DEP").unwrap_or_default();

    #[cfg(windows)]
    copy_runtime_dlls(&libkrun_dir, &libkrunfw_dir);

    #[cfg(target_os = "macos")]
    copy_runtime_dylibs(&libkrun_dir, &libkrunfw_dir);

    // On macOS, use @executable_path so the binary finds libkrun next to itself.
    // On Linux, emit rpath to the build directory (runtime discovery is handled differently).
    #[cfg(target_os = "macos")]
    {
        // Use @executable_path/../lib to find libkrun in the same directory as the binary.
        // At runtime, libkrun.1.dylib and libkrun.dylib must be copied next to the binary.
        // This is handled by the SDK's ensure_shim() function.
        println!("cargo:rustc-link-arg=-Wl,-rpath,@executable_path/../lib");
    }
    #[cfg(all(not(target_os = "macos"), not(windows)))]
    {
        // Linux: emit rpath so the binary can find libkrun at runtime.
        if !libkrun_dir.is_empty() && libkrun_dir != "/nonexistent" {
            println!("cargo:rustc-link-arg=-Wl,-rpath,{libkrun_dir}");
        }
        if !libkrunfw_dir.is_empty()
            && libkrunfw_dir != "/nonexistent"
            && libkrunfw_dir != libkrun_dir
        {
            println!("cargo:rustc-link-arg=-Wl,-rpath,{libkrunfw_dir}");
        }
    }
}

#[cfg(target_os = "macos")]
fn copy_runtime_dylibs(libkrun_dir: &str, _libkrunfw_dir: &str) {
    use std::path::{Path, PathBuf};

    fn copy_if_present(src_dir: &str, file_name: &str, bin_dir: &Path) {
        if src_dir.is_empty() || src_dir == "/nonexistent" {
            return;
        }

        let src = PathBuf::from(src_dir).join(file_name);
        if !src.exists() {
            println!("cargo:warning={} not found at {}", file_name, src.display());
            return;
        }

        let dst = bin_dir.join(file_name);
        std::fs::copy(&src, &dst).unwrap_or_else(|e| panic!("failed to copy {}: {}", file_name, e));
        println!(
            "cargo:warning=copied {} -> {}",
            src.display(),
            dst.display()
        );
        println!("cargo:rerun-if-changed={}", src.display());

        // Also fix the install name to use @executable_path
        let install_name = format!("@executable_path/{}", file_name);
        let status = std::process::Command::new("install_name_tool")
            .args(["-id", &install_name, dst.to_str().unwrap()])
            .status();
        if let Ok(s) = status {
            if s.success() {
                println!("cargo:warning=fixed install name to {}", install_name);
            }
        }
    }

    let out_dir = PathBuf::from(std::env::var("OUT_DIR").unwrap());
    let bin_dir = out_dir
        .ancestors()
        .nth(3)
        .expect("unexpected OUT_DIR depth");

    // Copy libkrun and its alias
    copy_if_present(libkrun_dir, "libkrun.1.dylib", bin_dir);
    copy_if_present(libkrun_dir, "libkrun.dylib", bin_dir);
}

#[cfg(windows)]
fn copy_runtime_dlls(libkrun_dir: &str, libkrunfw_dir: &str) {
    use std::path::{Path, PathBuf};

    fn copy_if_present(src_dir: &str, file_name: &str, bin_dir: &Path) {
        if src_dir.is_empty() || src_dir == "/nonexistent" {
            return;
        }

        let src = PathBuf::from(src_dir).join(file_name);
        if !src.exists() {
            println!("cargo:warning={} not found at {}", file_name, src.display());
            return;
        }

        let dst = bin_dir.join(file_name);
        std::fs::copy(&src, &dst).unwrap_or_else(|e| panic!("failed to copy {}: {}", file_name, e));
        println!(
            "cargo:warning=copied {} -> {}",
            src.display(),
            dst.display()
        );
        println!("cargo:rerun-if-changed={}", src.display());
    }

    let out_dir = PathBuf::from(std::env::var("OUT_DIR").unwrap());
    let bin_dir = out_dir
        .ancestors()
        .nth(3)
        .expect("unexpected OUT_DIR depth");

    copy_if_present(libkrun_dir, "krun.dll", bin_dir);
    copy_if_present(libkrunfw_dir, "libkrunfw.dll", bin_dir);
}
