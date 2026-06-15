//! Install/uninstall the `a3s-box monitor` as a supervised per-user service.
//!
//! The monitor (`a3s-box monitor`) restarts dead/unhealthy detached boxes per
//! their restart policy. On its own it is just a foreground process: if the
//! operator's shell or the process dies, every detached box silently loses
//! restart + health supervision. This module installs it as a **per-user**
//! service (systemd `--user` on Linux, a launchd LaunchAgent on macOS) that the
//! OS keeps running and restarts on crash — no root required.

#[cfg(any(target_os = "linux", target_os = "macos", test))]
use std::path::{Path, PathBuf};

/// systemd unit name.
#[cfg(any(target_os = "linux", test))]
const SYSTEMD_UNIT: &str = "a3s-box-monitor.service";
/// launchd label.
#[cfg(any(target_os = "macos", test))]
const LAUNCHD_LABEL: &str = "com.a3s-box.monitor";

/// Render the systemd **user** unit that supervises `a3s-box monitor`.
#[cfg(any(target_os = "linux", test))]
pub fn systemd_unit(exe: &Path, interval: u64) -> String {
    format!(
        "[Unit]\n\
         Description=a3s-box monitor — restarts dead/unhealthy detached boxes\n\
         Documentation=https://github.com/AI45Lab/Box\n\
         After=network.target\n\
         \n\
         [Service]\n\
         Type=simple\n\
         ExecStart={exe} monitor --interval {interval}\n\
         Restart=always\n\
         RestartSec=2\n\
         \n\
         [Install]\n\
         WantedBy=default.target\n",
        exe = exe.display(),
    )
}

/// Render the launchd LaunchAgent plist that supervises `a3s-box monitor`.
#[cfg(any(target_os = "macos", test))]
pub fn launchd_plist(exe: &Path, interval: u64, log_path: &Path) -> String {
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
         <plist version=\"1.0\">\n\
         <dict>\n\
         \x20 <key>Label</key><string>{label}</string>\n\
         \x20 <key>ProgramArguments</key>\n\
         \x20 <array>\n\
         \x20\x20\x20 <string>{exe}</string>\n\
         \x20\x20\x20 <string>monitor</string>\n\
         \x20\x20\x20 <string>--interval</string>\n\
         \x20\x20\x20 <string>{interval}</string>\n\
         \x20 </array>\n\
         \x20 <key>RunAtLoad</key><true/>\n\
         \x20 <key>KeepAlive</key><true/>\n\
         \x20 <key>StandardOutPath</key><string>{log}</string>\n\
         \x20 <key>StandardErrorPath</key><string>{log}</string>\n\
         </dict>\n\
         </plist>\n",
        label = LAUNCHD_LABEL,
        exe = xml_escape(&exe.display().to_string()),
        log = xml_escape(&log_path.display().to_string()),
    )
}

/// Escape XML element-content special characters. Without this, an exe or home
/// path containing `&`, `<`, or `>` produces a malformed plist that `launchctl`
/// rejects — silently leaving the monitor unsupervised.
#[cfg(any(target_os = "macos", test))]
fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// Path the systemd user unit is written to.
#[cfg(any(target_os = "linux", test))]
fn systemd_unit_path() -> PathBuf {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            dirs::home_dir()
                .unwrap_or_else(|| PathBuf::from("."))
                .join(".config")
        });
    base.join("systemd").join("user").join(SYSTEMD_UNIT)
}

/// Path the launchd agent plist is written to.
#[cfg(any(target_os = "macos", test))]
fn launchd_plist_path() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("Library")
        .join("LaunchAgents")
        .join(format!("{LAUNCHD_LABEL}.plist"))
}

/// Install and enable the monitor as a supervised per-user service.
pub fn install(interval: u64) -> Result<(), Box<dyn std::error::Error>> {
    install_impl(interval)
}

/// Disable and remove the installed monitor service.
pub fn uninstall() -> Result<(), Box<dyn std::error::Error>> {
    uninstall_impl()
}

