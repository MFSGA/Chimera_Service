use std::{env::current_exe, ffi::OsString, path::PathBuf};

use service_manager::{ServiceInstallCtx, ServiceLabel, ServiceStatus, ServiceStatusCtx};

use crate::consts::{APP_NAME, SERVICE_LABEL};

use super::CommandError;

#[derive(Debug, clap::Args)]
pub struct InstallCommand {
    /// The user who will run the service
    #[clap(long)]
    user: String,
    /// todo: rename The nyanpasu data directory
    #[clap(long)]
    nyanpasu_data_dir: PathBuf,
    /// todo: rename The nyanpasu config directory
    #[clap(long)]
    nyanpasu_config_dir: PathBuf,
    /// todo: rename The nyanpasu install directory, allowing to search the sidecar binary
    #[clap(long)]
    nyanpasu_app_dir: PathBuf,
}

pub fn install(ctx: InstallCommand) -> Result<(), CommandError> {
    tracing::info!("nyanpasu data dir: {:?}", ctx.nyanpasu_data_dir);
    tracing::info!("nyanpasu config dir: {:?}", ctx.nyanpasu_config_dir);

    let label: ServiceLabel = SERVICE_LABEL.parse()?;
    let manager = crate::utils::get_service_manager()?;

    if !matches!(
        manager.status(ServiceStatusCtx {
            label: label.clone(),
        })?,
        ServiceStatus::NotInstalled
    ) {
        return Err(CommandError::ServiceAlreadyInstalled);
    }

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

    let binary_name = format!("{}{}", APP_NAME, std::env::consts::EXE_SUFFIX);
    #[cfg(not(target_os = "linux"))]
    let service_binary = service_data_dir.join(binary_name);

    // todo: perhaps can not use in nixos
    #[cfg(target_os = "linux")]
    let service_binary = PathBuf::from("/usr/bin").join(binary_name);

    let current_binary = current_exe()?;
    if current_binary != service_binary {
        tracing::info!("copying service binary to: {:?}", service_binary);
        std::fs::copy(current_binary, &service_binary)?;
    }

    #[cfg(windows)]
    {
        let rt = tokio::runtime::Runtime::new()?;
        tracing::info!("ensuring acl file exists...");
        rt.block_on(crate::utils::acl::create_acl_file())?;

        let mut entries = std::collections::BTreeSet::from_iter(
            rt.block_on(crate::utils::acl::read_acl_file())?,
        );
        entries.insert(ctx.user.clone());

        let entries = entries.into_iter().collect::<Vec<_>>();
        tracing::info!(entries = ?entries, "writing acl file...");
        rt.block_on(crate::utils::acl::write_acl_file(entries.as_slice()))?;
    }

    let mut envs = Vec::new();
    if let Ok(home) = std::env::var("HOME") {
        envs.push(("HOME".to_string(), home));
    }

    tracing::info!("installing service...");
    manager.install(ServiceInstallCtx {
        label: label.clone(),
        program: service_binary,
        args: vec![
            OsString::from("server"),
            OsString::from("--nyanpasu-data-dir"),
            ctx.nyanpasu_data_dir.into(),
            OsString::from("--nyanpasu-config-dir"),
            ctx.nyanpasu_config_dir.into(),
            OsString::from("--nyanpasu-app-dir"),
            ctx.nyanpasu_app_dir.into(),
            OsString::from("--service"),
        ],
        contents: None,
        username: None,
        working_directory: Some(service_data_dir),
        environment: Some(envs),
        autostart: true,
        disable_restart_on_failure: false,
    })?;

    if matches!(
        manager.status(ServiceStatusCtx { label })?,
        ServiceStatus::NotInstalled
    ) {
        return Err(CommandError::Other(anyhow::anyhow!(
            "service install failed"
        )));
    }

    tracing::info!("service installed");
    Ok(())
}
