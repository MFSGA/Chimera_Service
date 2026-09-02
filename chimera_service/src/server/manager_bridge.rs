use std::{path::Path, sync::{Arc, Mutex}, time::Duration};

use camino::{Utf8Path, Utf8PathBuf};
use chimera_ipc::api::{
    CoreErrorKind,
    core::{
        apply::{ApplyOutcomeKind, CoreApplyData},
        v2::{
            CoreCommandInfo, CoreOperationReq, CoreSubmitReq, OperationErrorInfo, OperationInfo,
            OperationOutputInfo, OperationPhase, ReconcileOutcomeInfo, ReconcileOutcomeKind,
        },
    },
    status::{CoreInfos, RevisionIdInfo},
};
use chimera_utils::core::{ClashCoreType, CoreType};
use nyanpasu_core_manager::{
    ApplyOutcome, ConfigInput, ControlOptions, CoreCommand, CoreCommandEnvelope, CoreControl,
    CoreError as ControlError, CoreKind, CoreManager, CoreSpec, Error as ManagerError,
    ExecutorExit,
    InstanceOptions, InstanceSpec, ManagerOptions, OperationHandle, OperationId, OperationOutput,
    OperationState, ReconcileRequest,
};
use tokio::sync::{Semaphore, broadcast, watch};
use tokio_util::sync::CancellationToken;

use super::{
    consts::RuntimeInfos,
    manager_projection::{
        map_apply_outcome, map_error_kind, map_revision, map_revision_id, project_core_infos,
    },
};

const MAX_CONCURRENT_CHECKS: usize = 2;
const MAX_OPERATION_WAIT_MS: u64 = 90_000;

#[derive(Debug, thiserror::Error)]
#[error("{message}")]
pub struct OperationError {
    kind: Option<CoreErrorKind>,
    message: String,
    retryable: Option<bool>,
}

impl OperationError {
    pub fn kind(&self) -> Option<CoreErrorKind> {
        self.kind
    }

    pub fn retryable(&self) -> Option<bool> {
        self.retryable
    }

    fn plain(message: impl Into<String>) -> Self {
        Self {
            kind: None,
            message: message.into(),
            retryable: None,
        }
    }

    fn with_kind(kind: CoreErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind: Some(kind),
            message: message.into(),
            retryable: None,
        }
    }
}

impl From<ManagerError> for OperationError {
    fn from(error: ManagerError) -> Self {
        Self {
            kind: map_error_kind(&error),
            message: error.to_string(),
            retryable: None,
        }
    }
}

impl From<ControlError> for OperationError {
    fn from(error: ControlError) -> Self {
        Self {
            kind: error.kind,
            message: error.message,
            retryable: Some(error.retryable),
        }
    }
}

#[derive(Clone)]
pub struct CoreManagerService {
    manager: CoreManager,
    control: CoreControl,
    runtime: Arc<RuntimeInfos>,
    requested_core: watch::Sender<Option<CoreType>>,
    echo_applied: Arc<Mutex<Option<u64>>>,
    check_slots: Arc<Semaphore>,
}

impl CoreManagerService {
    pub async fn new(
        runtime: Arc<RuntimeInfos>,
        local_ipc_policy: nyanpasu_core_manager::LocalIpcPolicy,
        cancel_token: CancellationToken,
    ) -> Result<Self, anyhow::Error> {
        let runtime_dir = Utf8PathBuf::from_path_buf(runtime.service_data_dir.join("core-runtime"))
            .map_err(|path| anyhow::anyhow!("runtime directory is not UTF-8: {}", path.display()))?;
        let working_dir = Utf8PathBuf::from_path_buf(runtime.nyanpasu_data_dir.clone()).map_err(
            |path| anyhow::anyhow!("working directory is not UTF-8: {}", path.display()),
        )?;
        let source_dir = runtime_dir.join("v2-sources");
        let manager = CoreManager::new(ManagerOptions {
            runtime_dir: Some(runtime_dir),
            local_ipc_policy,
            cancel_token,
            ..ManagerOptions::default()
        })
        .await?;
        let control = CoreControl::spawn(
            manager.clone(),
            ControlOptions::new(source_dir, working_dir),
        );
        Ok(Self {
            manager,
            control,
            runtime,
            requested_core: watch::Sender::new(None),
            echo_applied: Arc::new(Mutex::new(None)),
            check_slots: Arc::new(Semaphore::new(MAX_CONCURRENT_CHECKS)),
        })
    }