#[cfg(target_os = "linux")]
fn install_impl(interval: u64) -> Result<(), Box<dyn std::error::Error>> {
    let exe = std::env::current_exe()?;
    let path = systemd_unit_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&path, systemd_unit(&exe, interval))?;
    println!("Wrote systemd user unit: {}", path.display());
    let ok = run_quiet("systemctl", &["--user", "daemon-reload"])
        && run_quiet("systemctl", &["--user", "enable", "--now", SYSTEMD_UNIT]);
    if ok {
        println!("Enabled and started {SYSTEMD_UNIT} (systemctl --user).");
        let user = std::env::var("USER").unwrap_or_else(|_| "<user>".to_string());
        println!(
            "Tip: for a headless host, run `loginctl enable-linger {user}` so the \
             monitor runs without an active login session."
        );
    } else {
        println!(
            "Could not enable via systemctl automatically. Enable it with:\n  \
             systemctl --user daemon-reload && systemctl --user enable --now {SYSTEMD_UNIT}"
        );
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn install_impl(interval: u64) -> Result<(), Box<dyn std::error::Error>> {
    let exe = std::env::current_exe()?;
    let path = launchd_plist_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let log = a3s_box_core::dirs_home().join("monitor.log");
    if let Some(parent) = log.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&path, launchd_plist(&exe, interval, &log))?;
    println!("Wrote launchd agent: {}", path.display());
    let _ = run_quiet("launchctl", &["unload", &path.to_string_lossy()]);
    if run_quiet("launchctl", &["load", "-w", &path.to_string_lossy()]) {
        println!(
            "Loaded {LAUNCHD_LABEL} (launchctl). Logs: {}",
            log.display()
        );
    } else {
        println!(
            "Could not load via launchctl automatically. Load it with:\n  \
             launchctl load -w {}",
            path.display()
        );
    }
    Ok(())
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn install_impl(_interval: u64) -> Result<(), Box<dyn std::error::Error>> {
    Err("monitor --install is only supported on Linux (systemd) and macOS (launchd)".into())
}

#[cfg(target_os = "linux")]
fn uninstall_impl() -> Result<(), Box<dyn std::error::Error>> {
    let _ = run_quiet("systemctl", &["--user", "disable", "--now", SYSTEMD_UNIT]);
    let path = systemd_unit_path();
    if path.exists() {
        std::fs::remove_file(&path)?;
        println!("Removed {}", path.display());
    }
    let _ = run_quiet("systemctl", &["--user", "daemon-reload"]);
    Ok(())
}

#[cfg(target_os = "macos")]
fn uninstall_impl() -> Result<(), Box<dyn std::error::Error>> {
    let path = launchd_plist_path();
    let _ = run_quiet("launchctl", &["unload", &path.to_string_lossy()]);
    if path.exists() {
        std::fs::remove_file(&path)?;
        println!("Removed {}", path.display());
    }
    Ok(())
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn uninstall_impl() -> Result<(), Box<dyn std::error::Error>> {
    Err("monitor --uninstall is only supported on Linux and macOS".into())
}

/// Run a command, returning true on success. Output is suppressed; failures are
/// non-fatal (the caller prints manual fallback instructions).
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn run_quiet(cmd: &str, args: &[&str]) -> bool {
    std::process::Command::new(cmd)
        .args(args)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn systemd_unit_has_execstart_and_restart() {
        let unit = systemd_unit(Path::new("/usr/local/bin/a3s-box"), 5);
        assert!(unit.contains("ExecStart=/usr/local/bin/a3s-box monitor --interval 5"));
        assert!(unit.contains("Restart=always"));
        assert!(unit.contains("WantedBy=default.target"));
    }

    #[test]
    fn launchd_plist_has_keepalive_and_program() {
        let plist = launchd_plist(
            Path::new("/usr/local/bin/a3s-box"),
            7,
            Path::new("/home/u/.a3s/monitor.log"),
        );
        assert!(plist.contains("<key>Label</key><string>com.a3s-box.monitor</string>"));
        assert!(plist.contains("<string>/usr/local/bin/a3s-box</string>"));
        assert!(plist.contains("<string>7</string>"));
        assert!(plist.contains("<key>KeepAlive</key><true/>"));
        assert!(plist.contains("/home/u/.a3s/monitor.log"));
    }

    #[test]
    fn install_paths_are_user_scoped() {
        assert!(systemd_unit_path()
            .to_string_lossy()
            .contains("systemd/user/a3s-box-monitor.service"));
        assert!(launchd_plist_path()
            .to_string_lossy()
            .contains("LaunchAgents/com.a3s-box.monitor.plist"));
    }

    #[test]
    fn launchd_plist_escapes_xml_specials_in_paths() {
        // A path with `&`/`<`/`>` (e.g. a quirky A3S_HOME) must not produce a
        // malformed plist that launchctl rejects.
        let plist = launchd_plist(
            Path::new("/opt/a&b/<bin>/a3s-box"),
            5,
            Path::new("/home/a&b/.a3s/monitor.log"),
        );
        assert!(plist.contains("/opt/a&amp;b/&lt;bin&gt;/a3s-box"));
        assert!(plist.contains("/home/a&amp;b/.a3s/monitor.log"));
        // The raw, unescaped forms must NOT appear.
        assert!(!plist.contains("a&b"));
        assert!(!plist.contains("<bin>"));
    }
}
