use std::{path::Path, sync::Arc};

use camino::{Utf8Path, Utf8PathBuf};
use chimera_ipc::api::{
    core::apply::{ApplyOutcomeKind, CoreApplyData},
    error_kind,
    status::{CoreInfos, RevisionIdInfo},
};
use chimera_utils::core::{ClashCoreType, CoreType};
use nyanpasu_core_manager::{
    CoreKind, CoreManager, CoreSpec, Error as ManagerError, InstanceOptions, InstanceSpec,
    ManagerOptions,
};
use tokio::sync::{Semaphore, broadcast, watch};
use tokio_util::sync::CancellationToken;

use super::{
    consts::RuntimeInfos,
    manager_projection::{
        map_apply_outcome, map_error_kind, map_revision_id, project_core_infos,
    },
};

const MAX_CONCURRENT_CHECKS: usize = 2;

#[derive(Debug, thiserror::Error)]
#[error("{message}")]
pub struct OperationError {
    kind: Option<&'static str>,
    message: String,
}

impl OperationError {
    pub fn kind(&self) -> Option<&'static str> {
        self.kind
    }

    fn plain(message: impl Into<String>) -> Self {
        Self {
            kind: None,
            message: message.into(),
        }
    }

    fn with_kind(kind: &'static str, message: impl Into<String>) -> Self {
        Self {
            kind: Some(kind),
            message: message.into(),
        }
    }
}

impl From<ManagerError> for OperationError {
    fn from(error: ManagerError) -> Self {
        Self {
            kind: map_error_kind(&error),
            message: error.to_string(),
        }
    }
}

#[derive(Clone)]
pub struct CoreManagerService {
    manager: Arc<CoreManager>,
    requested_core: watch::Sender<Option<CoreType>>,
    check_slots: Arc<Semaphore>,
}

impl CoreManagerService {
    pub async fn new(cancel_token: CancellationToken) -> Result<Self, anyhow::Error> {
        let infos = RuntimeInfos::global();
        let runtime_dir = Utf8PathBuf::from_path_buf(infos.service_data_dir.join("core-runtime"))
            .map_err(|path| anyhow::anyhow!("runtime directory is not UTF-8: {}", path.display()))?;
        let manager = CoreManager::new(ManagerOptions {
            runtime_dir: Some(runtime_dir),
            cancel_token,
            ..ManagerOptions::default()
        })
        .await?;
        Ok(Self {
            manager: Arc::new(manager),
            requested_core: watch::Sender::new(None),
            check_slots: Arc::new(Semaphore::new(MAX_CONCURRENT_CHECKS)),
        })
    }

    pub fn subscribe(&self) -> watch::Receiver<nyanpasu_core_manager::CoreStatus> {
        self.manager.subscribe()
    }

    pub fn subscribe_requested_core(&self) -> watch::Receiver<Option<CoreType>> {
        self.requested_core.subscribe()
    }

    pub fn subscribe_logs(&self) -> broadcast::Receiver<nyanpasu_core_manager::LogFrame> {
        self.manager.subscribe_logs()
    }

    pub async fn status(&self) -> CoreInfos {
        project_core_infos(&self.manager.status(), self.requested_core.borrow().clone())
    }

    pub async fn start(
        &self,
        core_type: &CoreType,
        config_path: &Utf8Path,
    ) -> Result<(), anyhow::Error> {
        let spec = self.instance_spec(
            core_type,
            canonical_config_path(config_path.as_std_path()).await?,
        )?;
        self.manager.start(spec).await.map_err(|error| match error {
            ManagerError::AlreadyRunning => anyhow::anyhow!("core is already running"),
            other => other.into(),
        })?;
        self.publish_requested_core(Some(core_type));
        Ok(())
    }

    pub async fn stop(&self) -> Result<(), anyhow::Error> {
        self.manager.stop().await.map_err(|error| match error {
            ManagerError::NotStarted => anyhow::anyhow!("core is already stopped"),
            other => other.into(),
        })
    }

    pub async fn restart(&self) -> Result<(), anyhow::Error> {
        self.manager.restart().await.map(|_| ()).map_err(|error| match error {
            ManagerError::NotStarted => anyhow::anyhow!("core have not been started yet"),
            other => other.into(),
        })
    }

