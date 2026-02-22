/// This module is a shortcut for client rpc calls.
/// It is useful for testing and debugging service rpc calls.
use clap::Subcommand;

#[derive(Debug, Subcommand)]
pub enum RpcCommand {
    /// Stop the running core
    StopCore,
}
