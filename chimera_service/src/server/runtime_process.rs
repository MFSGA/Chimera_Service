//! Service-owned core process adapter.
//!
//! The legacy `chimera_utils::CoreInstance` writes a numeric pid file. That is
//! intentionally insufficient for post-crash reaping because a recycled PID
//! could belong to another process. This adapter keeps Chimera's current launch
//! arguments while using `nyanpasu_utils::process` to publish the same
//! structured epoch identity record as the reference core manager.

use std::{ffi::OsString, sync::Arc};

use camino::{Utf8Path, Utf8PathBuf};
use chimera_utils::core::{
    ClashCoreType, CoreType,
    instance::{CoreInstance, MIHOMO_SAFE_PATHS_ENV_NAME},
};
use nyanpasu_utils::process::{
    Command, EpochPidRecord, ProcessEvent, ProcessHandle, inspect_process_identity,
    publish_epoch_pid_file, remove_epoch_pid_file_if_matches,
};
use parking_lot::RwLock;
use tokio::sync::mpsc::Receiver;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ServiceProcessState {
    Running,
    Stopped,
}

pub(super) struct ServiceRuntimeProcess {
    pub(super) core_type: CoreType,
    handle: ProcessHandle,
    pid_path: Utf8PathBuf,
    pid_record: EpochPidRecord,
    state: Arc<RwLock<ServiceProcessState>>,
}

fn run_args(
    core_type: &CoreType,
    app_dir: &Utf8Path,
    config_path: &Utf8Path,
) -> anyhow::Result<Vec<OsString>> {
    let args = match core_type {
        CoreType::Clash(ClashCoreType::Mihomo | ClashCoreType::MihomoAlpha) => vec![
            OsString::from("-m"),
            OsString::from("-d"),
            app_dir.as_os_str().to_owned(),
            OsString::from("-f"),
            config_path.as_os_str().to_owned(),
        ],
        CoreType::Clash(
            ClashCoreType::ClashRust | ClashCoreType::ClashRustAlpha | ClashCoreType::ChimeraClient,
        ) => vec![
            OsString::from("-d"),
            app_dir.as_os_str().to_owned(),
            OsString::from("-c"),
            config_path.as_os_str().to_owned(),
        ],
        CoreType::Clash(ClashCoreType::ClashPremium) => vec![
            OsString::from("-d"),
            app_dir.as_os_str().to_owned(),
            OsString::from("-f"),
            config_path.as_os_str().to_owned(),
        ],
        CoreType::SingBox => anyhow::bail!("SingBox is not supported yet"),
    };
    Ok(args)
}

impl ServiceRuntimeProcess {
    pub(super) async fn spawn(
        core_type: CoreType,
        app_dir: Utf8PathBuf,
        binary_path: Utf8PathBuf,
        config_path: Utf8PathBuf,
        pid_path: Utf8PathBuf,
        epoch: u64,
    ) -> anyhow::Result<(Arc<Self>, Receiver<ProcessEvent>)> {
        let args = run_args(&core_type, app_dir.as_path(), config_path.as_path())?;
        let config_dir = config_path
            .parent()
            .ok_or_else(|| anyhow::anyhow!("runtime config path has no parent"))?;
        let safe_paths = CoreInstance::get_mihomo_safe_paths(&app_dir, config_dir, None);
        let command = Command::new(binary_path.as_str())
            .args(args)
            .env(MIHOMO_SAFE_PATHS_ENV_NAME, safe_paths)
            .env("CLICOLOR_FORCE", "0")
            .current_dir(app_dir.as_str());

        let (handle, events) = command.spawn().await?;
        let pid = handle.pid();
        let identity = match inspect_process_identity(pid).await {
            Ok(Some(identity)) => identity,
            Ok(None) => {
                let _ = handle.kill().await;
                anyhow::bail!("spawned core exited before its process identity could be captured");
            }
            Err(error) => {
                let _ = handle.kill().await;
                return Err(error.into());
            }
        };
        let pid_record = EpochPidRecord {
            pid,
            epoch,
            executable: identity.executable,
            start_token: identity.start_token,
            runtime_config: config_path.as_std_path().to_owned(),
        };
        if let Err(error) = publish_epoch_pid_file(pid_path.as_std_path(), &pid_record).await {
            let cleanup = handle.kill().await;
            return match cleanup {
                Ok(()) => Err(error.into()),
                Err(cleanup_error) => Err(anyhow::anyhow!(
                    "failed to publish epoch pid record: {error}; failed to terminate unmanaged core process: {cleanup_error}"
                )),
            };
        }

        let process = Arc::new(Self {
            core_type,
            handle,
            pid_path,
            pid_record,
            state: Arc::new(RwLock::new(ServiceProcessState::Running)),
        });
        Ok((process, events))
    }

    pub(super) fn state(&self) -> ServiceProcessState {
        *self.state.read()
    }

    pub(super) fn mark_stopped(&self) {
        *self.state.write() = ServiceProcessState::Stopped;
    }

    pub(super) async fn cleanup_pid_record(&self) -> anyhow::Result<()> {
        remove_epoch_pid_file_if_matches(self.pid_path.as_std_path(), &self.pid_record).await?;
        Ok(())
    }

    pub(super) async fn kill(&self) -> anyhow::Result<()> {
        self.handle.graceful_kill().await?;
        self.mark_stopped();
        self.cleanup_pid_record().await?;
        Ok(())
    }
}

pub(super) fn epoch_pid_path(runtime_dir: &Utf8Path, epoch: u64) -> Utf8PathBuf {
    runtime_dir.join(format!("core-{epoch}.pid"))
}
