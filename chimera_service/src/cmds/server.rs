use std::{collections::BTreeSet, ffi::OsString, path::PathBuf, sync::OnceLock};

#[cfg(windows)]
use anyhow::Context;
use clap::Args;
use tokio_util::sync::CancellationToken;
use tracing_attributes::instrument;

use crate::server::consts::RuntimeInfos;

use super::CommandError;

#[derive(Args, Debug, Clone)]
pub struct ServerContext {
    /// nyanpasu config dir
    #[clap(long)]
    pub nyanpasu_config_dir: PathBuf,
    /// nyanpasu data dir
    #[clap(long)]
    pub nyanpasu_data_dir: PathBuf,
    /// The nyanpasu install directory, allowing to search the sidecar binary
    #[clap(long)]
    pub nyanpasu_app_dir: PathBuf,
    /// run as service
    #[clap(long, default_value = "false")]
    pub service: bool,
}

pub static SHUTDOWN_TOKEN: OnceLock<CancellationToken> = OnceLock::new();

/// Extract only names, as in the upstream Service runtime. Keeping the
/// projection pure makes it testable without changing process environment.
fn environment_names(pairs: impl IntoIterator<Item = (OsString, OsString)>) -> BTreeSet<String> {
    pairs
        .into_iter()
        .map(|(name, _)| name.to_string_lossy().into_owned())
        .collect()
}

pub async fn server_inner(
    ctx: ServerContext,
    token: CancellationToken,
) -> Result<(), CommandError> {
    chimera_utils::os::kill_by_pid_file(
        crate::utils::dirs::service_pid_file(),
        // TODO: use common name
        Some(&["mihomo", "clash"]),
    )
    .await?;
    tracing::info!("nyanpasu config dir: {:?}", ctx.nyanpasu_config_dir);
    tracing::info!("nyanpasu data dir: {:?}", ctx.nyanpasu_data_dir);

    // Upstream daemon rule: service logs are exposed to local IPC users, so
    // record environment *names* only, never proxy passwords, tokens or keys.
    let env_names = environment_names(std::env::vars_os());
    tracing::info!(environment_names = ?env_names, "collected current env names.");

    // check dirs accessibility
    let nyanpasu_config_dir = dunce::canonicalize(&ctx.nyanpasu_config_dir)?;
    let nyanpasu_data_dir = dunce::canonicalize(&ctx.nyanpasu_data_dir)?;
    let nyanpasu_app_dir = dunce::canonicalize(&ctx.nyanpasu_app_dir)?;

    let service_data_dir = crate::utils::dirs::service_data_dir();
    let service_config_dir = crate::utils::dirs::service_config_dir();
    tracing::info!("suggested service data dir: {:?}", service_data_dir);
    tracing::info!("suggested service config dir: {:?}", service_config_dir);

    if !service_data_dir.exists() {
        std::fs::create_dir_all(&service_data_dir)?;
    }
    if !service_config_dir.exists() {
        std::fs::create_dir_all(&service_config_dir)?;
    }

    // Write current process id to file
    if let Err(e) = chimera_utils::os::create_pid_file(
        crate::utils::dirs::service_pid_file(),
        std::process::id(),
    )
    .await
    {
        tracing::error!("create pid file error: {}", e);
    };

    crate::server::consts::RuntimeInfos::set_infos(RuntimeInfos {
        service_data_dir,
        service_config_dir,
        nyanpasu_config_dir,
        nyanpasu_data_dir,
        nyanpasu_app_dir,
    });

    #[cfg(windows)]
    let sids = crate::utils::acl::read_acl_file()
        .await
        .context("failed to read acl file")?;
    #[cfg(windows)]
    let sids_str = &sids.iter().map(|s| s.as_str()).collect::<Vec<_>>();
    #[cfg(not(windows))]
    let sids_str = ();

    #[cfg(windows)]
    tracing::info!(sids = ?sids_str, "Loaded acl file");

    crate::server::run(
        (*RuntimeInfos::global()).clone(),
        chimera_core_manager::LocalIpcPolicy::Disable,
        token,
        sids_str,
    )
    .await?;
    Ok(())
}

#[instrument]
pub async fn server(ctx: ServerContext) -> Result<(), CommandError> {
    let token = CancellationToken::new();
    SHUTDOWN_TOKEN.set(token.clone()).unwrap();
    server_inner(ctx, token).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn service_environment_logs_never_contain_secret_values() {
        let names = environment_names([
            (
                OsString::from("HTTP_PROXY"),
                OsString::from("https://user:password@example.com"),
            ),
            (
                OsString::from("SERVICE_TOKEN"),
                OsString::from("sensitive-token"),
            ),
            (
                OsString::from("SERVICE_TOKEN"),
                OsString::from("another-secret"),
            ),
        ]);
        assert_eq!(
            names,
            BTreeSet::from(["HTTP_PROXY".into(), "SERVICE_TOKEN".into()])
        );
        let output = format!("{names:?}");
        assert!(!output.contains("password"));
        assert!(!output.contains("sensitive-token"));
        assert!(!output.contains("another-secret"));
    }
}
