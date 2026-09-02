use crate::{
    Error, RuntimeConfigBackup, RuntimeInstance, RuntimeLaunchRequest,
    capability::ResolvedFeatures,
    config::{ConfigSnapshot, PreparedConfig},
    spec::{InstanceSpec, ResolvedController},
    state::{ConfigRevision, CoreState, SpecSummary},
};

use super::{
    Active, ApplyOutcome, CoreManager, abort_and_await, publish::spawn_forwarder,
    quarantine::reject_quarantine,
};

impl CoreManager {
    /// Apply a desired config with optimistic revision checking.
    ///
    /// Mihomo changes are classified deny-by-default. Safe controller patches
    /// and reloads stay on the current epoch; everything else falls back to the
    /// checked restart/rollback path.
    pub async fn apply_config(
        &self,
        input: InstanceSpec,
        expected_revision: Option<crate::RevisionId>,
    ) -> Result<ApplyOutcome, Error> {
        let _operation = self.inner.operation.lock().await;
        self.apply_config_inner(input, expected_revision).await
    }

    pub(super) async fn apply_config_inner(
        &self,
        input: InstanceSpec,
        expected_revision: Option<crate::RevisionId>,
    ) -> Result<ApplyOutcome, Error> {
        let (previous_spec, previous_revision, current_source, current_effective) = {
            let ctrl = self.inner.ctrl.lock().await;
            reject_quarantine(&ctrl)?;
            let active = ctrl.current.as_ref().ok_or(Error::NotStarted)?;
            if active.instance.state().borrow().state.is_terminal() {
                return Err(Error::NotStarted);
            }
            let actual = active.revision.id();
            if let Some(expected) = expected_revision
                && expected != actual
            {
                return Err(Error::RevisionConflict {
                    expected,
                    actual: Some(actual),
                });
            }
            (
                active.source_spec.clone(),
                active.revision.clone(),
                active.source_document.clone(),
                active.effective_document.clone(),
            )
        };

        let resolved = self.resolve_core_features(&input.core).await?;
        let snapshot = ConfigSnapshot::load(&input.config_path).await?;
        let previous_snapshot = ConfigSnapshot::from_document(
            previous_spec.config_path.clone(),
            current_source.clone(),
        )?;
        let candidate = snapshot.prepare_full(
            self.inner.options.controller_template.as_deref(),
            self.inner.store.dir(),
            previous_revision.epoch,
            resolved.runtime,
        )?;
        self.warn_http_fallback(
            &input.core,
            resolved.version.as_deref(),
            candidate.rewrote_controller,
        );
        let change = crate::config::mihomo::classify(
            &current_source,
            &current_effective,
            &previous_spec,
            snapshot.document(),
            &candidate.document,
            &input,
        )?;
        if matches!(change, crate::config::mihomo::ConfigChange::Noop) {
            return Ok(ApplyOutcome::Noop {
                revision: previous_revision,
            });
        }

        // Freeze the desired effective config before validation. The source path
        // can change concurrently; checking and later committing this staged
        // file guarantees the exact same bytes are validated and applied.
        let staged = self
            .inner
            .store
            .stage(previous_revision.epoch, &candidate.bytes)
            .await?;
        let mut check_spec = input.clone();
        check_spec.config_path = staged.path().to_owned();
        self.inner.backend.check_config(&check_spec).await?;
        let mut durability_warnings = Vec::new();
        if matches!(
            change,
            crate::config::mihomo::ConfigChange::Patch { .. }
                | crate::config::mihomo::ConfigChange::Reload
        ) {
            let backup = self
                .inner
                .store
                .backup(previous_revision.epoch, previous_revision.generation + 1)
                .await?;
            let commit = match self
                .inner
                .store
                .commit_replace(staged, previous_revision.epoch)
                .await
            {
                Ok(commit) => commit,
                Err(error) => {
                    if let Err(cleanup) = self.inner.store.remove_backup(backup).await {
                        tracing::warn!(
                            "failed to remove apply backup after commit failure: {cleanup}"
                        );
                    }
                    return Err(error);
                }
            };
            if let Some(warning) = commit.durability_warning() {
                durability_warnings.push(warning.to_owned());
            }
            let reconciled =
                match tokio::time::timeout(self.inner.options.reconcile_timeout, async {
                    let ctrl = self.inner.ctrl.lock().await;
                    let active = ctrl.current.as_ref().ok_or(Error::NotStarted)?;
                    Ok::<bool, Error>(
                        super::reconcile::reconcile_in_place(
                            active,
                            &change,
                            &previous_revision.runtime_path,
                            self.inner.options.control_timeout,
                        )
                        .await,
                    )
                })
                .await
                {
                    Ok(result) => result?,
                    Err(_) => {
                        tracing::warn!(
                            "in-place reconciliation timed out; falling back to restart"
                        );
                        false
                    }
                };
            if reconciled {
                let mut revision = previous_revision.clone();
                revision.generation += 1;
                revision.source_hash = candidate.source_hash;
                revision.effective_hash = candidate.effective_hash;
                let outcome = match change {
                    crate::config::mihomo::ConfigChange::Patch { .. } => ApplyOutcome::Patched {
                        revision: revision.clone(),
                    },
                    crate::config::mihomo::ConfigChange::Reload => ApplyOutcome::Reloaded {
                        revision: revision.clone(),
                    },
                    _ => unreachable!(),
                };
                let mut ctrl = self.inner.ctrl.lock().await;
                let committed_spec = {
                    let active = ctrl.current.as_mut().ok_or(Error::NotStarted)?;
                    active.source_spec = input.clone();
                    active.source_document = snapshot.document().clone();
                    active.effective_document = candidate.document;
                    active.revision = revision.clone();
                    active.source_spec.clone()
                };
                ctrl.last_spec = Some(committed_spec);
                self.inner.status_tx.send_modify(|status| {
                    status.revision = Some(revision.clone());
                    if let Some(spec) = status.spec.as_mut() {
                        spec.config_path = input.config_path.clone();
                    }
                });
                if let Err(error) = self.inner.store.remove_backup(backup).await {
                    tracing::warn!("failed to remove successful apply backup: {error}");
                }
                return Ok(wrap_apply_warnings(outcome, &durability_warnings));
            }
            return self
                .restart_same_epoch_with_compensation(
                    input,
                    snapshot,
                    candidate,
                    resolved,
                    backup,
                    durability_warnings,
                )
                .await;
        } else {
            drop(staged);
        }

        let switched = matches!(change, crate::config::mihomo::ConfigChange::Switch);
        self.stop_inner().await?;
        match self
            .start_inner_with_snapshot(input.clone(), snapshot)
            .await
        {
            Ok(()) => {
                let revision = self
                    .status()
                    .revision
                    .ok_or_else(|| Error::ApplyFailed("started epoch has no revision".into()))?;
                let outcome = if switched {
                    ApplyOutcome::Switched { revision }
                } else {
                    ApplyOutcome::Restarted { revision }
                };
                Ok(wrap_apply_warnings(outcome, &durability_warnings))
            }
            Err(apply_error) => match self
                .start_inner_with_snapshot(previous_spec, previous_snapshot)
                .await
            {
                Ok(()) => {
                    let revision =
                        self.status()
                            .revision
                            .ok_or_else(|| Error::ApplyRollbackFailed {
                                apply: apply_error.to_string(),
                                rollback: "rollback epoch has no revision".into(),
                            })?;
                    Ok(wrap_apply_warnings(
                        ApplyOutcome::RolledBack {
                            revision,
                            failed_apply: apply_error.to_string(),
                        },
                        &durability_warnings,
                    ))
                }
                Err(rollback_error) => Err(Error::ApplyRollbackFailed {
                    apply: apply_error.to_string(),
                    rollback: rollback_error.to_string(),
                }),
            },
        }
    }

