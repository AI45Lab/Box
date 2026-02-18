//! CLI command definitions and dispatch.

mod attach;
mod attest;
mod audit;
mod build;
mod commit;
pub(crate) mod common;
mod compose;
mod container_update;
mod cp;
mod create;
mod df;
pub(crate) mod diff;
mod events;
pub(crate) mod exec;
mod export;
mod history;
mod image_inspect;
mod image_prune;
mod image_tag;
mod images;
mod info;
mod inject_secret;
mod inspect;
mod kill;
mod load;
mod login;
mod logout;
mod logs;
mod monitor;
mod network;
mod pause;
mod port;
mod ps;
mod pull;
mod push;
mod rename;
mod restart;
mod rm;
mod rmi;
mod run;
mod save;
mod seal;
mod snapshot;
mod start;
mod stats;
mod stop;
mod system_prune;
mod top;
mod unpause;
mod unseal;
mod version;
pub mod volume;
mod wait;

use std::path::PathBuf;

use clap::{Parser, Subcommand};

/// Environment variable to override the image cache size limit.
///
/// Accepts human-readable sizes: `500m`, `10g`, `1t`, etc.
const IMAGE_CACHE_SIZE_ENV: &str = "A3S_IMAGE_CACHE_SIZE";

/// A3S Box — Docker-like MicroVM runtime.
#[derive(Parser)]
#[command(name = "a3s-box", version, about)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

/// Available commands.
#[derive(Subcommand)]
pub enum Command {
    /// Create and start a new box from an image
    Run(run::RunArgs),
    /// Create a new box without starting it
    Create(create::CreateArgs),
    /// Start one or more stopped or created boxes
    Start(start::StartArgs),
    /// Gracefully stop one or more running boxes
    Stop(stop::StopArgs),
    /// Restart one or more boxes
    Restart(restart::RestartArgs),
    /// Remove one or more boxes
    Rm(rm::RmArgs),
    /// Force-kill one or more running boxes
    Kill(kill::KillArgs),
    /// Pause one or more running boxes
    Pause(pause::PauseArgs),
    /// Unpause one or more paused boxes
    Unpause(unpause::UnpauseArgs),
    /// List boxes
    Ps(ps::PsArgs),
    /// Display resource usage statistics
    Stats(stats::StatsArgs),
    /// View box logs
    Logs(logs::LogsArgs),
    /// Execute a command in a running box
    Exec(exec::ExecArgs),
    /// Display running processes in a box
    Top(top::TopArgs),
    /// Display detailed box information
    Inspect(inspect::InspectArgs),
    /// Attach to a running box's console output
    Attach(attach::AttachArgs),
    /// Request and verify a TEE attestation report from a running box
    Attest(attest::AttestArgs),
    /// View the audit log
    Audit(audit::AuditArgs),
    /// Seal (encrypt) data bound to a TEE's identity
    Seal(seal::SealArgs),
    /// Unseal (decrypt) data inside a TEE
    Unseal(unseal::UnsealArgs),
    /// Inject secrets into a running TEE box via RA-TLS
    InjectSecret(inject_secret::InjectSecretArgs),
    /// Block until one or more boxes stop
    Wait(wait::WaitArgs),
    /// Rename a box
    Rename(rename::RenameArgs),
    /// List port mappings for a box
    Port(port::PortArgs),
    /// Export a box's filesystem to a tar archive
    Export(export::ExportArgs),
    /// Create an image from a box's changes
    Commit(commit::CommitArgs),
    /// Show filesystem changes in a box
    Diff(diff::DiffArgs),
    /// Stream real-time system events
    Events(events::EventsArgs),
    /// Update resource limits of a box
    ContainerUpdate(container_update::ContainerUpdateArgs),
    /// Manage multi-container workloads with a compose file
    Compose(compose::ComposeArgs),
    /// Manage VM snapshots (create, restore, list, remove)
    Snapshot(snapshot::SnapshotArgs),
    /// Build an image from a Dockerfile
    Build(build::BuildArgs),
    /// List cached images
    Images(images::ImagesArgs),
    /// Pull an image from a registry
    Pull(pull::PullArgs),
    /// Push an image to a registry
    Push(push::PushArgs),
    /// Log in to a container registry
    Login(login::LoginArgs),
    /// Log out from a container registry
    Logout(logout::LogoutArgs),
    /// Remove one or more cached images
    Rmi(rmi::RmiArgs),
    /// Display detailed image information as JSON
    ImageInspect(image_inspect::ImageInspectArgs),
    /// Show image layer history
    History(history::HistoryArgs),
    /// Remove unused images
    ImagePrune(image_prune::ImagePruneArgs),
    /// Create a tag that refers to an existing image
    Tag(image_tag::ImageTagArgs),
    /// Save an image to a tar archive
    Save(save::SaveArgs),
    /// Load an image from a tar archive
    Load(load::LoadArgs),
    /// Copy files between host and a running box
    Cp(cp::CpArgs),
    /// Manage networks
    Network(network::NetworkArgs),
    /// Manage volumes
    Volume(volume::VolumeArgs),
    /// Show disk usage
    Df(df::DfArgs),
    /// Remove all unused data (stopped boxes and unused images)
    SystemPrune(system_prune::SystemPruneArgs),
    /// Show version information
    Version(version::VersionArgs),
    /// Show system information
    Info(info::InfoArgs),
    /// Background daemon that monitors and restarts dead boxes
    Monitor(monitor::MonitorArgs),
}

