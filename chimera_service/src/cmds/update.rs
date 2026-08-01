use std::path::PathBuf;

use chimera_ipc::client::shortcuts::Client;
use semver::Version;
use tokio::task::spawn_blocking;

use crate::consts::{APP_NAME, APP_VERSION};

use super::CommandError;

#[derive(Debug, clap::Args)]
pub struct UpdateCommand {
    /// Report what an update would do without modifying files or services
    #[clap(long, default_value = "false")]
    pub(super) check: bool,

    /// Copy this binary over the installed service instead of the running one
    #[clap(long, value_name = "PATH")]
    from: Option<PathBuf>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum UpdatePlan {
    Seed,
    ReplaceOffline,
    StopReplaceStart,
    UpToDate,
}

impl UpdatePlan {
    fn summary(self) -> &'static str {
        match self {
            Self::Seed => "would install: no service binary is present",
            Self::ReplaceOffline => "would replace: the service is not answering",
            Self::StopReplaceStart => {
                "would update: stop the service, replace the binary, start it again"
            }
            Self::UpToDate => {
                "up to date: the installed service is not older than the running binary"
            }
        }
    }
}

fn plan_update(binary_exists: bool, source: &Version, installed: Option<&Version>) -> UpdatePlan {
    if !binary_exists {
        return UpdatePlan::Seed;
    }
    match installed {
        None => UpdatePlan::ReplaceOffline,
        Some(installed) if source > installed => UpdatePlan::StopReplaceStart,
        Some(_) => UpdatePlan::UpToDate,
    }
}

fn source_path(explicit: &Option<PathBuf>) -> Result<PathBuf, CommandError> {
    match explicit {
        Some(path) => Ok(path.clone()),
        None => Ok(std::env::current_exe()?),
    }
}

pub async fn update(ctx: UpdateCommand) -> Result<(), CommandError> {
    tracing::info!("Checking for updates...");
    let explicit_source = match ctx.from {
        Some(from) => {
            if !from.is_file() {
                return Err(CommandError::Other(anyhow::anyhow!(
                    "update source does not exist or is not a file: {}",
                    from.display()
                )));
            }
            Some(from)
        }
        None => None,
    };

    let service_data_dir = crate::utils::dirs::service_data_dir();
    let service_binary =
        service_data_dir.join(format!("{}{}", APP_NAME, std::env::consts::EXE_SUFFIX));
    let binary_exists = service_binary.exists();
    let client_version = Version::parse(APP_VERSION)
        .map_err(|error| CommandError::Other(error.into()))?;
    let installed = if binary_exists {
        Client::service_default()
            .status()
            .await
            .ok()
            .and_then(|status| Version::parse(&status.version).ok())
    } else {
        None
    };
    let plan = plan_update(binary_exists, &client_version, installed.as_ref());

    if ctx.check {
        let source = source_path(&explicit_source)?;
        println!("service binary: {}", service_binary.display());
        println!("update source: {}", source.display());
        println!("comparison version (running binary): {APP_VERSION}");
        match installed.as_ref() {
            Some(installed) => println!("installed version: {installed}"),
            None => println!("installed version: unknown (the service did not answer)"),
        }
        println!("{}", plan.summary());
        return Ok(());
    }

    if !service_data_dir.exists() {
        tokio::fs::create_dir_all(&service_data_dir).await?;
    }
    match plan {
        UpdatePlan::Seed => {
            tracing::info!("Service binary not found; seeding it...");
            tokio::fs::copy(source_path(&explicit_source)?, &service_binary).await?;
        }
        UpdatePlan::ReplaceOffline => {
            tracing::info!("Service is offline; replacing its binary directly...");
            tokio::fs::copy(source_path(&explicit_source)?, &service_binary).await?;
        }
        UpdatePlan::StopReplaceStart => {
            tracing::info!("Stopping the service before updating...");
            spawn_blocking(super::stop::stop).await??;
            tracing::info!("Replacing the service binary...");
            tokio::fs::copy(source_path(&explicit_source)?, &service_binary).await?;
            tracing::info!("Starting the updated service...");
            spawn_blocking(super::start::start).await??;
        }
        UpdatePlan::UpToDate => {
            tracing::info!("Installed service is already up to date.");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn version(raw: &str) -> Version {
        Version::parse(raw).unwrap()
    }

    #[test]
    fn update_plan_matches_the_reference_branches() {
        let source = version("1.4.5");
        assert_eq!(plan_update(false, &source, None), UpdatePlan::Seed);
        assert_eq!(plan_update(true, &source, None), UpdatePlan::ReplaceOffline);
        assert_eq!(
            plan_update(true, &source, Some(&version("1.4.4"))),
            UpdatePlan::StopReplaceStart
        );
        assert_eq!(
            plan_update(true, &source, Some(&version("1.4.5"))),
            UpdatePlan::UpToDate
        );
        assert_eq!(
            plan_update(true, &source, Some(&version("1.5.0"))),
            UpdatePlan::UpToDate
        );
    }
}
