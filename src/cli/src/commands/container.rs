//! `a3s-box container` subcommands — Docker-compatible container command namespace.

use clap::{Args, Subcommand};

#[derive(Args)]
pub struct ContainerArgs {
    #[command(subcommand)]
    pub command: ContainerCommand,
}

#[derive(Subcommand)]
pub enum ContainerCommand {
    /// List boxes
    #[command(aliases = ["list", "ps"])]
    Ls(super::ps::PsArgs),
    /// Create a new box without starting it
    Create(super::create::CreateArgs),
    /// Start one or more stopped or created boxes
    Start(super::start::StartArgs),
    /// Gracefully stop one or more running boxes
    Stop(super::stop::StopArgs),
    /// Restart one or more boxes
    Restart(super::restart::RestartArgs),
    /// Remove one or more boxes
    #[command(alias = "remove")]
    Rm(super::rm::RmArgs),
    /// Remove all stopped boxes
    Prune(super::container_prune::ContainerPruneArgs),
    /// Force-kill one or more running boxes
    Kill(super::kill::KillArgs),
    /// Pause one or more running boxes
    Pause(super::pause::PauseArgs),
    /// Unpause one or more paused boxes
    Unpause(super::unpause::UnpauseArgs),
    /// Display detailed box information
    Inspect(super::inspect::InspectArgs),
    /// View box logs
    Logs(super::logs::LogsArgs),
    /// Execute a command in a running box
    Exec(super::exec::ExecArgs),
    /// Display running processes in a box
    Top(super::top::TopArgs),
    /// Display resource usage statistics
    Stats(super::stats::StatsArgs),
    /// Attach to a running box's console output
    Attach(super::attach::AttachArgs),
    /// Rename a box
    Rename(super::rename::RenameArgs),
    /// Block until one or more boxes stop
    Wait(super::wait::WaitArgs),
    /// List port mappings for a box
    Port(super::port::PortArgs),
    /// Export a box's filesystem to a tar archive
    Export(super::export::ExportArgs),
    /// Show filesystem changes in a box
    Diff(super::diff::DiffArgs),
    /// Update resource limits of a box
    Update(super::container_update::ContainerUpdateArgs),
}

pub async fn execute(args: ContainerArgs) -> Result<(), Box<dyn std::error::Error>> {
    match args.command {
        ContainerCommand::Ls(a) => super::ps::execute(a).await,
        ContainerCommand::Create(a) => super::create::execute(a).await,
        ContainerCommand::Start(a) => super::start::execute(a).await,
        ContainerCommand::Stop(a) => super::stop::execute(a).await,
        ContainerCommand::Restart(a) => super::restart::execute(a).await,
        ContainerCommand::Rm(a) => super::rm::execute(a).await,
        ContainerCommand::Prune(a) => super::container_prune::execute(a).await,
        ContainerCommand::Kill(a) => super::kill::execute(a).await,
        ContainerCommand::Pause(a) => super::pause::execute(a).await,
        ContainerCommand::Unpause(a) => super::unpause::execute(a).await,
        ContainerCommand::Inspect(a) => super::inspect::execute(a).await,
        ContainerCommand::Logs(a) => super::logs::execute(a).await,
        ContainerCommand::Exec(a) => super::exec::execute(a).await,
        ContainerCommand::Top(a) => super::top::execute(a).await,
        ContainerCommand::Stats(a) => super::stats::execute(a).await,
        ContainerCommand::Attach(a) => super::attach::execute(a).await,
        ContainerCommand::Rename(a) => super::rename::execute(a).await,
        ContainerCommand::Wait(a) => super::wait::execute(a).await,
        ContainerCommand::Port(a) => super::port::execute(a).await,
        ContainerCommand::Export(a) => super::export::execute(a).await,
        ContainerCommand::Diff(a) => super::diff::execute(a).await,
        ContainerCommand::Update(a) => super::container_update::execute(a).await,
    }
}