    pub fn subscribe(&self) -> watch::Receiver<nyanpasu_core_manager::CoreStatus> {
        self.manager.subscribe()
    }

    pub fn subscribe_requested_core(&self) -> watch::Receiver<Option<CoreType>> {
        self.requested_core.subscribe()
    }

    pub fn subscribe_logs(
        &self,
    ) -> broadcast::Receiver<std::sync::Arc<nyanpasu_core_manager::LogFrame>> {
        self.manager.subscribe_logs()
    }

    pub async fn status(&self) -> CoreInfos {
        project_core_infos(&self.manager.status(), self.requested_core.borrow().clone())
    }

    pub fn core_log_dir(&self) -> Option<std::path::PathBuf> {
        self.manager
            .log_dir()
            .map(|dir| dir.as_std_path().to_path_buf())
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
            .map_err(map_config_path_error)?;
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
            .map_err(map_config_path_error)?;
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
        self.manager.recover_quarantine().await.map_err(Into::into)
    }

    pub fn submit_v2(&self, request: &CoreSubmitReq<'_>) -> Result<OperationInfo, OperationError> {
        let id: OperationId = request.operation_id.parse().map_err(|_| {
            OperationError::plain(format!("malformed operation id: {}", request.operation_id))
        })?;
        let mut echoed_core = None;
        let command = match &request.command {
            CoreCommandInfo::Reconcile {
                core_type,
                config,
                expected_digest,
                expected_applied,
            } => {
                let spec = self.instance_spec(core_type, Utf8PathBuf::new())?;
                echoed_core = Some(core_type.clone().into_owned());
                CoreCommand::Reconcile(Box::new(ReconcileRequest {
                    core: spec.core,
                    config: ConfigInput::Inline {
                        bytes: config.as_bytes().to_vec(),
                        expected_digest: expected_digest.as_ref().map(ToString::to_string),
                    },
                    options: spec.options,
                    expected_applied: expected_applied.as_ref().map(map_revision_id),
                }))
            }
            CoreCommandInfo::Stop => CoreCommand::Stop,
            CoreCommandInfo::Recover => CoreCommand::Recover,
        };
        let handle = self.control.submit(CoreCommandEnvelope {
            operation_id: id,
            command,
        })?;
        let admitted = map_operation(handle.id(), handle.state());
        if let Some(core_type) = echoed_core
            && handle.newly_admitted()
        {
            self.watch_reconcile_echo(handle, core_type);
        }
        Ok(admitted)
    }

    pub async fn operation_v2(
        &self,
        request: &CoreOperationReq<'_>,
    ) -> Result<OperationInfo, OperationError> {
        let id: OperationId = request.operation_id.parse().map_err(|_| {
            OperationError::plain(format!("malformed operation id: {}", request.operation_id))
        })?;
        let state = match request.wait_ms {
            Some(wait_ms) => {
                self.control
                    .wait_operation(
                        id,
                        Duration::from_millis(wait_ms.min(MAX_OPERATION_WAIT_MS)),
                    )
                    .await
            }
            None => self.control.operation(id),
        };
        state.map(|state| map_operation(id, state)).ok_or_else(|| {
            OperationError::plain(format!(
                "unknown operation {id}: the registry entry was evicted or never existed; re-read /v2/core/status and rely on the revision CAS"
            ))
        })
    }

    fn watch_reconcile_echo(&self, handle: OperationHandle, core_type: CoreType) {
        let service = self.clone();
        let sequence = handle.sequence();
        tokio::spawn(async move {
            let state = match handle.wait().await {
                Ok(output) => OperationState::Succeeded(output),
                Err(error) => OperationState::Failed(error),
            };
            let commits = echo_commits(&state);
            if !commits
                && !matches!(
                    state,
                    OperationState::Succeeded(OperationOutput::Reconciled(_))
                )
            {
                return;
            }
            let mut applied = service
                .echo_applied
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if applied.is_some_and(|latest| latest >= sequence) {
                return;
            }
            *applied = Some(sequence);
            drop(applied);
            if commits {
                service.publish_requested_core(Some(&core_type));
            } else {
                service.publish_requested_core(None);
            }
        });
    }

