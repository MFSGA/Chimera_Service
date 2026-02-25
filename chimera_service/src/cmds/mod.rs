use crate::logging;
use clap::{Parser, Subcommand};

mod rpc;
mod status;

mod server;

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
    /// Uninstall the service
    Uninstall,
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

    if matches!(cli.command, Some(Commands::Server(_))) {
        logging::init(cli.verbose, true)?;
    } else {
        logging::init(cli.verbose, false)?;
    }

    match cli.command {
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
