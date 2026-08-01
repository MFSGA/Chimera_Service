use std::{borrow::Cow, net::IpAddr};

use chimera_ipc::{
    api::{network::set_dns::NetworkSetDnsReq, status::RevisionIdInfo},
    client::shortcuts::Client,
};
use chimera_utils::core::{ClashCoreType, CoreType};
use clap::Subcommand;

fn core_type_parser(value: &str) -> Result<CoreType, String> {
    let friendly = match value {
        "mihomo" => Some(CoreType::Clash(ClashCoreType::Mihomo)),
        "mihomo-alpha" => Some(CoreType::Clash(ClashCoreType::MihomoAlpha)),
        "clash-rs" => Some(CoreType::Clash(ClashCoreType::ClashRust)),
        "clash-rs-alpha" => Some(CoreType::Clash(ClashCoreType::ClashRustAlpha)),
        "clash" => Some(CoreType::Clash(ClashCoreType::ClashPremium)),
        "chimera-client" | "chimera_client" => {
            Some(CoreType::Clash(ClashCoreType::ChimeraClient))
        }
        "singbox" => Some(CoreType::SingBox),
        _ => None,
    };
    friendly.or_else(|| serde_json::from_str(value).ok()).ok_or_else(|| {
        format!(
            "Failed to parse core type `{value}`; use a core name or the legacy JSON form"
        )
    })
}

fn parse_revision_id(value: &str) -> Result<RevisionIdInfo, String> {
    let parts: Vec<&str> = value.split(':').collect();
    let [epoch, generation, effective_hash] = parts.as_slice() else {
        return Err(format!(
            "`{value}` is not <epoch>:<generation>:<effective-hash>"
        ));
    };
    Ok(RevisionIdInfo {
        epoch: epoch
            .parse()
            .map_err(|_| format!("`{epoch}` is not an epoch"))?,
        generation: generation
            .parse()
            .map_err(|_| format!("`{generation}` is not a generation"))?,
        effective_hash: (*effective_hash).to_owned(),
    })
}

/// This module is a shortcut for client rpc calls.
/// It is useful for testing and debugging service rpc calls.
#[derive(Debug, Subcommand)]
pub enum RpcCommand {
    /// Start specific core with the given config file
    StartCore {
        /// The core type to start
        #[clap(long)]
        #[arg(value_parser = core_type_parser)]
        core_type: CoreType,

        /// The path to the core config file
        #[clap(long)]
        config_file: std::path::PathBuf,
    },
    /// Stop the running core
    StopCore,
    /// Restart the running core
    RestartCore,
    /// Apply a config to the running core
    ApplyConfig {
        #[clap(long)]
        #[arg(value_parser = core_type_parser)]
        core_type: CoreType,
        #[clap(long)]
        config_file: std::path::PathBuf,
        /// Only apply if the running revision still matches
        #[clap(long, value_parser = parse_revision_id)]
        expected_revision: Option<RevisionIdInfo>,
    },
    /// Dry-run a config without touching the running core
    CheckConfig {
        #[clap(long)]
        #[arg(value_parser = core_type_parser)]
        core_type: CoreType,
        #[clap(long)]
        config_file: std::path::PathBuf,
    },
    /// Clear the manager quarantine latch
    RecoverCore,
    /// Get the logs of the service
    InspectLogs,
    /// Retrieve all buffered logs of the service
    RetrieveLogs,
    /// Set the dns servers
    SetDns { dns_servers: Option<Vec<IpAddr>> },
}

pub async fn rpc(commands: RpcCommand) -> Result<(), crate::cmds::CommandError> {
    match commands {
        RpcCommand::StartCore {
            core_type,
            config_file,
        } => {
            let client = Client::service_default();
            let payload = chimera_ipc::api::core::start::CoreStartReq {
                core_type: Cow::Borrowed(&core_type),
                config_file: Cow::Borrowed(&config_file),
            };
            client
                .start_core(&payload)
                .await
                .map_err(|e| crate::cmds::CommandError::Other(e.into()))?;
        }
        RpcCommand::StopCore => {
            let client = Client::service_default();
            client
                .stop_core()
                .await
                .map_err(|e| crate::cmds::CommandError::Other(e.into()))?;
        }
        RpcCommand::RestartCore => {
            let client = Client::service_default();
            client
                .restart_core()
                .await
                .map_err(|e| crate::cmds::CommandError::Other(e.into()))?;
        }
        RpcCommand::ApplyConfig {
            core_type,
            config_file,
            expected_revision,
        } => {
            let client = Client::service_default();
            let payload = chimera_ipc::api::core::apply::CoreApplyReq {
                core_type: Cow::Borrowed(&core_type),
                config_file: Cow::Borrowed(&config_file),
                expected_revision,
            };
            let data = client
                .apply_core(&payload)
                .await
                .map_err(|e| crate::cmds::CommandError::Other(e.into()))?;
            println!(
                "{}",
                serde_json::to_string_pretty(&data)
                    .map_err(|e| crate::cmds::CommandError::Other(e.into()))?
            );
        }
        RpcCommand::CheckConfig {
            core_type,
            config_file,
        } => {
            let client = Client::service_default();
            let payload = chimera_ipc::api::core::check::CoreCheckReq {
                core_type: Cow::Borrowed(&core_type),
                config_file: Cow::Borrowed(&config_file),
            };
            client
                .check_core(&payload)
                .await
                .map_err(|e| crate::cmds::CommandError::Other(e.into()))?;
        }
        RpcCommand::RecoverCore => {
            Client::service_default()
                .recover_core()
                .await
                .map_err(|e| crate::cmds::CommandError::Other(e.into()))?;
        }
        RpcCommand::InspectLogs => {
            let client = Client::service_default();
            let logs = client
                .inspect_logs()
                .await
                .map_err(|e| crate::cmds::CommandError::Other(e.into()))?;
            for log in logs.logs {
                println!("{}", log.trim_matches('\n'));
            }
        }
        RpcCommand::RetrieveLogs => {
            let client = Client::service_default();
            let logs = client
                .retrieve_logs()
                .await
                .map_err(|e| crate::cmds::CommandError::Other(e.into()))?;
            for log in logs.logs {
                println!("{}", log.trim_matches('\n'));
            }
        }
        RpcCommand::SetDns { dns_servers } => {
            let client = Client::service_default();
            client
                .set_dns(&NetworkSetDnsReq {
                    dns_servers: dns_servers
                        .as_ref()
                        .map(|v| v.iter().map(Cow::Borrowed).collect()),
                })
                .await
                .map_err(|e| crate::cmds::CommandError::Other(e.into()))?;
        }
    }
    Ok(())
}
