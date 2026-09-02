use crate::{
    Error, RuntimeFeature, RuntimeInstance, RuntimeLaunchRequest,
    spec::{InstanceSpec, ResolvedController},
    state::{CoreState, SpecSummary, StopReason},
};

use super::super::{Active, CoreManager, Ctrl, SwitchOutcome, map_instance_state, spawn_forwarder};

impl CoreManager {
    pub(super) async fn spawn_switch_instance(
        &self,
        spec: InstanceSpec,
        epoch: u64,
        controller: ResolvedController,
    ) -> Result<Box<dyn RuntimeInstance>, Error> {
        self.inner
            .backend
            .launch(RuntimeLaunchRequest {
                effective_spec: spec,
                epoch,
                controller,
                log_tx: self.inner.log_tx.clone(),
            })
            .await
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) fn install_switched(
        &self,
        ctrl: &mut Ctrl,
        instance: Box<dyn RuntimeInstance>,
        source_spec: InstanceSpec,
        source_document: serde_yaml_ng::Mapping,
        effective_document: serde_yaml_ng::Mapping,
        revision: crate::ConfigRevision,
        capabilities: Vec<crate::Feature>,
        runtime_features: Vec<RuntimeFeature>,
    ) {
        let epoch = revision.epoch;
        let state = instance.state().borrow().clone();
        let pid = instance.pid().unwrap_or_default();
        self.inner.publish(
            CoreState::Running { epoch, pid },
            state.health,
            Some(SpecSummary {
                kind: source_spec.core.kind,
                config_path: source_spec.config_path.clone(),
                capabilities: capabilities.clone(),
                runtime_features: runtime_features.clone(),
            }),
            Some(instance.controller().host.clone()),
            Some(revision.clone()),
        );
        let forwarder = spawn_forwarder(self.inner.clone(), instance.state(), epoch);
        ctrl.last_spec = Some(source_spec.clone());
        ctrl.current = Some(Active {
            instance,
            forwarder,
            source_spec,
            capabilities,
            runtime_features,
            source_document,
            effective_document,
            revision,
        });
    }

    pub(super) fn republish_retained(&self, ctrl: &Ctrl) {
        if let Some(active) = ctrl.current.as_ref() {
            let status = active.instance.state().borrow().clone();
            self.inner.publish(
                map_instance_state(active.instance.epoch(), &status.state),
                status.health,
                Some(SpecSummary {
                    kind: active.source_spec.core.kind,
                    config_path: active.source_spec.config_path.clone(),
                    capabilities: active.capabilities.clone(),
                    runtime_features: active.runtime_features.clone(),
                }),
                Some(active.instance.controller().host.clone()),
                Some(active.revision.clone()),
            );
        }
    }

    pub(super) fn publish_switch_error(&self, error: &Error) {
        self.inner.publish(
            CoreState::Stopped {
                reason: Some(StopReason::Error(error.to_string())),
            },
            None,
            None,
            None,
            None,
        );
    }
}

pub(super) fn with_durability(
    result: Result<SwitchOutcome, Error>,
    warning: Option<String>,
) -> Result<SwitchOutcome, Error> {
    match (result, warning) {
        (Ok(outcome), Some(warning)) => Ok(SwitchOutcome::DurabilityUncertain {
            outcome: Box::new(outcome),
            warning,
        }),
        (Err(error), Some(warning)) => Err(Error::DurabilityUncertain {
            source: Box::new(error),
            warning,
        }),
        (result, None) => result,
    }
}
