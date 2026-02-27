//! WSL2 detection and installation helpers.

use std::process::Command;

/// Result of WSL2 status check.
#[derive(Debug, PartialEq)]
pub enum WslStatus {
    /// WSL2 is installed and a default distro is set.
    Ready,
    /// WSL is installed but no default distro (needs `wsl --install`).
    NoDistro,
    /// WSL is not installed at all.
    NotInstalled,
}

/// Detect current WSL2 status by running `wsl --status`.
pub fn detect() -> WslStatus {
    let output = Command::new("wsl")
        .args(["--status"])
        .output();

    match output {
        Err(_) => WslStatus::NotInstalled,
        Ok(out) => {
            let text = String::from_utf8_lossy(&out.stdout);
            if out.status.success() && text.contains("Default Distribution") {
                WslStatus::Ready
            } else {
                WslStatus::NoDistro
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detect_returns_a_valid_variant() {
        // On any machine this must return one of the three variants without panicking.
        let status = detect();
        assert!(matches!(
            status,
            WslStatus::Ready | WslStatus::NoDistro | WslStatus::NotInstalled
        ));
    }
}
