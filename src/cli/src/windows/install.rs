//! Download the Linux a3s-box binary and install it into WSL2.

use std::process::Command;

const RELEASE_URL: &str =
    "https://github.com/A3S-Lab/Box/releases/download/v{version}/a3s-box-linux-x86_64";

/// Download the Linux binary for `version` and install it at `~/.a3s/bin/a3s-box` in WSL2.
pub fn install_linux_binary(version: &str) -> Result<(), String> {
    let url = RELEASE_URL.replace("{version}", version);

    // Use curl inside WSL2 to download directly into the WSL2 filesystem.
    let script = format!(
        "mkdir -p ~/.a3s/bin && \
         curl -fsSL '{url}' -o ~/.a3s/bin/a3s-box && \
         chmod +x ~/.a3s/bin/a3s-box"
    );

    let output = Command::new("wsl")
        .args(["--", "bash", "-c", &script])
        .output()
        .map_err(|e| format!("Failed to run wsl: {e}"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("Binary install failed: {stderr}"));
    }

    Ok(())
}

/// Check if a3s-box is already installed in WSL2.
pub fn is_installed_in_wsl() -> bool {
    Command::new("wsl")
        .args(["--", "test", "-f", "~/.a3s/bin/a3s-box"])
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn url_substitution_is_correct() {
        let url = RELEASE_URL.replace("{version}", "0.6.0");
        assert!(url.contains("v0.6.0"));
        assert!(url.ends_with("a3s-box-linux-x86_64"));
    }
}
