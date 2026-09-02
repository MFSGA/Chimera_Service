mod cmds;
pub mod consts;
mod logging;
mod server;
pub mod utils;

use consts::ExitCode;
use tokio_util::sync::CancellationToken;
use tracing::error;

pub async fn handler() -> ExitCode {
    crate::utils::deadlock_detection();
    let result = cmds::process().await;
    match result {
        Ok(_) => ExitCode::Normal,
        Err(cmds::CommandError::PermissionDenied) => {
            eprintln!("Permission denied, please run as administrator or root");
            ExitCode::PermissionDenied
        }
        Err(cmds::CommandError::ServiceNotInstalled) => {
            eprintln!("Service not installed");
            ExitCode::ServiceNotInstalled
        }
        Err(cmds::CommandError::ServiceAlreadyInstalled) => {
            eprintln!("Service already installed");
            ExitCode::ServiceAlreadyInstalled
        }
        Err(cmds::CommandError::ServiceAlreadyStopped) => {
            eprintln!("Service already stopped");
            ExitCode::ServiceAlreadyStopped
        }
        Err(cmds::CommandError::ServiceAlreadyRunning) => {
            eprintln!("Service already running");
            ExitCode::ServiceAlreadyRunning
        }
        Err(error) => {
            error!("Error: {error:#?}");
            ExitCode::Other
        }
    }
}

/// The running server's cancellation token, published by the `server` command.
pub fn server_shutdown_token() -> Option<CancellationToken> {
    cmds::SERVER_SHUTDOWN_TOKEN.get().cloned()
}

#[cfg(feature = "dev")]
pub fn init_dev_logging() -> anyhow::Result<()> {
    logging::init(true, false)
}

#[cfg(test)]
mod tests {
    #[test]
    fn runtime_library_keeps_the_service_module_path_root() {
        assert_eq!(module_path!().split("::").next(), Some("chimera_service"));
    }
}