/// Return the path to the image store directory (~/.a3s/images).
pub(crate) fn images_dir() -> PathBuf {
    dirs::home_dir()
        .map(|h| h.join(".a3s"))
        .unwrap_or_else(|| PathBuf::from(".a3s"))
        .join("images")
}

/// Open the shared image store.
///
/// The cache size limit can be configured via the `A3S_IMAGE_CACHE_SIZE`
/// environment variable (e.g., `500m`, `20g`). Defaults to 10 GB.
pub(crate) fn open_image_store() -> Result<a3s_box_runtime::ImageStore, Box<dyn std::error::Error>>
{
    let dir = images_dir();
    let max_size = match std::env::var(IMAGE_CACHE_SIZE_ENV) {
        Ok(val) => crate::output::parse_size_bytes(&val).map_err(|e| {
            format!("Invalid {IMAGE_CACHE_SIZE_ENV}={val:?}: {e} (examples: 500m, 10g, 1t)")
        })?,
        Err(_) => a3s_box_runtime::DEFAULT_IMAGE_CACHE_SIZE,
    };
    let store = a3s_box_runtime::ImageStore::new(&dir, max_size)?;
    Ok(store)
}

/// Tail a file, printing new content as it appears.
///
/// Waits for the file to exist, then continuously reads and prints new data.
/// Used by `run` (foreground mode) and `attach`.
pub(crate) async fn tail_file(path: &std::path::Path) {
    use tokio::io::AsyncReadExt;

    // Wait for file to exist
    loop {
        if path.exists() {
            break;
        }
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
    }

    let mut file = match tokio::fs::File::open(path).await {
        Ok(f) => f,
        Err(_) => return,
    };

    let mut buf = vec![0u8; 4096];
    loop {
        match file.read(&mut buf).await {
            Ok(0) => {
                tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;
            }
            Ok(n) => {
                let text = String::from_utf8_lossy(&buf[..n]);
                print!("{text}");
            }
            Err(_) => break,
        }
    }
}

/// Dispatch a parsed CLI to the appropriate command handler.
pub async fn dispatch(cli: Cli) -> Result<(), Box<dyn std::error::Error>> {
    match cli.command {
        Command::Run(args) => run::execute(args).await,
        Command::Create(args) => create::execute(args).await,
        Command::Start(args) => start::execute(args).await,
        Command::Stop(args) => stop::execute(args).await,
        Command::Restart(args) => restart::execute(args).await,
        Command::Rm(args) => rm::execute(args).await,
        Command::Kill(args) => kill::execute(args).await,
        Command::Pause(args) => pause::execute(args).await,
        Command::Unpause(args) => unpause::execute(args).await,
        Command::Ps(args) => ps::execute(args).await,
        Command::Stats(args) => stats::execute(args).await,
        Command::Logs(args) => logs::execute(args).await,
        Command::Exec(args) => exec::execute(args).await,
        Command::Top(args) => top::execute(args).await,
        Command::Inspect(args) => inspect::execute(args).await,
        Command::Attach(args) => attach::execute(args).await,
        Command::Attest(args) => attest::execute(args).await,
        Command::Audit(args) => audit::execute(args).await,
        Command::Seal(args) => seal::execute(args).await,
        Command::Unseal(args) => unseal::execute(args).await,
        Command::InjectSecret(args) => inject_secret::execute(args).await,
        Command::Wait(args) => wait::execute(args).await,
        Command::Rename(args) => rename::execute(args).await,
        Command::Port(args) => port::execute(args).await,
        Command::Export(args) => export::execute(args).await,
        Command::Commit(args) => commit::execute(args).await,
        Command::Diff(args) => diff::execute(args).await,
        Command::Events(args) => events::execute(args).await,
        Command::ContainerUpdate(args) => container_update::execute(args).await,
        Command::Compose(args) => compose::execute(args).await,
        Command::Snapshot(args) => snapshot::execute(args).await,
        Command::Build(args) => build::execute(args).await,
        Command::Images(args) => images::execute(args).await,
        Command::Pull(args) => pull::execute(args).await,
        Command::Push(args) => push::execute(args).await,
        Command::Login(args) => login::execute(args).await,
        Command::Logout(args) => logout::execute(args).await,
        Command::Rmi(args) => rmi::execute(args).await,
        Command::ImageInspect(args) => image_inspect::execute(args).await,
        Command::History(args) => history::execute(args).await,
        Command::ImagePrune(args) => image_prune::execute(args).await,
        Command::Tag(args) => image_tag::execute(args).await,
        Command::Save(args) => save::execute(args).await,
        Command::Load(args) => load::execute(args).await,
        Command::Cp(args) => cp::execute(args).await,
        Command::Network(args) => network::execute(args).await,
        Command::Volume(args) => volume::execute(args).await,
        Command::Df(args) => df::execute(args).await,
        Command::SystemPrune(args) => system_prune::execute(args).await,
        Command::Version(args) => version::execute(args).await,
        Command::Info(args) => info::execute(args).await,
        Command::Monitor(args) => monitor::execute(args).await,
    }
}