    async fn restart_same_epoch_with_compensation(
        &self,
        input: InstanceSpec,
        snapshot: ConfigSnapshot,
        candidate: PreparedConfig,
        resolved: ResolvedFeatures,
        backup: RuntimeConfigBackup,
        mut durability_warnings: Vec<String>,
    ) -> Result<ApplyOutcome, Error> {
        let mut ctrl = self.inner.ctrl.lock().await;
        let old = ctrl.current.take().ok_or(Error::NotStarted)?;
        let Active {
            instance,
            forwarder,
            source_spec: old_source_spec,
            capabilities: old_capabilities,
            runtime_features: old_runtime_features,
            source_document: old_source_document,
            effective_document: old_effective_document,
            revision: old_revision,
        } = old;
        let epoch = old_revision.epoch;
        let old_effective_spec = instance.spec().clone();
        let old_controller = instance.controller().clone();
        abort_and_await(forwarder).await;
        if let Err(error) = instance
            .stop_and_confirm_dead(self.inner.options.stop_timeout)
            .await
        {
            self.latch_quarantine(&mut ctrl, epoch, &error);
            return Err(error);
        }

        let mut effective_spec = input.clone();
        effective_spec.config_path = old_revision.runtime_path.clone();
        effective_spec.pid_file = Some(self.inner.store.pid_path(epoch));
        if effective_spec.core.version.is_none() {
            effective_spec.core.version = resolved.version.clone();
        }
        let desired_revision = ConfigRevision {
            epoch,
            generation: old_revision.generation + 1,
            source_hash: candidate.source_hash.clone(),
            effective_hash: candidate.effective_hash.clone(),
            runtime_path: old_revision.runtime_path.clone(),
        };
        let desired_capabilities: Vec<_> = resolved.capabilities.iter().collect();
        let desired_runtime_features: Vec<_> = resolved.runtime.iter().collect();
        self.inner.publish(
            CoreState::Restarting { epoch, attempt: 0 },
            None,
            Some(SpecSummary {
                kind: input.core.kind,
                config_path: input.config_path.clone(),
                capabilities: desired_capabilities.clone(),
                runtime_features: desired_runtime_features.clone(),
            }),
            Some(candidate.controller.host.clone()),
            Some(desired_revision.clone()),
        );

        match self
            .launch_ready(effective_spec, epoch, candidate.controller.clone())
            .await
        {
            Ok(instance) => {
                let state = instance.state().borrow().clone();
                let pid = instance.pid().unwrap_or_default();
                self.inner.publish(
                    CoreState::Running { epoch, pid },
                    state.health,
                    Some(SpecSummary {
                        kind: input.core.kind,
                        config_path: input.config_path.clone(),
                        capabilities: desired_capabilities.clone(),
                        runtime_features: desired_runtime_features.clone(),
                    }),
                    Some(instance.controller().host.clone()),
                    Some(desired_revision.clone()),
                );
                let forwarder = spawn_forwarder(self.inner.clone(), instance.state(), epoch);
                ctrl.last_spec = Some(input.clone());
                ctrl.current = Some(Active {
                    instance,
                    forwarder,
                    source_spec: input,
                    capabilities: desired_capabilities,
                    runtime_features: desired_runtime_features,
                    source_document: snapshot.document().clone(),
                    effective_document: candidate.document,
                    revision: desired_revision.clone(),
                });
                if let Err(error) = self.inner.store.remove_backup(backup).await {
                    tracing::warn!("failed to remove successful restart backup: {error}");
                }
                Ok(wrap_apply_warnings(
                    ApplyOutcome::Restarted {
                        revision: desired_revision,
                    },
                    &durability_warnings,
                ))
            }
            Err(error @ Error::StopUnconfirmed(_)) => {
                self.latch_quarantine(&mut ctrl, epoch, &error);
                Err(error)
            }
            Err(apply_error) => {
                let apply_text = apply_error.to_string();
                let restore = self
                    .inner
                    .store
                    .restore(&backup)
                    .await
                    .map_err(|restore_error| {
                        let error = Error::ApplyRollbackFailed {
                            apply: apply_text.clone(),
                            rollback: format!("runtime restore failed: {restore_error}"),
                        };
                        self.inner.publish(
                            CoreState::Stopped {
                                reason: Some(crate::StopReason::Error(error.to_string())),
                            },
                            None,
                            None,
                            None,
                            None,
                        );
                        error
                    })?;
                if let Some(warning) = restore.durability_warning() {
                    durability_warnings.push(warning.to_owned());
                }

                self.inner.publish(
                    CoreState::Restarting { epoch, attempt: 0 },
                    None,
                    Some(SpecSummary {
                        kind: old_source_spec.core.kind,
                        config_path: old_source_spec.config_path.clone(),
                        capabilities: old_capabilities.clone(),
                        runtime_features: old_runtime_features.clone(),
                    }),
                    Some(old_controller.host.clone()),
                    Some(old_revision.clone()),
                );
                match self
                    .launch_ready(old_effective_spec, epoch, old_controller)
                    .await
                {
                    Ok(instance) => {
                        let state = instance.state().borrow().clone();
                        let pid = instance.pid().unwrap_or_default();
                        self.inner.publish(
                            CoreState::Running { epoch, pid },
                            state.health,
                            Some(SpecSummary {
                                kind: old_source_spec.core.kind,
                                config_path: old_source_spec.config_path.clone(),
                                capabilities: old_capabilities.clone(),
                                runtime_features: old_runtime_features.clone(),
                            }),
                            Some(instance.controller().host.clone()),
                            Some(old_revision.clone()),
                        );
                        let forwarder =
                            spawn_forwarder(self.inner.clone(), instance.state(), epoch);
                        ctrl.last_spec = Some(old_source_spec.clone());
                        ctrl.current = Some(Active {
                            instance,
                            forwarder,
                            source_spec: old_source_spec,
                            capabilities: old_capabilities,
                            runtime_features: old_runtime_features,
                            source_document: old_source_document,
                            effective_document: old_effective_document,
                            revision: old_revision.clone(),
                        });
                        if let Err(error) = self.inner.store.remove_backup(backup).await {
                            tracing::warn!("failed to remove rollback backup: {error}");
                        }
                        Ok(wrap_apply_warnings(
                            ApplyOutcome::RolledBack {
                                revision: old_revision,
                                failed_apply: apply_text,
                            },
                            &durability_warnings,
                        ))
                    }
                    Err(rollback_error @ Error::StopUnconfirmed(_)) => {
                        let error = Error::StopUnconfirmed(format!(
                            "desired apply failed ({apply_text}); rollback replacement {rollback_error}"
                        ));
                        self.latch_quarantine(&mut ctrl, epoch, &error);
                        Err(error)
                    }
                    Err(rollback_error) => {
                        let error = Error::ApplyRollbackFailed {
                            apply: apply_text,
                            rollback: rollback_error.to_string(),
                        };
                        self.inner.publish(
                            CoreState::Stopped {
                                reason: Some(crate::StopReason::Error(error.to_string())),
                            },
                            None,
                            None,
                            None,
                            None,
                        );
                        Err(error)
                    }
                }
            }
        }
    }

    async fn launch_ready(
        &self,
        effective_spec: InstanceSpec,
        epoch: u64,
        controller: ResolvedController,
    ) -> Result<Box<dyn RuntimeInstance>, Error> {
        let instance = self
            .inner
            .backend
            .launch(RuntimeLaunchRequest {
                effective_spec,
                epoch,
                controller,
                log_tx: self.inner.log_tx.clone(),
            })
            .await?;
        if let Err(readiness_error) = instance.wait_ready().await {
            return match instance
                .stop_and_confirm_dead(self.inner.options.stop_timeout)
                .await
            {
                Ok(()) => Err(readiness_error),
                Err(stop_error) => Err(Error::StopUnconfirmed(format!(
                    "{readiness_error}; failed to stop rejected replacement: {stop_error}"
                ))),
            };
        }
        Ok(instance)
    }
}

fn wrap_apply_warnings(mut outcome: ApplyOutcome, warnings: &[String]) -> ApplyOutcome {
    for warning in warnings.iter().rev() {
        outcome = ApplyOutcome::DurabilityUncertain {
            outcome: Box::new(outcome),
            warning: warning.clone(),
        };
    }
    outcome
}
