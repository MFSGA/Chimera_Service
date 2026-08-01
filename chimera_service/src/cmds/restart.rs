use service_manager::{ServiceLabel, ServiceStatus, ServiceStatusCtx};

use crate::consts::SERVICE_LABEL;

use super::CommandError;

pub fn restart() -> Result<(), CommandError> {
    let label: ServiceLabel = SERVICE_LABEL.parse()?;
    let manager = crate::utils::get_service_manager()?;
    match manager.status(ServiceStatusCtx { label })? {
        ServiceStatus::NotInstalled => Err(CommandError::ServiceNotInstalled),
        ServiceStatus::Stopped(_) => {
            tracing::info!("service already stopped, starting it...");
            super::start::start()
        }
        ServiceStatus::Running => {
            tracing::info!("service is running, cycling it...");
            super::stop::stop()?;
            super::start::start()
        }
    }
}
