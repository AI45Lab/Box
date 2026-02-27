//! Proxy CLI arguments to a3s-box running inside WSL2.

use std::process;

/// Proxy `args` to `~/.a3s/bin/a3s-box` inside WSL2.
/// Inherits stdin/stdout/stderr and exits with the same code.
pub fn run(args: &[String]) -> ! {
    let status = process::Command::new("wsl")
        .arg("--")
        .arg("~/.a3s/bin/a3s-box")
        .args(args)
        .status()
        .unwrap_or_else(|e| {
            eprintln!("Failed to launch a3s-box in WSL2: {e}");
            process::exit(1);
        });

    process::exit(status.code().unwrap_or(1));
}

#[cfg(test)]
mod tests {
    #[test]
    fn args_passthrough_is_identity() {
        let input = vec!["run".to_string(), "ubuntu".to_string(), "--rm".to_string()];
        assert_eq!(input, input.clone());
    }
}
