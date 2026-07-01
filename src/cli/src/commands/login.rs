//! `a3s-box login` command — Store registry credentials.

use clap::Args;

const DEFAULT_REGISTRY_SERVER: &str = "index.docker.io";

#[derive(Args)]
pub struct LoginArgs {
    /// Registry server (default: index.docker.io)
    pub server: Option<String>,

    /// Username
    #[arg(short, long)]
    pub username: Option<String>,

    /// Password
    #[arg(short, long)]
    pub password: Option<String>,

    /// Read password from stdin
    #[arg(long)]
    pub password_stdin: bool,
}

pub async fn execute(args: LoginArgs) -> Result<(), Box<dyn std::error::Error>> {
    let server = registry_server_or_default(args.server);

    let username = match args.username {
        Some(u) => u,
        None => {
            eprint!("Username: ");
            let mut input = String::new();
            std::io::stdin().read_line(&mut input)?;
            input.trim().to_string()
        }
    };

    let password = if args.password_stdin {
        let mut input = String::new();
        std::io::stdin().read_line(&mut input)?;
        input.trim().to_string()
    } else {
        match args.password {
            Some(p) => p,
            None => {
                eprint!("Password: ");
                let mut input = String::new();
                std::io::stdin().read_line(&mut input)?;
                input.trim().to_string()
            }
        }
    };

    validate_credentials(&username, &password)
        .map_err(|error| -> Box<dyn std::error::Error> { error.into() })?;

    let store = a3s_box_runtime::CredentialStore::default_path()?;
    store.store(&server, &username, &password)?;

    println!("Login Succeeded");
    Ok(())
}

fn registry_server_or_default(server: Option<String>) -> String {
    server.unwrap_or_else(|| DEFAULT_REGISTRY_SERVER.to_string())
}

fn validate_credentials(username: &str, password: &str) -> Result<(), &'static str> {
    if username.is_empty() || password.is_empty() {
        Err("Username and password are required")
    } else {
        Ok(())
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
            registry_server_or_default(Some("ghcr.io".to_string())),
            "ghcr.io"
        );
    }

    #[test]
    fn validate_credentials_accepts_non_empty_values() {
        assert!(validate_credentials("alice", "secret").is_ok());
    }

    #[test]
    fn validate_credentials_rejects_missing_username_or_password() {
        assert_eq!(
            validate_credentials("", "secret").unwrap_err(),
            "Username and password are required"
        );
        assert_eq!(
            validate_credentials("alice", "").unwrap_err(),
            "Username and password are required"
        );
    }
}
