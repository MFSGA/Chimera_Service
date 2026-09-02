use std::sync::Arc;

use tokio::sync::watch;

use crate::{
    HealthStatus,
    state::{ConfigRevision, CoreState, InstanceState, SpecSummary, now_ms},
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
            status.state = state.clone();
            status.health = health.clone();
            status.spec = spec.clone();
            status.controller = controller.clone();
            status.revision = revision.clone();
            if lifecycle_changed {
                status.changed_at = now_ms();
            }
        });
    }
}

pub(super) fn spawn_forwarder(
    inner: Arc<Inner>,
    mut states: watch::Receiver<crate::InstanceStatus>,
    epoch: u64,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        while states.changed().await.is_ok() {
            let instance = states.borrow_and_update().clone();
            inner.status_tx.send_if_modified(|status| {
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
            });
        }
    })
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
