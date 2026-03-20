use std::thread;

use service_manager::{ServiceLabel, ServiceStartCtx, ServiceStatus, ServiceStatusCtx};

use crate::consts::SERVICE_LABEL;

use super::CommandError;

pub fn start() -> Result<(), CommandError> {
    tracing::debug!(service = SERVICE_LABEL, "start command received");

    let label: ServiceLabel = SERVICE_LABEL.parse()?;
    let manager = crate::utils::get_service_manager()?;
    let status = manager.status(ServiceStatusCtx {
        label: label.clone(),
    })?;
    tracing::info!(service = %label, ?status, "queried service status before start");
    match status {
        ServiceStatus::NotInstalled => {
            tracing::error!(service = %label, "service is not installed");
            return Err(CommandError::ServiceNotInstalled);
        }
        ServiceStatus::Stopped(_) => {
            tracing::info!(service = %label, "service is stopped, sending start request");
            manager.start(ServiceStartCtx {
                label: label.clone(),
            })?;
            tracing::info!(service = %label, "service start request sent");
        }
        ServiceStatus::Running => {
            tracing::info!(service = %label, "service already running, nothing to do");
            return Err(CommandError::ServiceAlreadyRunning);
        }
    }
    tracing::info!(service = %label, wait_seconds = 3, "waiting for service manager to report running");
    thread::sleep(std::time::Duration::from_secs(3));
    // check if the service is running
    let status = manager.status(ServiceStatusCtx {
        label: label.clone(),
    })?;
    tracing::info!(service = %label, ?status, "queried service status after start");
    if status != ServiceStatus::Running {
        tracing::error!(service = %label, ?status, "service failed to reach running state");
        return Err(CommandError::Other(anyhow::anyhow!(
            "service start failed, status: {:?}",
            status
        )));
    }
    tracing::info!(service = %label, "service started successfully");

    Ok(())
}
