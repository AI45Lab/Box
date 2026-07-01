//! `a3s-box logout` command — Remove stored registry credentials.

use clap::Args;

#[derive(Args)]
pub struct LogoutArgs {
    /// Registry server (default: index.docker.io)
    pub server: Option<String>,
}

pub async fn execute(args: LogoutArgs) -> Result<(), Box<dyn std::error::Error>> {
    let server = args.server.unwrap_or_else(|| "index.docker.io".to_string());

    let store = a3s_box_runtime::CredentialStore::default_path()?;
    let removed = store.remove(&server)?;

    if removed {
        println!("Removing login credentials for {}", server);
    } else {
        println!("Not logged in to {}", server);
    }

    Ok(())
}
