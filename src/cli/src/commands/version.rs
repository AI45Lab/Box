//! `a3s-box version` command.

use clap::Args;

#[derive(Args)]
pub struct VersionArgs;

pub async fn execute(_args: VersionArgs) -> Result<(), Box<dyn std::error::Error>> {
    println!("{}", version_line());
    Ok(())
}

fn version_line() -> String {
    format!("a3s-box version {}", a3s_box_core::VERSION)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_line_uses_core_package_version() {
        assert_eq!(
            version_line(),
            format!("a3s-box version {}", a3s_box_core::VERSION)
        );
    }
}