    pub async fn shutdown(&self) {
        if let Err(error) = self.control.shutdown().await {
            tracing::error!("failed to shut down core control plane cleanly: {error}");
        }
    }

    pub async fn until_control_closed(&self) -> ExecutorExit {
        self.control.until_closed().await
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
        let infos = self.runtime.as_ref();
        let binary_path = find_binary_path(infos, core_type).map_err(|error| {
            OperationError::with_kind(CoreErrorKind::BinaryNotFound, error.to_string())
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

fn echo_commits(state: &OperationState) -> bool {
    let OperationState::Succeeded(OperationOutput::Reconciled(outcome)) = state else {
        return false;
    };
    let mut effective = outcome;
    while let ApplyOutcome::DurabilityUncertain { outcome, .. } = effective {
        effective = &**outcome;
    }
    !matches!(effective, ApplyOutcome::RolledBack { .. })
}

fn map_operation(id: OperationId, state: OperationState) -> OperationInfo {
    let (phase, output, error) = match state {
        OperationState::Queued => (OperationPhase::Queued, None, None),
        OperationState::Running => (OperationPhase::Running, None, None),
        OperationState::Succeeded(output) => (
            OperationPhase::Succeeded,
            Some(map_operation_output(output)),
            None,
        ),
        OperationState::Failed(failure) => (
            OperationPhase::Failed,
            None,
            Some(OperationErrorInfo {
                kind: failure.kind.map(|kind| std::borrow::Cow::Borrowed(kind.as_str())),
                message: failure.message,
                retryable: failure.retryable,
            }),
        ),
    };
    OperationInfo {
        id: id.to_string(),
        phase,
        output,
        error,
    }
}

fn map_operation_output(output: OperationOutput) -> OperationOutputInfo {
    match output {
        OperationOutput::Reconciled(outcome) => {
            OperationOutputInfo::Reconciled(map_reconcile_outcome(&outcome))
        }
        OperationOutput::Stopped => OperationOutputInfo::Stopped,
        OperationOutput::Recovered => OperationOutputInfo::Recovered,
        OperationOutput::ShutDown => OperationOutputInfo::ShutDown,
    }
}

fn map_reconcile_outcome(outcome: &ApplyOutcome) -> ReconcileOutcomeInfo {
    let mut warnings = Vec::new();
    let mut current = outcome;
    while let ApplyOutcome::DurabilityUncertain { outcome, warning } = current {
        warnings.push(warning.clone());
        current = &**outcome;
    }
    let (kind, revision, failed_apply) = match current {
        ApplyOutcome::Started { revision } => (ReconcileOutcomeKind::Started, revision, None),
        ApplyOutcome::Noop { revision } => (ReconcileOutcomeKind::Noop, revision, None),
        ApplyOutcome::Patched { revision } => (ReconcileOutcomeKind::Patched, revision, None),
        ApplyOutcome::Reloaded { revision } => (ReconcileOutcomeKind::Reloaded, revision, None),
        ApplyOutcome::Restarted { revision } => (ReconcileOutcomeKind::Restarted, revision, None),
        ApplyOutcome::Switched { revision } => (ReconcileOutcomeKind::Switched, revision, None),
        ApplyOutcome::RolledBack {
            revision,
            failed_apply,
        } => (
            ReconcileOutcomeKind::RolledBack,
            revision,
            Some(failed_apply.clone()),
        ),
        ApplyOutcome::DurabilityUncertain { .. } => unreachable!("unwrapped above"),
    };
    ReconcileOutcomeInfo {
        outcome: kind,
        revision: map_revision(revision),
        warning: (!warnings.is_empty()).then(|| warnings.join("; ")),
        failed_apply,
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

fn map_config_path_error(error: std::io::Error) -> OperationError {
    if error.kind() == std::io::ErrorKind::NotFound {
        OperationError::with_kind(CoreErrorKind::ConfigNotFound, error.to_string())
    } else {
        OperationError::plain(error.to_string())
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_not_found_errors_receive_the_config_kind() {
        let missing = map_config_path_error(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "missing",
        ));
        assert_eq!(missing.kind(), Some(CoreErrorKind::ConfigNotFound));

        let denied = map_config_path_error(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "denied",
        ));
        assert_eq!(denied.kind(), None);
    }
}
