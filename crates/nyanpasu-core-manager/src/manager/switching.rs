mod helpers;
mod prepare;

use helpers::with_durability;
use crate::{
    Error, ProbePhase,
    capability::ResolvedFeatures,
    config::{ConfigSnapshot, mihomo::OverlapBlock},
    kind::CoreKind,
    spec::InstanceSpec,
    state::{CoreState, SpecSummary},
};

use super::{
    CoreManager, DegradeReason, SwitchOutcome, abort_and_await, quarantine::reject_quarantine,
    reconcile,
};
use prepare::GracefulPlan;

pub(super) fn graceful_degrade_reason(
    local_controller: bool,
    kind: CoreKind,
    overlap_block: Option<OverlapBlock>,
) -> Option<DegradeReason> {
    if !local_controller {
        return Some(DegradeReason::HttpController);
    }
    if kind != CoreKind::Mihomo {
        return Some(DegradeReason::UnsupportedKind);
    }
    overlap_block.map(|block| match block {
        OverlapBlock::DnsListen => DegradeReason::DnsListen,
        OverlapBlock::InboundSurface => DegradeReason::InboundConflict,
    })
}

impl CoreManager {
    pub(super) async fn graceful_switch(
        &self,
        spec: InstanceSpec,
        snapshot: ConfigSnapshot,
        resolved: ResolvedFeatures,
    ) -> Result<SwitchOutcome, Error> {
        let mut ctrl = self.inner.ctrl.lock().await;
        reject_quarantine(&ctrl)?;
        let old_epoch = ctrl
            .current
            .as_ref()
            .filter(|active| !active.instance.state().borrow().state.is_terminal())
            .map(|active| active.instance.epoch())
            .ok_or(Error::NotStarted)?;
        let epoch = self.inner.epoch.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
        let plan = match self.prepare_graceful(spec, epoch, &snapshot, resolved).await {
            Ok(plan) => plan,
            Err(error) => {
                let _ = self.inner.store.cleanup_epoch(epoch).await;
                self.republish_retained(&ctrl);
                return Err(error);
            }
        };
        let GracefulPlan {
            source_spec,
            effective_spec,
            controller,
            revision,
            capabilities,
            runtime_features,
            source_document,
            effective_document,
            full_staged,
            restoration,
        } = plan;
        self.inner.publish(
            CoreState::Switching {
                from: Some(old_epoch),
                to: epoch,
            },
            None,
            Some(SpecSummary {
                kind: source_spec.core.kind,
                config_path: source_spec.config_path.clone(),
                capabilities: capabilities.clone(),
                runtime_features: runtime_features.clone(),
            }),
            Some(controller.host.clone()),
            Some(revision.clone()),
        );

        let instance = match self
            .spawn_switch_instance(effective_spec.clone(), epoch, controller.clone())
            .await
        {
            Ok(instance) => instance,
            Err(error) => {
                let _ = self.inner.store.cleanup_epoch(epoch).await;
                self.republish_retained(&ctrl);
                return Err(error);
            }
        };
        if let Err(readiness_error) = instance.wait_ready().await {
            match instance
                .stop_and_confirm_dead(self.inner.options.stop_timeout)
                .await
            {
                Ok(()) => {
                    let _ = self.inner.store.cleanup_epoch(epoch).await;
                    self.republish_retained(&ctrl);
                    return Err(readiness_error);
                }
                Err(stop_error) => {
                    let error = Error::StopUnconfirmed(format!(
                        "{readiness_error}; failed to stop rejected graceful bootstrap: {stop_error}"
                    ));
                    self.latch_quarantine(&mut ctrl, epoch, &error);
                    return Err(error);
                }
            }
        }

        let old = ctrl.current.take().expect("running checked above");
        abort_and_await(old.forwarder).await;
        if let Err(error) = old
            .instance
            .stop_and_confirm_dead(self.inner.options.stop_timeout)
            .await
        {
            let _ = instance
                .stop_and_confirm_dead(self.inner.options.stop_timeout)
                .await;
            let _ = self.inner.store.cleanup_epoch(epoch).await;
            if matches!(error, Error::StopUnconfirmed(_)) {
                self.latch_quarantine(&mut ctrl, old_epoch, &error);
            } else {
                self.publish_switch_error(&error);
            }
            return Err(error);
        }

        let commit = match self.inner.store.commit_replace(full_staged, epoch).await {
            Ok(commit) => commit,
            Err(error) => {
                let stop = instance
                    .stop_and_confirm_dead(self.inner.options.stop_timeout)
                    .await;
                if stop.is_ok() {
                    let _ = self.inner.store.cleanup_epoch(epoch).await;
                }
                let error = match stop {
                    Ok(()) => error,
                    Err(stop_error) => Error::StopUnconfirmed(format!(
                        "full runtime commit failed: {error}; bootstrap stop also failed: {stop_error}"
                    )),
                };
                if matches!(error, Error::StopUnconfirmed(_)) {
                    self.latch_quarantine(&mut ctrl, epoch, &error);
                } else {
                    self.publish_switch_error(&error);
                }
                return Err(error);
            }
        };
        let durability_warning = commit.durability_warning().map(str::to_owned);

        let reconciled = tokio::time::timeout(self.inner.options.reconcile_timeout, async {
            match restoration {
                Some((patch, projection)) => {
                    reconcile::patch_and_verify(
                        instance.as_ref(),
                        &patch,
                        &projection,
                        self.inner.options.control_timeout,
                    )
                    .await
                        && instance.probe_now(ProbePhase::Reconcile).await.is_healthy()
                }
                None => instance.probe_now(ProbePhase::Reconcile).await.is_healthy(),
            }
        })
        .await
        .unwrap_or(false);
        if reconciled {
            self.install_switched(
                &mut ctrl,
                instance,
                source_spec,
                source_document,
                effective_document,
                revision,
                capabilities,
                runtime_features,
            );
            let result = self
                .inner
                .store
                .cleanup_epoch(old_epoch)
                .await
                .map(|()| SwitchOutcome::Graceful);
            return with_durability(result, durability_warning);
        }

        if let Err(error) = instance
            .stop_and_confirm_dead(self.inner.options.stop_timeout)
            .await
        {
            if matches!(error, Error::StopUnconfirmed(_)) {
                self.latch_quarantine(&mut ctrl, epoch, &error);
            } else {
                self.publish_switch_error(&error);
            }
            return with_durability(Err(error), durability_warning);
        }
        let replacement = match self
            .spawn_switch_instance(effective_spec, epoch, controller)
            .await
        {
            Ok(instance) => instance,
            Err(error) => {
                let _ = self.inner.store.cleanup_epoch(epoch).await;
                self.publish_switch_error(&error);
                return with_durability(Err(error), durability_warning);
            }
        };
        if let Err(readiness_error) = replacement.wait_ready().await {
            let stop = replacement
                .stop_and_confirm_dead(self.inner.options.stop_timeout)
                .await;
            match stop {
                Ok(()) => {
                    let _ = self.inner.store.cleanup_epoch(epoch).await;
                    self.publish_switch_error(&readiness_error);
                    return with_durability(Err(readiness_error), durability_warning);
                }
                Err(stop_error) => {
                    let error = Error::StopUnconfirmed(format!(
                        "{readiness_error}; failed to stop rejected hard replacement: {stop_error}"
                    ));
                    self.latch_quarantine(&mut ctrl, epoch, &error);
                    return with_durability(Err(error), durability_warning);
                }
            }
        }
        self.install_switched(
            &mut ctrl,
            replacement,
            source_spec,
            source_document,
            effective_document,
            revision,
            capabilities,
            runtime_features,
        );
        let result = self
            .inner
            .store
            .cleanup_epoch(old_epoch)
            .await
            .map(|()| SwitchOutcome::Hard {
                reason: DegradeReason::PatchFailed,
            });
        with_durability(result, durability_warning)
    }

}
