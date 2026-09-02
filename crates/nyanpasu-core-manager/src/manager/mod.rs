//! Cross-epoch orchestration and atomic status publication.

mod publish;
mod quarantine;
mod reconcile;
mod switching;

use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};

use tokio::sync::{broadcast, watch};

use crate::{
    Error, LogFrame, ProbeHandle,
    capability::{ResolvedFeatures, VersionCache, resolve_features},
    config::{
        ConfigSnapshot,
        runtime_store::{RuntimeConfigStore, RuntimeDirectoryLock},
    },
    log_sink::{self, SinkHandle, SinkOptions},
    runtime::{
        RuntimeBackend, RuntimeInstance, RuntimeLaunchRequest,
        process::{ProbePlan, ProcessRuntimeBackend},
    },
    spec::{CoreSpec, InstanceSpec, LocalIpcPolicy, ManagerOptions},
    state::{
        ConfigRevision, CoreState, CoreStatus, InstanceState, SpecSummary, StopReason, now_ms,
    },
};
use publish::{map_instance_state, spawn_forwarder};
use quarantine::reject_quarantine;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DegradeReason {
    NotRunning,
    UnsupportedKind,
    DnsListen,
    InboundConflict,
    PatchFailed,
    HttpController,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SwitchOutcome {
    Graceful,
    Hard { reason: DegradeReason },
    DurabilityUncertain {
        outcome: Box<SwitchOutcome>,
        warning: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApplyOutcome {
    Started { revision: ConfigRevision },
    Noop { revision: ConfigRevision },
    Patched { revision: ConfigRevision },
    Reloaded { revision: ConfigRevision },
    Restarted { revision: ConfigRevision },
    Switched { revision: ConfigRevision },
    RolledBack {
        revision: ConfigRevision,
        failed_apply: String,
    },
    DurabilityUncertain {
        outcome: Box<ApplyOutcome>,
        warning: String,
    },
}

#[derive(Clone)]
pub struct CoreManager {
    inner: Arc<Inner>,
}

pub struct CoreManagerBuilder {
    options: ManagerOptions,
    probes: ProbePlan,
    backend: Option<Arc<dyn RuntimeBackend>>,
}

struct Inner {
    options: ManagerOptions,
    backend: Arc<dyn RuntimeBackend>,
    store: RuntimeConfigStore,
    _runtime_lock: RuntimeDirectoryLock,
    status_tx: watch::Sender<CoreStatus>,
    log_tx: broadcast::Sender<Arc<LogFrame>>,
    log_dir: Option<camino::Utf8PathBuf>,
    log_sink: tokio::sync::Mutex<Option<SinkHandle>>,
    version_cache: VersionCache,
    operation: tokio::sync::Mutex<()>,
    ctrl: tokio::sync::Mutex<Ctrl>,
    epoch: AtomicU64,
}

#[derive(Default)]
struct Ctrl {
    current: Option<Active>,
    last_spec: Option<InstanceSpec>,
    quarantine: Vec<QuarantinedEpoch>,
}

#[derive(Debug, Clone)]
struct QuarantinedEpoch {
    epoch: u64,
    reason: String,
    death_proven: bool,
}

struct Active {
    instance: Box<dyn RuntimeInstance>,
    forwarder: tokio::task::JoinHandle<()>,
    source_spec: InstanceSpec,
    capabilities: Vec<crate::Feature>,
    runtime_features: Vec<crate::RuntimeFeature>,
    source_document: serde_yaml_ng::Mapping,
    effective_document: serde_yaml_ng::Mapping,
    revision: ConfigRevision,
}

impl CoreManagerBuilder {
    pub fn readiness_probe(mut self, probe: ProbeHandle) -> Self {
        self.probes.readiness = Some(probe);
        self
    }

    pub fn liveness_probe(mut self, probe: ProbeHandle) -> Self {
        self.probes.liveness = Some(probe);
        self.probes.liveness_with_readiness = false;
        self
    }

    pub fn liveness_with_readiness_probe(mut self) -> Self {
        self.probes.liveness = None;
        self.probes.liveness_with_readiness = true;
        self
    }

    pub fn runtime_backend(mut self, backend: Arc<dyn RuntimeBackend>) -> Self {
        self.backend = Some(backend);
        self
    }

    pub async fn build(self) -> Result<CoreManager, Error> {
        CoreManager::build_configured(self).await
    }
}

impl CoreManager {
    pub fn builder(options: ManagerOptions) -> CoreManagerBuilder {
        CoreManagerBuilder {
            options,
            probes: ProbePlan::default(),
            backend: None,
        }
    }

    pub async fn new(options: ManagerOptions) -> Result<Self, Error> {
        Self::builder(options).build().await
    }

    async fn build_configured(builder: CoreManagerBuilder) -> Result<Self, Error> {
        let CoreManagerBuilder {
            options,
            probes,
            backend,
        } = builder;
        let runtime_dir = options
            .runtime_dir
            .as_ref()
            .ok_or_else(|| Error::InvalidManagerOptions("runtime_dir is required".into()))?;
        if runtime_dir.as_str().is_empty() {
            return Err(Error::InvalidManagerOptions(
                "runtime_dir must not be empty".into(),
            ));
        }
        if let Some(template) = options.controller_template.as_deref() {
            validate_controller_template(template)?;
        }
        for (name, timeout) in [
            ("control_timeout", options.control_timeout),
            ("reconcile_timeout", options.reconcile_timeout),
            ("stop_timeout", options.stop_timeout),
        ] {
            if timeout.is_zero() {
                return Err(Error::InvalidManagerOptions(format!(
                    "{name} must be greater than zero"
                )));
            }
        }
        if options.log_max_bytes == 0 {
            return Err(Error::InvalidManagerOptions(
                "log_max_bytes must be greater than zero".into(),
            ));
        }
        if options.log_max_files == 0 {
            return Err(Error::InvalidManagerOptions(
                "log_max_files must be greater than zero".into(),
            ));
        }
        let store = RuntimeConfigStore::new(runtime_dir.clone()).await?;
        let runtime_lock = store.acquire_ownership().await?;
        crate::config::managed_endpoint_path(
            store.dir(),
            options.controller_template.as_deref(),
            0,
        )?;
        let max_epoch = sweep_orphans(&store).await?;
        let (status_tx, _) = watch::channel(CoreStatus::initial());
        let (log_tx, _) = broadcast::channel(crate::log::LOG_CHANNEL_CAPACITY);
        let (log_dir, log_sink) = if options.log_sink_enabled {
            let dir = log_sink::prepare_dir(store.dir()).await?;
            let handle = log_sink::spawn(
                dir.clone(),
                SinkOptions {
                    max_bytes: options.log_max_bytes,
                    max_files: options.log_max_files,
                },
                log_tx.subscribe(),
                options.cancel_token.child_token(),
            )
            .await?;
            (Some(dir), Some(handle))
        } else {
            (None, None)
        };
        let backend = backend.unwrap_or_else(|| {
            Arc::new(ProcessRuntimeBackend::new(
                probes,
                options.cancel_token.clone(),
            ))
        });
        Ok(Self {
            inner: Arc::new(Inner {
                options,
                backend,
                store,
                _runtime_lock: runtime_lock,
                status_tx,
                log_tx,
                log_dir,
                log_sink: tokio::sync::Mutex::new(log_sink),
                version_cache: VersionCache::default(),
                operation: tokio::sync::Mutex::new(()),
                ctrl: tokio::sync::Mutex::new(Ctrl::default()),
                epoch: AtomicU64::new(max_epoch),
            }),
        })
    }

    pub fn subscribe(&self) -> watch::Receiver<CoreStatus> {
        self.inner.status_tx.subscribe()
    }

    pub fn status(&self) -> CoreStatus {
        self.inner.status_tx.borrow().clone()
    }

    pub fn subscribe_logs(&self) -> broadcast::Receiver<Arc<LogFrame>> {
        self.inner.log_tx.subscribe()
    }

    pub fn log_dir(&self) -> Option<&camino::Utf8Path> {
        self.inner.log_dir.as_deref()
    }

    #[cfg(feature = "test-hooks")]
    pub fn inject_runtime_parent_sync_failure_once_for_test(&self) {
        self.inner.store.inject_replace_parent_sync_failure_once();
    }

    pub async fn start(&self, spec: InstanceSpec) -> Result<(), Error> {
        let _operation = self.inner.operation.lock().await;
        self.start_inner(spec).await
    }

    async fn start_inner(&self, spec: InstanceSpec) -> Result<(), Error> {
        let snapshot = ConfigSnapshot::load(&spec.config_path).await?;
        self.start_inner_with_snapshot(spec, snapshot).await
    }

    async fn start_inner_with_snapshot(
        &self,
        spec: InstanceSpec,
        snapshot: ConfigSnapshot,
    ) -> Result<(), Error> {
        let mut ctrl = self.inner.ctrl.lock().await;
        reject_quarantine(&ctrl)?;
        if ctrl
            .current
            .as_ref()
            .is_some_and(|active| !active.instance.state().borrow().state.is_terminal())
        {
            return Err(Error::AlreadyRunning);
        }
        if let Some(stale) = ctrl.current.take() {
            let epoch = stale.revision.epoch;
            abort_and_await(stale.forwarder).await;
            if let Err(error) = stale
                .instance
                .stop_and_confirm_dead(self.inner.options.stop_timeout)
                .await
            {
                self.latch_quarantine(&mut ctrl, epoch, &error);
                return Err(error);
            }
            self.inner.store.cleanup_epoch(epoch).await?;
        }

        let epoch = self.inner.epoch.fetch_add(1, Ordering::SeqCst) + 1;
        let resolved = self.resolve_core_features(&spec.core).await?;
        let runtime_dir = self.inner.store.dir();
        let source_path = snapshot.source_path().to_owned();
        let source_document = snapshot.document().clone();
        let prepared = snapshot.prepare(
            self.inner.options.controller_template.as_deref(),
            runtime_dir,
            epoch,
            resolved.runtime,
        )?;
        self.warn_http_fallback(
            &spec.core,
            resolved.version.as_deref(),
            prepared.rewrote_controller,
        );
        let staged = self.inner.store.stage(epoch, &prepared.bytes).await?;
        let runtime_path = self.inner.store.commit_new(staged, epoch).await?;

        let mut effective_spec = spec.clone();
        effective_spec.config_path = runtime_path.clone();
        effective_spec.pid_file = Some(self.inner.store.pid_path(epoch));
        if effective_spec.core.version.is_none() {
            effective_spec.core.version = resolved.version.clone();
        }
        let revision = ConfigRevision {
            epoch,
            generation: 1,
            source_hash: prepared.source_hash,
            effective_hash: prepared.effective_hash,
            runtime_path,
        };
        let capabilities: Vec<_> = resolved.capabilities.iter().collect();
        let runtime_features: Vec<_> = resolved.runtime.iter().collect();
        let summary = SpecSummary {
            kind: spec.core.kind,
            config_path: source_path.clone(),
            capabilities: capabilities.clone(),
            runtime_features: runtime_features.clone(),
        };

        if let Err(error) = self.inner.backend.check_config(&effective_spec).await {
            let _ = self.inner.store.cleanup_epoch(epoch).await;
            self.inner.publish(
                CoreState::Stopped {
                    reason: Some(StopReason::Error(error.to_string())),
                },
                None,
                None,
                None,
                None,
            );
            return Err(error);
        }
        self.inner.publish(
            CoreState::Starting { epoch },
            None,
            Some(summary),
            Some(prepared.controller.host.clone()),
            Some(revision.clone()),
        );

        let instance = match self
            .inner
            .backend
            .launch(RuntimeLaunchRequest {
                effective_spec,
                epoch,
                controller: prepared.controller,
                log_tx: self.inner.log_tx.clone(),
            })
            .await
        {
            Ok(instance) => instance,
            Err(error) => {
                let _ = self.inner.store.cleanup_epoch(epoch).await;
                self.inner.publish(
                    CoreState::Stopped {
                        reason: Some(StopReason::Error(error.to_string())),
                    },
                    None,
                    None,
                    None,
                    None,
                );
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
                    self.inner.publish(
                        CoreState::Stopped {
                            reason: Some(StopReason::Error(readiness_error.to_string())),
                        },
                        None,
                        None,
                        None,
                        None,
                    );
                    return Err(readiness_error);
                }
                Err(stop_error) => {
                    let error = Error::StopUnconfirmed(format!(
                        "{readiness_error}; failed to stop rejected initial instance: {stop_error}"
                    ));
                    self.latch_quarantine(&mut ctrl, epoch, &error);
                    return Err(error);
                }
            }
        }
        let state = instance.state().borrow().clone();
        let pid = match state.state {
            InstanceState::Running { pid } => pid,
            _ => 0,
        };
        self.inner.publish(
            CoreState::Running { epoch, pid },
            state.health,
            Some(SpecSummary {
                kind: spec.core.kind,
                config_path: source_path,
                capabilities: capabilities.clone(),
                runtime_features: runtime_features.clone(),
            }),
            Some(instance.controller().host.clone()),
            Some(revision.clone()),
        );
        let forwarder = spawn_forwarder(self.inner.clone(), instance.state(), epoch);
        ctrl.last_spec = Some(spec.clone());
        ctrl.current = Some(Active {
            instance,
            forwarder,
            source_spec: spec,
            capabilities,
            runtime_features,
            source_document,
            effective_document: prepared.document,
            revision,
        });
        Ok(())
    }

    pub async fn stop(&self) -> Result<(), Error> {
        let _operation = self.inner.operation.lock().await;
        self.stop_inner().await
    }

    pub async fn shutdown(&self) -> Result<(), Error> {
        let _operation = self.inner.operation.lock().await;
        let result = match self.stop_inner().await {
            Err(Error::NotStarted) => Ok(()),
            result => result,
        };
        if let Some(sink) = self.inner.log_sink.lock().await.take() {
            sink.shutdown().await;
        }
        result
    }

    async fn stop_inner(&self) -> Result<(), Error> {
        let mut ctrl = self.inner.ctrl.lock().await;
        let Some(active) = ctrl.current.take() else {
            return Err(Error::NotStarted);
        };
        let epoch = active.instance.epoch();
        abort_and_await(active.forwarder).await;
        self.inner.status_tx.send_modify(|status| {
            status.state = CoreState::Stopping { epoch };
            status.changed_at = now_ms();
        });
        if let Err(error) = active
            .instance
            .stop_and_confirm_dead(self.inner.options.stop_timeout)
            .await
        {
            self.latch_quarantine(&mut ctrl, epoch, &error);
            return Err(error);
        }
        self.inner.store.cleanup_epoch(epoch).await?;
        self.inner.publish(
            CoreState::Stopped {
                reason: Some(StopReason::User),
            },
            None,
            None,
            None,
            None,
        );
        Ok(())
    }

    /// Restart the last requested spec as a new epoch.
    pub async fn restart(&self) -> Result<SwitchOutcome, Error> {
        let _operation = self.inner.operation.lock().await;
        let spec = {
            let ctrl = self.inner.ctrl.lock().await;
            reject_quarantine(&ctrl)?;
            ctrl.current
                .as_ref()
                .map(|active| active.source_spec.clone())
                .or_else(|| ctrl.last_spec.clone())
                .ok_or(Error::NotStarted)?
        };
        self.switch_inner(spec).await
    }

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

    async fn apply_config_inner(
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
        let candidate = snapshot.prepare(
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
                        tracing::warn!("failed to remove apply backup after commit failure: {cleanup}");
                    }
                    return Err(error);
                }
            };
            if let Some(warning) = commit.durability_warning() {
                durability_warnings.push(warning.to_owned());
            }
            let reconciled = match tokio::time::timeout(
                self.inner.options.reconcile_timeout,
                async {
                    let ctrl = self.inner.ctrl.lock().await;
                    let active = ctrl.current.as_ref().ok_or(Error::NotStarted)?;
                    Ok::<bool, Error>(
                        reconcile::reconcile_in_place(
                            active,
                            &change,
                            &previous_revision.runtime_path,
                            self.inner.options.control_timeout,
                        )
                        .await,
                    )
                },
            )
            .await
            {
                Ok(result) => result?,
                Err(_) => {
                    tracing::warn!("in-place reconciliation timed out; falling back to restart");
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
            let restore = self.inner.store.restore(&backup).await.map_err(|error| {
                Error::ApplyRollbackFailed {
                    apply: "in-place reconciliation failed".into(),
                    rollback: format!("runtime restore failed: {error}"),
                }
            })?;
            if let Some(warning) = restore.durability_warning() {
                durability_warnings.push(warning.to_owned());
            }
            if let Err(error) = self.inner.store.remove_backup(backup).await {
                tracing::warn!("failed to remove restored apply backup: {error}");
            }
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
                    let revision = self.status().revision.ok_or_else(|| {
                        Error::ApplyRollbackFailed {
                            apply: apply_error.to_string(),
                            rollback: "rollback epoch has no revision".into(),
                        }
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

    /// Converge the runtime toward the requested spec.
    ///
    /// This is the desired-state entry used by the v2 control plane: callers
    /// do not choose start/reload/restart/switch. The manager derives the
    /// transition from the current runtime and the requested configuration.
    pub async fn reconcile(
        &self,
        spec: InstanceSpec,
        expected_applied: Option<crate::RevisionId>,
    ) -> Result<ApplyOutcome, Error> {
        let _operation = self.inner.operation.lock().await;
        let running = {
            let ctrl = self.inner.ctrl.lock().await;
            reject_quarantine(&ctrl)?;
            ctrl.current.as_ref().is_some_and(|active| {
                !active.instance.state().borrow().state.is_terminal()
            })
        };
        if running {
            return self.apply_config_inner(spec, expected_applied).await;
        }
        if let Some(expected) = expected_applied {
            return Err(Error::RevisionConflict {
                expected,
                actual: None,
            });
        }
        self.start_inner(spec).await?;
        let revision = self
            .status()
            .revision
            .ok_or_else(|| Error::ApplyFailed("started epoch has no revision".into()))?;
        Ok(ApplyOutcome::Started { revision })
    }

    /// Switch to a requested spec as a new epoch.
    pub async fn switch(&self, spec: InstanceSpec) -> Result<SwitchOutcome, Error> {
        let _operation = self.inner.operation.lock().await;
        self.switch_inner(spec).await
    }

    async fn switch_inner(&self, spec: InstanceSpec) -> Result<SwitchOutcome, Error> {
        let (running, stale_exists) = {
            let ctrl = self.inner.ctrl.lock().await;
            reject_quarantine(&ctrl)?;
            (
                ctrl.current
                    .as_ref()
                    .is_some_and(|active| !active.instance.state().borrow().state.is_terminal()),
                ctrl.current.is_some(),
            )
        };
        if !running {
            if stale_exists {
                self.stop_inner().await?;
            }
            self.start_inner(spec).await?;
            return Ok(SwitchOutcome::Hard {
                reason: DegradeReason::NotRunning,
            });
        }

        let snapshot = ConfigSnapshot::load(&spec.config_path).await?;
        let resolved = self.resolve_core_features(&spec.core).await?;
        let local_controller = resolved
            .runtime
            .contains(crate::RuntimeFeature::LocalIpc);
        let reason = switching::graceful_degrade_reason(
            local_controller,
            spec.core.kind,
            crate::config::mihomo::overlap_block(snapshot.document()),
        );
        if let Some(reason) = reason {
            self.stop_inner().await?;
            self.start_inner(spec).await?;
            return Ok(SwitchOutcome::Hard { reason });
        }
        self.graceful_switch(spec, snapshot, resolved).await
    }

    /// Run one serialized reconciliation probe against the active epoch.
    pub async fn probe_reconcile(&self) -> Result<crate::ProbeResult, Error> {
        let _operation = self.inner.operation.lock().await;
        let ctrl = self.inner.ctrl.lock().await;
        reject_quarantine(&ctrl)?;
        let active = ctrl.current.as_ref().ok_or(Error::NotStarted)?;
        Ok(active.instance.probe_now(crate::ProbePhase::Reconcile).await)
    }

    /// Validate a config with the selected core binary without changing the
    /// manager's current state.
    pub async fn check_config(&self, spec: &crate::spec::InstanceSpec) -> Result<(), Error> {
        self.inner.backend.check_config(spec).await
    }

    fn warn_http_fallback(
        &self,
        core: &CoreSpec,
        resolved_version: Option<&str>,
        rewrote_controller: bool,
    ) {
        if self.inner.options.local_ipc_policy == LocalIpcPolicy::Prefer && !rewrote_controller {
            tracing::warn!(
                kind = %core.kind,
                version = resolved_version.or(core.version.as_deref()).unwrap_or("unknown"),
                "local IPC is unsupported; falling back to the configured HTTP controller"
            );
        }
    }

    pub(crate) async fn resolve_core_features(
        &self,
        core: &CoreSpec,
    ) -> Result<ResolvedFeatures, Error> {
        resolve_features(
            &self.inner.version_cache,
            core,
            self.inner.options.local_ipc_policy,
        )
        .await
    }

    pub fn options(&self) -> &ManagerOptions {
        &self.inner.options
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

async fn sweep_orphans(store: &RuntimeConfigStore) -> Result<u64, Error> {
    let epochs = store.artifact_epochs().await?;
    let maximum = epochs.last().copied().unwrap_or(0);
    for epoch in epochs {
        let pid_path = store.pid_path(epoch);
        if tokio::fs::try_exists(&pid_path).await? {
            nyanpasu_utils::process::reap_epoch_pid_file(
                pid_path.as_std_path(),
                store.dir().as_std_path(),
            )
            .await?;
        }
        store.cleanup_epoch(epoch).await?;
    }
    Ok(maximum)
}

async fn abort_and_await(handle: tokio::task::JoinHandle<()>) {
    handle.abort();
    let _ = handle.await;
}

fn validate_controller_template(template: &str) -> Result<(), Error> {
    if !template.contains("{epoch}") {
        return Err(Error::InvalidManagerOptions(
            "controller_template must contain `{epoch}`".into(),
        ));
    }
    if template.replace("{epoch}", "0").trim().is_empty() {
        return Err(Error::InvalidManagerOptions(
            "controller_template resolves to an empty endpoint".into(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    fn options() -> ManagerOptions {
        static NEXT_RUNTIME: AtomicU64 = AtomicU64::new(0);
        let runtime = std::env::temp_dir().join(format!(
            "nyanpasu-core-manager-test-{}-{}",
            std::process::id(),
            NEXT_RUNTIME.fetch_add(1, Ordering::Relaxed)
        ));
        ManagerOptions {
            runtime_dir: Some(camino::Utf8PathBuf::from_path_buf(runtime).unwrap()),
            ..ManagerOptions::default()
        }
    }

    #[tokio::test]
    async fn manager_publishes_the_initial_stopped_snapshot() {
        let options = options();
        let expected_runtime = options.runtime_dir.clone();
        let manager = CoreManager::new(options).await.unwrap();
        assert!(matches!(
            manager.status().state,
            crate::CoreState::Stopped { reason: None }
        ));
        assert_eq!(manager.options().runtime_dir, expected_runtime);
    }

    #[tokio::test]
    async fn runtime_directory_and_nonzero_timeouts_are_required() {
        assert!(CoreManager::new(ManagerOptions::default()).await.is_err());
        let mut invalid = options();
        invalid.control_timeout = Duration::ZERO;
        assert!(CoreManager::new(invalid).await.is_err());
    }

    #[tokio::test]
    async fn config_check_failure_does_not_change_manager_state() {
        let manager = CoreManager::new(options()).await.unwrap();
        let spec = crate::InstanceSpec {
            core: CoreSpec {
                kind: crate::kind::CoreKind::Mihomo,
                binary_path: "definitely-missing-core".into(),
                version: Some("v1.18.9".into()),
                features: Vec::new(),
            },
            config_path: "config.yaml".into(),
            working_dir: ".".into(),
            pid_file: None,
            options: crate::InstanceOptions::default(),
        };
        assert!(manager.check_config(&spec).await.is_err());
        assert!(matches!(
            manager.status().state,
            crate::CoreState::Stopped { reason: None }
        ));
    }

    #[tokio::test]
    async fn explicit_core_version_resolves_through_the_manager_cache() {
        let manager = CoreManager::new(options()).await.unwrap();
        let resolved = manager
            .resolve_core_features(&CoreSpec {
                kind: crate::kind::CoreKind::Mihomo,
                binary_path: "missing-core".into(),
                version: Some("v1.18.9".into()),
                features: Vec::new(),
            })
            .await
            .unwrap();
        assert_eq!(resolved.version.as_deref(), Some("v1.18.9"));
    }

    #[tokio::test]
    async fn runtime_config_publication_is_create_new_and_atomic() {
        let temp = tempfile::tempdir().unwrap();
        let root = camino::Utf8PathBuf::from_path_buf(temp.path().to_path_buf()).unwrap();
        let store = RuntimeConfigStore::new(root.clone()).await.unwrap();
        let first = store.stage(4, b"first: true\n").await.unwrap();
        let path = store.commit_new(first, 4).await.unwrap();
        let second = store.stage(4, b"second: true\n").await.unwrap();
        let error = store.commit_new(second, 4).await.unwrap_err();
        assert!(matches!(
            error,
            Error::Io(error) if error.kind() == std::io::ErrorKind::AlreadyExists
        ));
        assert_eq!(tokio::fs::read(&path).await.unwrap(), b"first: true\n");
        let mut entries = tokio::fs::read_dir(&root).await.unwrap();
        let mut names = Vec::new();
        while let Some(entry) = entries.next_entry().await.unwrap() {
            names.push(entry.file_name().to_string_lossy().into_owned());
        }
        assert_eq!(names, ["config-4.yaml"]);
    }

    #[tokio::test]
    async fn runtime_config_replacement_is_atomic_and_cleans_staging() {
        let temp = tempfile::tempdir().unwrap();
        let root = camino::Utf8PathBuf::from_path_buf(temp.path().to_path_buf()).unwrap();
        let store = RuntimeConfigStore::new(root.clone()).await.unwrap();
        let staged = store.stage(3, b"old: true\n").await.unwrap();
        let path = store.commit_new(staged, 3).await.unwrap();

        store.replace(3, b"new: true\n").await.unwrap();
        assert_eq!(tokio::fs::read(&path).await.unwrap(), b"new: true\n");
        let mut entries = tokio::fs::read_dir(&root).await.unwrap();
        let mut names = Vec::new();
        while let Some(entry) = entries.next_entry().await.unwrap() {
            names.push(entry.file_name().to_string_lossy().into_owned());
        }
        assert_eq!(names, ["config-3.yaml"]);
    }

    #[tokio::test]
    async fn construction_resumes_after_the_highest_epoch_artifact() {
        let temp = tempfile::tempdir().unwrap();
        let root = camino::Utf8PathBuf::from_path_buf(temp.path().to_path_buf()).unwrap();
        tokio::fs::write(root.join("config-7.yaml"), b"old").await.unwrap();
        tokio::fs::write(root.join("core-9.sock"), b"old").await.unwrap();
        tokio::fs::write(root.join("config-noise.yaml.tmp"), b"ignored")
            .await
            .unwrap();

        let manager = CoreManager::new(ManagerOptions {
            runtime_dir: Some(root.clone()),
            ..ManagerOptions::default()
        })
        .await
        .unwrap();
        assert_eq!(manager.inner.epoch.load(Ordering::SeqCst), 9);
        assert!(!root.join("config-7.yaml").exists());
        assert!(!root.join("core-9.sock").exists());
        assert!(root.join("config-noise.yaml.tmp").exists());
    }

    #[tokio::test]
    async fn construction_reaps_identity_recorded_orphans() {
        let temp = tempfile::tempdir().unwrap();
        let root = camino::Utf8PathBuf::from_path_buf(temp.path().to_path_buf()).unwrap();
        let config = root.join("config-11.yaml");
        let pid = root.join("core-11.pid");
        tokio::fs::write(&config, b"old").await.unwrap();
        nyanpasu_utils::process::publish_epoch_pid_file(
            pid.as_std_path(),
            &nyanpasu_utils::process::EpochPidRecord {
                pid: u32::MAX - 1,
                epoch: 11,
                executable: "definitely-exited-core".into(),
                start_token: 1,
                runtime_config: config.as_std_path().to_path_buf(),
            },
        )
        .await
        .unwrap();

        let manager = CoreManager::new(ManagerOptions {
            runtime_dir: Some(root.clone()),
            ..ManagerOptions::default()
        })
        .await
        .unwrap();
        assert_eq!(manager.inner.epoch.load(Ordering::SeqCst), 11);
        assert!(!pid.exists());
        assert!(!config.exists());
    }

    #[tokio::test]
    async fn controller_template_must_include_epoch() {
        let mut invalid = options();
        invalid.controller_template = Some("core.sock".into());
        assert!(CoreManager::new(invalid).await.is_err());

        let mut valid = options();
        valid.controller_template = Some("core-{epoch}.sock".into());
        assert!(CoreManager::new(valid).await.is_ok());
    }
}
