use crate::logging;
use clap::{Parser, Subcommand};

mod install;
mod rpc;
mod status;
mod uninstall;

mod server;
mod start;

/// Nyanpasu Service, a privileged service for managing the core service.
///
/// The main entry point for the service, Other commands are the control plane for the service.
///
/// rpc subcommands are shortcuts for client rpc calls,
/// It is useful for testing and debugging service rpc calls.
#[derive(Parser)]
#[command(version, author, about, long_about, disable_version_flag = true)]
struct Cli {
    /// Enable verbose logging
    #[clap(short = 'V', long, default_value = "false")]
    verbose: bool,

    /// Print the version
    #[clap(short, long, default_value = "false")]
    version: bool,

    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand)]
enum Commands {
    /// Install the service
    Install(install::InstallCommand),
    /// Uninstall the service
    Uninstall,
    /// Start the service
    Start,
    /// Run the server. It should be called by the service manager.
    Server(server::ServerContext), // The main entry point for the service, other commands are the control plane for the service
    /// Get the status of the service
    Status(status::StatusCommand),
    /// RPC commands, a shortcut for client rpc calls
    #[command(subcommand)]
    Rpc(rpc::RpcCommand),
}

#[derive(thiserror::Error, Debug)]
pub enum CommandError {
    #[error("permission denied")]
    PermissionDenied,

    #[error("service not installed")]
    ServiceNotInstalled,

    #[error("service already installed")]
    ServiceAlreadyInstalled,
    #[error("service not running")]
    ServiceAlreadyStopped,
    #[error("service already running")]
    ServiceAlreadyRunning,
    #[error("join error: {0}")]
    JoinError(#[from] tokio::task::JoinError),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

pub async fn process() -> Result<(), CommandError> {
    let cli = Cli::parse();
    if cli.version {
        print_version();
    }

    if !matches!(
        cli.command,
        Some(Commands::Status(_)) | Some(Commands::Rpc(_)) | None
    ) && !crate::utils::must_check_elevation()
    {
        return Err(CommandError::PermissionDenied);
    }

    #[cfg(not(feature = "dev"))]
    {
        if matches!(cli.command, Some(Commands::Server(_))) {
            logging::init(cli.verbose, true)?;
        } else {
            // todo: used for debug. delete it in the future.
            logging::init(cli.verbose, false)?;
        }
    }

    match cli.command {
        Some(Commands::Install(ctx)) => {
            Ok(tokio::task::spawn_blocking(move || install::install(ctx)).await??)
        }
        Some(Commands::Uninstall) => Ok(tokio::task::spawn_blocking(uninstall::uninstall).await??),
        Some(Commands::Start) => Ok(tokio::task::spawn_blocking(start::start).await??),
        Some(Commands::Status(ctx)) => Ok(status::status(ctx).await?),
        Some(_) => {
            todo!()
        }
        None => {
            eprintln!("No command specified");
            Ok(())
        }
    }
}

pub fn print_version() {
    todo!()
}
