//! Windows launcher entry point for a3s-box.
//!
//! Detection chain:
//!   1. Check ~/.a3s/wsl-ready cache
//!   2. Detect WSL2 status
//!   3. Install WSL2 if missing (UAC elevation)
//!   4. Install Linux binary if missing
//!   5. Proxy all args to WSL2

mod cache;
mod elevate;
mod install;
mod proxy;
mod wsl;

const VERSION: &str = env!("CARGO_PKG_VERSION");

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();

    // Internal flag: we were relaunched as admin to install WSL2.
    if args.first().map(|s| s.as_str()) == Some("--install-wsl") {
        match elevate::install_wsl() {
            Ok(out) => {
                let msg = String::from_utf8_lossy(&out.stdout);
                println!("{msg}");
                println!("WSL2 installed. Please restart your computer, then run a3s-box again.");
            }
            Err(e) => {
                eprintln!("WSL2 installation failed: {e}");
                std::process::exit(1);
            }
        }
        return;
    }

    // Step 1: cache hit → skip detection.
    if cache::is_valid(VERSION) {
        proxy::run(&args);
    }

    // Step 2: detect WSL2.
    match wsl::detect() {
        wsl::WslStatus::NotInstalled | wsl::WslStatus::NoDistro => {
            eprintln!("WSL2 is not installed. Requesting administrator privileges to install...");
            if let Err(e) = elevate::relaunch_as_admin() {
                eprintln!("Failed to elevate: {e}");
                eprintln!("Please run this command as Administrator.");
                std::process::exit(1);
            }
            // Elevated process takes over; exit this one.
            std::process::exit(0);
        }
        wsl::WslStatus::Ready => {}
    }

    // Step 3: install Linux binary if missing.
    if !install::is_installed_in_wsl() {
        eprintln!("Installing a3s-box into WSL2 (one-time setup)...");
        if let Err(e) = install::install_linux_binary(VERSION) {
            eprintln!("Setup failed: {e}");
            std::process::exit(1);
        }
    }

    // Step 4: write cache.
    if let Err(e) = cache::write(VERSION) {
        // Non-fatal: next run will just re-check.
        eprintln!("Warning: could not write cache: {e}");
    }

    // Step 5: proxy.
    proxy::run(&args);
}
