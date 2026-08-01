use chimera_ipc::api::{
    core::apply::{ApplyOutcomeKind, CoreApplyData},
    error_kind,
    status::{
        ConfigRevisionInfo, CoreControllerInfo, CoreHealthInfo, CoreHealthState, CoreInfos,
        CoreState, CoreStateDetail, RevisionIdInfo,
    },
};
use chimera_utils::core::CoreType;
use nyanpasu_core_manager::{
    ApplyOutcome, ConfigRevision, CoreState as ManagerCoreState, CoreStatus, Error as ManagerError,
    HealthState, HealthStatus, Host, RevisionId,
};

pub(crate) fn project_core_infos(
    status: &CoreStatus,
    requested_core: Option<CoreType>,
) -> CoreInfos {
    CoreInfos {
        r#type: requested_core,
        state: map_core_state(&status.state),
        state_changed_at: status.changed_at,
        config_path: status
            .spec
            .as_ref()
            .map(|spec| spec.config_path.as_std_path().to_path_buf()),
        controller: status.controller.as_ref().and_then(map_controller),
        health: status.health.as_ref().map(map_health),
        revision: status.revision.as_ref().map(map_revision),
        detail: map_state_detail(&status.state),
    }
}

pub(crate) fn map_revision_id(info: &RevisionIdInfo) -> RevisionId {
    RevisionId {
        epoch: info.epoch,
        generation: info.generation,
        effective_hash: info.effective_hash.clone(),
    }
}

pub(crate) fn map_apply_outcome(outcome: &ApplyOutcome) -> CoreApplyData {
    let mut warnings = Vec::new();
    let mut current = outcome;
    while let ApplyOutcome::DurabilityUncertain { outcome, warning } = current {
        warnings.push(warning.clone());
        current = outcome;
    }
    let (kind, revision, failed_apply) = match current {
        ApplyOutcome::Noop { revision } => (ApplyOutcomeKind::Noop, revision, None),
        ApplyOutcome::Patched { revision } => (ApplyOutcomeKind::Patched, revision, None),
        ApplyOutcome::Reloaded { revision } => (ApplyOutcomeKind::Reloaded, revision, None),
        ApplyOutcome::Restarted { revision } => (ApplyOutcomeKind::Restarted, revision, None),
        ApplyOutcome::Switched { revision } => (ApplyOutcomeKind::Switched, revision, None),
        ApplyOutcome::RolledBack {
            revision,
            failed_apply,
        } => (
            ApplyOutcomeKind::RolledBack,
            revision,
            Some(failed_apply.clone()),
        ),
        ApplyOutcome::DurabilityUncertain { .. } => unreachable!("unwrapped above"),
    };
    CoreApplyData {
        outcome: kind,
        revision: map_revision(revision),
        warning: (!warnings.is_empty()).then(|| warnings.join("; ")),
        failed_apply,
    }
}

pub(crate) fn map_error_kind(error: &ManagerError) -> Option<&'static str> {
    match error {
        ManagerError::NotStarted => Some(error_kind::NOT_STARTED),
        ManagerError::AlreadyRunning => Some(error_kind::ALREADY_RUNNING),
        ManagerError::RevisionConflict { .. } => Some(error_kind::REVISION_CONFLICT),
        ManagerError::ManagerQuarantined { .. } => Some(error_kind::QUARANTINED),
        ManagerError::ConfigCheckFailed(_) => Some(error_kind::CONFIG_CHECK_FAILED),
        ManagerError::ConfigNotFound(_) => Some(error_kind::CONFIG_NOT_FOUND),
        ManagerError::BinaryNotFound(_) => Some(error_kind::BINARY_NOT_FOUND),
        ManagerError::InvalidConfig(_) | ManagerError::Yaml(_) => Some(error_kind::INVALID_CONFIG),
        ManagerError::ControllerMissing => Some(error_kind::CONTROLLER_MISSING),
        ManagerError::ApplyFailed(_) => Some(error_kind::APPLY_FAILED),
        ManagerError::ApplyRollbackFailed { .. } => Some(error_kind::APPLY_ROLLBACK_FAILED),
        ManagerError::StopUnconfirmed(_) => Some(error_kind::STOP_UNCONFIRMED),
        ManagerError::DurabilityUncertain { source, .. } => map_error_kind(source),
        _ => None,
    }
}

fn map_core_state(state: &ManagerCoreState) -> CoreState {
    match state {
        ManagerCoreState::Running { .. }
        | ManagerCoreState::Switching { .. }
        | ManagerCoreState::Stopping { .. } => CoreState::Running,
        ManagerCoreState::Starting { .. } | ManagerCoreState::Restarting { .. } => {
            CoreState::Stopped(None)
        }
        ManagerCoreState::Stopped { reason } => {
            CoreState::Stopped(reason.as_ref().map(ToString::to_string))
        }
        _ => CoreState::Stopped(None),
    }
}

fn map_state_detail(state: &ManagerCoreState) -> Option<CoreStateDetail> {
    match state {
        ManagerCoreState::Stopped { reason } => Some(CoreStateDetail::Stopped {
            reason: reason.as_ref().map(ToString::to_string),
        }),
        ManagerCoreState::Starting { epoch } => Some(CoreStateDetail::Starting { epoch: *epoch }),
        ManagerCoreState::Running { epoch, pid } => Some(CoreStateDetail::Running {
            epoch: *epoch,
            pid: *pid,
        }),
        ManagerCoreState::Restarting { epoch, attempt } => Some(CoreStateDetail::Restarting {
            epoch: *epoch,
            attempt: *attempt,
        }),
        ManagerCoreState::Switching { from, to } => Some(CoreStateDetail::Switching {
            from: *from,
            to: *to,
        }),
        ManagerCoreState::Stopping { epoch } => Some(CoreStateDetail::Stopping { epoch: *epoch }),
        _ => None,
    }
}

fn map_controller(host: &Host) -> Option<CoreControllerInfo> {
    match host {
        Host::NamedPipe(path) => Some(CoreControllerInfo::NamedPipe(path.clone())),
        Host::UnixSocket(path) => Some(CoreControllerInfo::UnixSocket(path.clone())),
        Host::Http(url) => {
            let mut url = url.clone();
            let _ = url.set_username("");
            let _ = url.set_password(None);
            Some(CoreControllerInfo::Http(url.to_string()))
        }
        _ => None,
    }
}

fn map_health(health: &HealthStatus) -> CoreHealthInfo {
    CoreHealthInfo {
        state: match health.state {
            HealthState::Starting => CoreHealthState::Starting,
            HealthState::Healthy => CoreHealthState::Healthy,
            HealthState::Unhealthy => CoreHealthState::Unhealthy,
            _ => CoreHealthState::Unhealthy,
        },
        changed_at: health.changed_at,
        consecutive_failures: health.consecutive_failures,
        last_error: health.last_error.clone(),
        last_success_at: health.last_success_at,
    }
}

fn map_revision(revision: &ConfigRevision) -> ConfigRevisionInfo {
    ConfigRevisionInfo {
        epoch: revision.epoch,
        generation: revision.generation,
        source_hash: revision.source_hash.clone(),
        effective_hash: revision.effective_hash.clone(),
    }
}
