//! `a3s-box login` command — Store registry credentials.

use clap::Args;

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
    let server = args.server.unwrap_or_else(|| "index.docker.io".to_string());

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

    if username.is_empty() || password.is_empty() {
        return Err("Username and password are required".into());
    }

    let store = a3s_box_runtime::CredentialStore::default_path()?;
    store.store(&server, &username, &password)?;

    println!("Login Succeeded");
    Ok(())
}
