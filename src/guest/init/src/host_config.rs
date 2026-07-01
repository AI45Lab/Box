//! Guest hostname and sysctl configuration.

use std::path::Path;

/// Apply host configuration from the boot environment: pod sysctls and, if
/// present, the hostname.
pub fn apply_from_env() -> Result<(), Box<dyn std::error::Error>> {
    apply_sysctls_from_env();

    let Ok(hostname) = std::env::var("BOX_HOSTNAME") else {
        return Ok(());
    };
    apply_hostname(&hostname, Path::new("/etc/hostname"))
}

/// Apply pod sysctls passed as `BOX_SYSCTL_<index>=<name>=<value>`.
///
/// Each is written to `/proc/sys/<name with '.' as '/'>`. Best-effort: a sysctl
/// the guest kernel does not expose is logged and skipped rather than aborting
/// VM startup.
fn apply_sysctls_from_env() {
    let mut index = 0;
    while let Ok(spec) = std::env::var(format!("BOX_SYSCTL_{index}")) {
        index += 1;
        let Some((name, value)) = spec.split_once('=') else {
            continue;
        };
        let path = format!("/proc/sys/{}", name.trim().replace('.', "/"));
        match std::fs::write(&path, value) {
            Ok(()) => tracing::info!("Applied sysctl {name}={value}"),
            Err(e) => tracing::warn!("Failed to apply sysctl {name}={value} ({path}): {e}"),
        }
    }
}

fn apply_hostname(hostname: &str, hostname_path: &Path) -> Result<(), Box<dyn std::error::Error>> {
    a3s_box_core::dns::validate_hostname(hostname)
        .map_err(|e| format!("invalid BOX_HOSTNAME: {e}"))?;

    set_kernel_hostname(hostname)?;
    write_hostname_file(hostname_path, hostname)?;
    Ok(())
}

#[cfg(target_os = "linux")]
fn set_kernel_hostname(hostname: &str) -> Result<(), Box<dyn std::error::Error>> {
    use std::ffi::CString;

    let hostname = CString::new(hostname.as_bytes())?;
    let ret = unsafe { libc::sethostname(hostname.as_ptr(), hostname.as_bytes().len()) };
    if ret != 0 {
        return Err(Box::new(std::io::Error::last_os_error()));
    }
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn set_kernel_hostname(hostname: &str) -> Result<(), Box<dyn std::error::Error>> {
    let _ = hostname;
    Ok(())
}

fn write_hostname_file(
    hostname_path: &Path,
    hostname: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    if let Some(parent) = hostname_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(hostname_path, format!("{hostname}\n"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_write_hostname_file() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("etc/hostname");

        write_hostname_file(&path, "web").unwrap();

        assert_eq!(std::fs::read_to_string(path).unwrap(), "web\n");
    }

    #[test]
    fn test_apply_hostname_rejects_invalid_hostname_before_write() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("etc/hostname");

        let err = apply_hostname("bad_host", &path).unwrap_err();

        assert!(err.to_string().contains("invalid BOX_HOSTNAME"));
        assert!(!path.exists());
    }
}