    pub async fn check(
        &self,
        core_type: &CoreType,
        config_file: &Utf8Path,
    ) -> Result<(), OperationError> {
        let _slot = self.check_slots.try_acquire().map_err(|_| {
            OperationError::plain(format!(
                "at most {MAX_CONCURRENT_CHECKS} config checks may run at once; retry"
            ))
        })?;
        let config_path = canonical_config_path(config_file.as_std_path())
            .await
            .map_err(|error| {
                OperationError::with_kind(error_kind::CONFIG_NOT_FOUND, error.to_string())
            })?;
        let spec = self.instance_spec(core_type, config_path)?;
        self.manager.check_config(&spec).await.map_err(Into::into)
    }

    pub async fn apply(
        &self,
        core_type: &CoreType,
        config_file: &Utf8Path,
        expected_revision: Option<&RevisionIdInfo>,
    ) -> Result<CoreApplyData, OperationError> {
        let config_path = canonical_config_path(config_file.as_std_path())
            .await
            .map_err(|error| {
                OperationError::with_kind(error_kind::CONFIG_NOT_FOUND, error.to_string())
            })?;
        let spec = self.instance_spec(core_type, config_path)?;
        let outcome = self
            .manager
            .apply_config(spec, expected_revision.map(map_revision_id))
            .await?;
        let data = map_apply_outcome(&outcome);
        if data.outcome != ApplyOutcomeKind::RolledBack {
            self.publish_requested_core(Some(core_type));
        } else {
            self.publish_requested_core(None);
        }
        Ok(data)
    }

    pub async fn recover(&self) -> Result<(), OperationError> {
        // Quarantine is introduced with confirmed-death shutdown. Until that
        // layer is migrated this operation is intentionally idempotent.
        Ok(())
    }

    pub async fn shutdown(&self) {
        if let Err(error) = self.manager.stop().await
            && !matches!(error, ManagerError::NotStarted)
        {
            tracing::error!("failed to stop core during shutdown: {error}");
        }
    }

    fn publish_requested_core(&self, committed: Option<&CoreType>) {
        self.requested_core.send_modify(|current| {
            if let Some(core_type) = committed {
                *current = Some(core_type.clone());
            }
        });
    }

    fn instance_spec(
        &self,
        core_type: &CoreType,
        config_path: Utf8PathBuf,
    ) -> Result<InstanceSpec, OperationError> {
        let infos = RuntimeInfos::global();
        let binary_path = find_binary_path(infos, core_type).map_err(|error| {
            OperationError::with_kind(error_kind::BINARY_NOT_FOUND, error.to_string())
        })?;
        let binary_path = Utf8PathBuf::from_path_buf(binary_path).map_err(|path| {
            OperationError::plain(format!("core binary path is not UTF-8: {}", path.display()))
        })?;
        let working_dir = Utf8PathBuf::from_path_buf(infos.nyanpasu_data_dir.clone()).map_err(
            |path| OperationError::plain(format!("working directory is not UTF-8: {}", path.display())),
        )?;
        Ok(InstanceSpec {
            core: CoreSpec {
                kind: core_kind(core_type)?,
                binary_path,
                version: None,
                features: Vec::new(),
            },
            config_path,
            working_dir,
            pid_file: None,
            options: InstanceOptions::default(),
        })
    }
}

fn core_kind(core_type: &CoreType) -> Result<CoreKind, OperationError> {
    match core_type {
        CoreType::Clash(ClashCoreType::Mihomo | ClashCoreType::MihomoAlpha) => Ok(CoreKind::Mihomo),
        CoreType::Clash(ClashCoreType::ClashRust | ClashCoreType::ClashRustAlpha) => {
            Ok(CoreKind::ClashRust)
        }
        CoreType::Clash(ClashCoreType::ClashPremium) => Ok(CoreKind::ClashPremium),
        CoreType::Clash(ClashCoreType::ChimeraClient) => Err(OperationError::plain(
            "chimera-client is not supported by the new core manager yet",
        )),
        CoreType::SingBox => Err(OperationError::plain("sing-box is not a supported core")),
    }
}

fn find_binary_path(infos: &RuntimeInfos, core_type: &CoreType) -> std::io::Result<std::path::PathBuf> {
    for directory in [&infos.nyanpasu_data_dir, &infos.nyanpasu_app_dir] {
        let candidate = directory.join(core_type.get_executable_name());
        if candidate.exists() {
            return Ok(candidate);
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::NotFound,
        format!("{} not found", core_type.get_executable_name()),
    ))
}

async fn canonical_config_path(path: &Path) -> std::io::Result<Utf8PathBuf> {
    let canonical = tokio::fs::canonicalize(path).await?;
    Utf8PathBuf::from_path_buf(dunce::simplified(&canonical).to_path_buf()).map_err(|path| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("config path is not UTF-8: {}", path.display()),
        )
    })
}
