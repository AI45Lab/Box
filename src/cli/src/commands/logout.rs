//! `a3s-box logout` command — Remove stored registry credentials.

use clap::Args;

const DEFAULT_REGISTRY_SERVER: &str = "index.docker.io";

#[derive(Args)]
pub struct LogoutArgs {
    /// Registry server (default: index.docker.io)
    pub server: Option<String>,
}

pub async fn execute(args: LogoutArgs) -> Result<(), Box<dyn std::error::Error>> {
    let server = registry_server_or_default(args.server);

    let store = a3s_box_runtime::CredentialStore::default_path()?;
    let removed = store.remove(&server)?;

    println!("{}", logout_result_line(&server, removed));

    Ok(())
}

fn registry_server_or_default(server: Option<String>) -> String {
    server.unwrap_or_else(|| DEFAULT_REGISTRY_SERVER.to_string())
}

fn logout_result_line(server: &str, removed: bool) -> String {
    if removed {
        format!("Removing login credentials for {server}")
    } else {
        format!("Not logged in to {server}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_server_defaults_to_docker_hub() {
        assert_eq!(registry_server_or_default(None), "index.docker.io");
    }

    #[test]
    fn registry_server_preserves_explicit_server() {
        assert_eq!(
            registry_server_or_default(Some("registry.example".to_string())),
            "registry.example"
        );
    }

    #[test]
    fn logout_result_line_reports_removed_credentials() {
        assert_eq!(
            logout_result_line("ghcr.io", true),
            "Removing login credentials for ghcr.io"
        );
    }

    #[test]
    fn logout_result_line_reports_not_logged_in() {
        assert_eq!(
            logout_result_line("ghcr.io", false),
            "Not logged in to ghcr.io"
        );
    }
}
