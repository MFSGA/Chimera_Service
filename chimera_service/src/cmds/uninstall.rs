use std::{thread, time::Duration};

use service_manager::{
    ServiceLabel, ServiceStatus, ServiceStatusCtx, ServiceStopCtx, ServiceUninstallCtx,
};

use crate::consts::SERVICE_LABEL;

use super::CommandError;

pub fn uninstall() -> Result<(), CommandError> {
    let label: ServiceLabel = SERVICE_LABEL.parse()?;
    let manager = crate::utils::get_service_manager()?;
    let status = manager.status(ServiceStatusCtx {
        label: label.clone(),
    })?;

    match status {
        ServiceStatus::NotInstalled => {
            tracing::info!("service not installed, nothing to do");
            return Err(CommandError::ServiceNotInstalled);
        }
        ServiceStatus::Stopped(_) => {
            tracing::info!("service already stopped, uninstalling directly");
        }
        ServiceStatus::Running => {
            tracing::info!("service is running, stopping it before uninstall");
            manager.stop(ServiceStopCtx {
                label: label.clone(),
            })?;
            thread::sleep(Duration::from_secs(3));
        }
    }

    manager.uninstall(ServiceUninstallCtx {
        label: label.clone(),
    })?;

    let status = manager.status(ServiceStatusCtx { label })?;
    if status != ServiceStatus::NotInstalled {
        return Err(CommandError::Other(anyhow::anyhow!(
            "service uninstall failed, status: {:?}",
            status
        )));
    }

    tracing::info!("service uninstalled");
    Ok(())
}
