use std::sync::Arc;

use tokio::sync::watch;

use crate::{
    HealthStatus,
    state::{
        ConfigRevision, CoreState, CoreStatus, HealthState, InstanceState, InstanceStatus,
        SpecSummary, now_ms,
    },
};

use super::Inner;

impl Inner {
    pub(super) fn publish(
        &self,
        state: CoreState,
        health: Option<HealthStatus>,
        spec: Option<SpecSummary>,
        controller: Option<clash_api::Host>,
        revision: Option<ConfigRevision>,
    ) {
        self.status_tx.send_modify(|status| {
            let lifecycle_changed = status.state != state;
            let health =
                health.or_else(|| default_health_for_state(status.health.as_ref(), &state));
            status.state = state;
            status.health = health;
            status.spec = spec;
            status.controller = controller;
            status.revision = revision;
            if lifecycle_changed {
                status.changed_at = now_ms();
            }
        });
    }
}

fn default_health_for_state(
    previous: Option<&HealthStatus>,
    state: &CoreState,
) -> Option<HealthStatus> {
    let target = match state {
        CoreState::Starting { .. } | CoreState::Restarting { .. } | CoreState::Switching { .. } => {
            HealthState::Starting
        }
        CoreState::Running { .. } => HealthState::Healthy,
        CoreState::Stopping { .. } | CoreState::Stopped { .. } => return None,
    };
    let mut health = HealthStatus::starting();
    health.state = target;
    if let Some(previous) = previous.filter(|status| status.state == target) {
        health.changed_at = previous.changed_at;
        health.consecutive_failures = previous.consecutive_failures;
        health.last_error.clone_from(&previous.last_error);
        health.last_success_at = previous.last_success_at;
    }
    Some(health)
}

pub(super) fn spawn_forwarder(
    inner: Arc<Inner>,
    mut states: watch::Receiver<InstanceStatus>,
    epoch: u64,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        while states.changed().await.is_ok() {
            let instance = states.borrow_and_update().clone();
            inner
                .status_tx
                .send_if_modified(|status| apply_epoch_status(status, epoch, &instance));
        }
    })
}

fn apply_epoch_status(status: &mut CoreStatus, epoch: u64, instance: &InstanceStatus) -> bool {
    if status.revision.as_ref().map(|revision| revision.epoch) != Some(epoch) {
        return false;
    }
    let next_state = map_instance_state(epoch, &instance.state);
    let lifecycle_changed = status.state != next_state;
    let health_changed = status.health != instance.health;
    if !lifecycle_changed && !health_changed {
        return false;
    }
    status.state = next_state;
    status.health = instance.health.clone();
    if lifecycle_changed {
        status.changed_at = now_ms();
    }
    true
}

pub(super) fn map_instance_state(epoch: u64, state: &InstanceState) -> CoreState {
    match state {
        InstanceState::Starting => CoreState::Starting { epoch },
        InstanceState::Running { pid } => CoreState::Running { epoch, pid: *pid },
        InstanceState::Restarting { attempt } => CoreState::Restarting {
            epoch,
            attempt: *attempt,
        },
        InstanceState::Stopping => CoreState::Stopping { epoch },
        InstanceState::Stopped(reason) => CoreState::Stopped {
            reason: Some(reason.clone()),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::StopReason;

    fn revision(epoch: u64) -> ConfigRevision {
        ConfigRevision {
            epoch,
            generation: 1,
            source_hash: "source".into(),
            effective_hash: "effective".into(),
            runtime_path: format!("config-{epoch}.yaml").into(),
        }
    }

    #[test]
    fn old_epoch_events_cannot_overwrite_new_epoch_status() {
        let mut status = CoreStatus::initial();
        status.revision = Some(revision(9));
        status.state = CoreState::Running { epoch: 9, pid: 90 };

        let stale = InstanceStatus {
            state: InstanceState::Stopped(StopReason::Finished),
            health: None,
        };
        assert!(!apply_epoch_status(&mut status, 8, &stale));
        assert!(matches!(
            status.state,
            CoreState::Running { epoch: 9, pid: 90 }
        ));
    }

    #[test]
    fn stale_epoch_status_neither_mutates_nor_wakes_watchers() {
        let mut status = CoreStatus::initial();
        status.revision = Some(revision(9));
        status.state = CoreState::Running { epoch: 9, pid: 90 };
        let (tx, rx) = watch::channel(status);
        let stale = InstanceStatus {
            state: InstanceState::Running { pid: 80 },
            health: Some(HealthStatus::starting()),
        };

        let sent = tx.send_if_modified(|status| apply_epoch_status(status, 8, &stale));

        assert!(!sent);
        assert!(!rx.has_changed().unwrap());
        assert!(matches!(
            rx.borrow().state,
            CoreState::Running { epoch: 9, pid: 90 }
        ));
    }

    #[test]
    fn pure_health_transition_preserves_lifecycle_changed_at() {
        let mut status = CoreStatus::initial();
        status.revision = Some(revision(3));
        status.state = CoreState::Running { epoch: 3, pid: 30 };
        status.changed_at = 7;
        let mut health = HealthStatus::starting();
        health.state = HealthState::Unhealthy;
        let instance = InstanceStatus {
            state: InstanceState::Running { pid: 30 },
            health: Some(health.clone()),
        };

        assert!(apply_epoch_status(&mut status, 3, &instance));
        assert_eq!(status.changed_at, 7);
        assert_eq!(status.health, Some(health));
    }

    #[test]
    fn lifecycle_publication_gets_default_health() {
        assert_eq!(
            default_health_for_state(None, &CoreState::Starting { epoch: 1 })
                .map(|health| health.state),
            Some(HealthState::Starting)
        );
        assert_eq!(
            default_health_for_state(None, &CoreState::Running { epoch: 1, pid: 7 })
                .map(|health| health.state),
            Some(HealthState::Healthy)
        );
        assert!(
            default_health_for_state(
                None,
                &CoreState::Stopped {
                    reason: Some(StopReason::User)
                }
            )
            .is_none()
        );
    }
}
