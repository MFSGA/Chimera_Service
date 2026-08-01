//! Cross-epoch orchestration and atomic status publication.

use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};

use tokio::sync::watch;

use crate::{
    Error, Instance, ProbeHandle,
    capability::{ResolvedFeatures, VersionCache, resolve_features},
    config::ConfigSnapshot,
    spec::{CoreSpec, InstanceSpec, ManagerOptions},
    state::{
        ConfigRevision, CoreState, CoreStatus, InstanceState, SpecSummary, StopReason, now_ms,
    },
};

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

pub struct CoreManager {
    inner: Arc<Inner>,
}

pub struct CoreManagerBuilder {
    options: ManagerOptions,
    probes: ProbePlan,
}

#[derive(Clone, Default)]
struct ProbePlan {
    readiness: Option<ProbeHandle>,
    liveness: Option<ProbeHandle>,
    liveness_with_readiness: bool,
}

struct Inner {
    options: ManagerOptions,
    probes: ProbePlan,
    status_tx: watch::Sender<CoreStatus>,
    version_cache: VersionCache,
    operation: tokio::sync::Mutex<()>,
    ctrl: tokio::sync::Mutex<Ctrl>,
    epoch: AtomicU64,
}

#[derive(Default)]
struct Ctrl {
    current: Option<Active>,
}

struct Active {
    instance: Instance,
    forwarder: tokio::task::JoinHandle<()>,
    source_spec: InstanceSpec,
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

    pub async fn build(self) -> Result<CoreManager, Error> {
        CoreManager::build_configured(self)
    }
}

impl CoreManager {
    pub fn builder(options: ManagerOptions) -> CoreManagerBuilder {
        CoreManagerBuilder {
            options,
            probes: ProbePlan::default(),
        }
    }

    pub async fn new(options: ManagerOptions) -> Result<Self, Error> {
        Self::builder(options).build().await
    }

    fn build_configured(builder: CoreManagerBuilder) -> Result<Self, Error> {
        let CoreManagerBuilder { options, probes } = builder;
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
        let (status_tx, _) = watch::channel(CoreStatus::initial());
        Ok(Self {
            inner: Arc::new(Inner {
                options,
                probes,
                status_tx,
                version_cache: VersionCache::default(),
                operation: tokio::sync::Mutex::new(()),
                ctrl: tokio::sync::Mutex::new(Ctrl::default()),
                epoch: AtomicU64::new(0),
            }),
        })
    }

    pub fn subscribe(&self) -> watch::Receiver<CoreStatus> {
        self.inner.status_tx.subscribe()
    }

    pub fn status(&self) -> CoreStatus {
        self.inner.status_tx.borrow().clone()
    }

    pub async fn start(&self, spec: InstanceSpec) -> Result<(), Error> {
        let _operation = self.inner.operation.lock().await;
        self.start_inner(spec).await
    }

    async fn start_inner(&self, spec: InstanceSpec) -> Result<(), Error> {
        let mut ctrl = self.inner.ctrl.lock().await;
        if ctrl
            .current
            .as_ref()
            .is_some_and(|active| !active.instance.status().state.is_terminal())
        {
            return Err(Error::AlreadyRunning);
        }
        if let Some(stale) = ctrl.current.take() {
            abort_and_await(stale.forwarder).await;
            let _ = stale.instance.stop().await;
            cleanup_epoch(&stale.revision).await;
        }

        let epoch = self.inner.epoch.fetch_add(1, Ordering::SeqCst) + 1;
        let resolved = self.resolve_core_features(&spec.core).await?;
        let runtime_dir = self
            .inner
            .options
            .runtime_dir
            .as_ref()
            .expect("validated runtime directory");
        tokio::fs::create_dir_all(runtime_dir).await?;
        let snapshot = ConfigSnapshot::load(&spec.config_path).await?;
        let source_path = snapshot.source_path().to_owned();
        let prepared = snapshot.prepare(
            self.inner.options.controller_template.as_deref(),
            runtime_dir,
            epoch,
            resolved.runtime,
        )?;
        let runtime_path = runtime_dir.join(format!("config-{epoch}.yaml"));
        tokio::fs::write(&runtime_path, &prepared.bytes).await?;

        let mut effective_spec = spec.clone();
        effective_spec.config_path = runtime_path.clone();
        effective_spec.pid_file = Some(runtime_dir.join(format!("core-{epoch}.pid")));
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

        if let Err(error) = crate::kind::check_config(&effective_spec).await {
            cleanup_epoch(&revision).await;
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

        let mut builder = Instance::builder(
            effective_spec,
            epoch,
            prepared.controller,
            self.inner.options.cancel_token.clone(),
        );
        if let Some(probe) = self.inner.probes.readiness.clone() {
            builder = builder.readiness_probe(probe);
        }
        if let Some(probe) = self.inner.probes.liveness.clone() {
            builder = builder.liveness_probe(probe);
        } else if self.inner.probes.liveness_with_readiness {
            builder = builder.liveness_with_readiness_probe();
        }
        let instance = match builder.spawn().await {
            Ok(instance) => instance,
            Err(error) => {
                cleanup_epoch(&revision).await;
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
        let pid = match instance.status().state {
            InstanceState::Running { pid } => pid,
            _ => 0,
        };
        self.inner.publish(
            CoreState::Running { epoch, pid },
            instance.status().health,
            Some(SpecSummary {
                kind: spec.core.kind,
                config_path: source_path,
                capabilities: capabilities.clone(),
                runtime_features: runtime_features.clone(),
            }),
            Some(instance.controller().host.clone()),
            Some(revision.clone()),
        );
        let forwarder = spawn_forwarder(self.inner.clone(), instance.subscribe(), epoch);
        ctrl.current = Some(Active {
            instance,
            forwarder,
            source_spec: spec,
            revision,
        });
        Ok(())
    }

    pub async fn stop(&self) -> Result<(), Error> {
        let _operation = self.inner.operation.lock().await;
        self.stop_inner().await
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
        active.instance.stop().await?;
        cleanup_epoch(&active.revision).await;
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
    ///
    /// This currently performs the hard-switch path. HTTP controllers cannot
    /// overlap; local non-Mihomo cores are also hard-switched. Local Mihomo
    /// reports `PatchFailed` until graceful overlap preparation is migrated.
    pub async fn restart(&self) -> Result<SwitchOutcome, Error> {
        let _operation = self.inner.operation.lock().await;
        let (spec, reason) = {
            let ctrl = self.inner.ctrl.lock().await;
            let active = ctrl.current.as_ref().ok_or(Error::NotStarted)?;
            (active.source_spec.clone(), hard_restart_reason(active))
        };
        self.stop_inner().await?;
        self.start_inner(spec).await?;
        Ok(SwitchOutcome::Hard { reason })
    }

    /// Apply a desired config with optimistic revision checking.
    ///
    /// In-place patch/reload classification is still pending. Non-noop changes
    /// use a checked hard restart; if the desired epoch fails to start, the
    /// previous source spec is started again and reported as `RolledBack`.
    pub async fn apply_config(
        &self,
        input: InstanceSpec,
        expected_revision: Option<crate::RevisionId>,
    ) -> Result<ApplyOutcome, Error> {
        let _operation = self.inner.operation.lock().await;
        let (previous_spec, previous_revision) = {
            let ctrl = self.inner.ctrl.lock().await;
            let active = ctrl.current.as_ref().ok_or(Error::NotStarted)?;
            if active.instance.status().state.is_terminal() {
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
            (active.source_spec.clone(), active.revision.clone())
        };

        let resolved = self.resolve_core_features(&input.core).await?;
        let runtime_dir = self
            .inner
            .options
            .runtime_dir
            .as_ref()
            .expect("validated runtime directory");
        let snapshot = ConfigSnapshot::load(&input.config_path).await?;
        let candidate = snapshot.prepare(
            self.inner.options.controller_template.as_deref(),
            runtime_dir,
            previous_revision.epoch,
            resolved.runtime,
        )?;
        if input == previous_spec
            && candidate.source_hash == previous_revision.source_hash
            && candidate.effective_hash == previous_revision.effective_hash
        {
            return Ok(ApplyOutcome::Noop {
                revision: previous_revision,
            });
        }

        // Reject invalid desired input before taking the active core down.
        crate::kind::check_config(&input).await?;
        let switched = process_spec_changed(&previous_spec, &input);
        self.stop_inner().await?;
        match self.start_inner(input.clone()).await {
            Ok(()) => {
                let revision = self
                    .status()
                    .revision
                    .ok_or_else(|| Error::ApplyFailed("started epoch has no revision".into()))?;
                Ok(if switched {
                    ApplyOutcome::Switched { revision }
                } else {
                    ApplyOutcome::Restarted { revision }
                })
            }
            Err(apply_error) => match self.start_inner(previous_spec).await {
                Ok(()) => {
                    let revision = self.status().revision.ok_or_else(|| {
                        Error::ApplyRollbackFailed {
                            apply: apply_error.to_string(),
                            rollback: "rollback epoch has no revision".into(),
                        }
                    })?;
                    Ok(ApplyOutcome::RolledBack {
                        revision,
                        failed_apply: apply_error.to_string(),
                    })
                }
                Err(rollback_error) => Err(Error::ApplyRollbackFailed {
                    apply: apply_error.to_string(),
                    rollback: rollback_error.to_string(),
                }),
            },
        }
    }

    /// Switch to a requested spec as a new epoch.
    ///
    /// The graceful-overlap path is still pending; this method currently
    /// provides the target API with a deterministic hard-switch fallback.
    pub async fn switch(&self, spec: InstanceSpec) -> Result<SwitchOutcome, Error> {
        let _operation = self.inner.operation.lock().await;
        let reason = {
            let ctrl = self.inner.ctrl.lock().await;
            match ctrl.current.as_ref() {
                Some(active) if !active.instance.status().state.is_terminal() => {
                    hard_switch_reason(active, &spec)
                }
                _ => DegradeReason::NotRunning,
            }
        };
        if reason != DegradeReason::NotRunning {
            self.stop_inner().await?;
        } else {
            let stale_exists = self.inner.ctrl.lock().await.current.is_some();
            if stale_exists {
                let _ = self.stop_inner().await;
            }
        }
        self.start_inner(spec).await?;
        Ok(SwitchOutcome::Hard { reason })
    }

    /// Run one serialized reconciliation probe against the active epoch.
    pub async fn reconcile(&self) -> Result<crate::ProbeResult, Error> {
        let _operation = self.inner.operation.lock().await;
        let ctrl = self.inner.ctrl.lock().await;
        let active = ctrl.current.as_ref().ok_or(Error::NotStarted)?;
        Ok(active.instance.probe_now(crate::ProbePhase::Reconcile).await)
    }

    /// Validate a config with the selected core binary without changing the
    /// manager's current state.
    pub async fn check_config(&self, spec: &crate::spec::InstanceSpec) -> Result<(), Error> {
        crate::kind::check_config(spec).await
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

    pub fn has_custom_readiness_probe(&self) -> bool {
        self.inner.probes.readiness.is_some()
    }

    pub fn has_custom_liveness_probe(&self) -> bool {
        self.inner.probes.liveness.is_some() || self.inner.probes.liveness_with_readiness
    }
}

impl Inner {
    fn publish(
        &self,
        state: CoreState,
        health: Option<crate::HealthStatus>,
        spec: Option<SpecSummary>,
        controller: Option<clash_api::Host>,
        revision: Option<ConfigRevision>,
    ) {
        self.status_tx.send_modify(|status| {
            status.state = state.clone();
            status.changed_at = now_ms();
            status.health = health.clone();
            status.spec = spec.clone();
            status.controller = controller.clone();
            status.revision = revision.clone();
        });
    }
}

fn process_spec_changed(previous: &InstanceSpec, desired: &InstanceSpec) -> bool {
    previous.core != desired.core
        || previous.working_dir != desired.working_dir
        || previous.options != desired.options
}

fn hard_restart_reason(active: &Active) -> DegradeReason {
    hard_switch_reason(active, &active.source_spec)
}

fn hard_switch_reason(active: &Active, requested: &InstanceSpec) -> DegradeReason {
    match &active.instance.controller().host {
        clash_api::Host::Http(_) => DegradeReason::HttpController,
        _ if !matches!(requested.core.kind, crate::CoreKind::Mihomo) => {
            DegradeReason::UnsupportedKind
        }
        _ => DegradeReason::PatchFailed,
    }
}

fn spawn_forwarder(
    inner: Arc<Inner>,
    mut states: watch::Receiver<crate::InstanceStatus>,
    epoch: u64,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        while states.changed().await.is_ok() {
            let instance = states.borrow_and_update().clone();
            inner.status_tx.send_modify(|status| {
                status.state = map_instance_state(epoch, &instance.state);
                status.changed_at = now_ms();
                status.health = instance.health.clone();
            });
        }
    })
}

fn map_instance_state(epoch: u64, state: &InstanceState) -> CoreState {
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

async fn abort_and_await(handle: tokio::task::JoinHandle<()>) {
    handle.abort();
    let _ = handle.await;
}

async fn cleanup_epoch(revision: &ConfigRevision) {
    let _ = tokio::fs::remove_file(&revision.runtime_path).await;
    if let Some(parent) = revision.runtime_path.parent() {
        let _ = tokio::fs::remove_file(parent.join(format!("core-{}.pid", revision.epoch))).await;
        #[cfg(not(windows))]
        let _ = tokio::fs::remove_file(parent.join(format!("core-{}.sock", revision.epoch))).await;
    }
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
        ManagerOptions {
            runtime_dir: Some("runtime".into()),
            ..ManagerOptions::default()
        }
    }

    #[tokio::test]
    async fn manager_publishes_the_initial_stopped_snapshot() {
        let manager = CoreManager::new(options()).await.unwrap();
        assert!(matches!(
            manager.status().state,
            crate::CoreState::Stopped { reason: None }
        ));
        assert_eq!(manager.options().runtime_dir.as_deref(), Some("runtime".into()));
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
    async fn controller_template_must_include_epoch() {
        let mut invalid = options();
        invalid.controller_template = Some("core.sock".into());
        assert!(CoreManager::new(invalid).await.is_err());

        let mut valid = options();
        valid.controller_template = Some("core-{epoch}.sock".into());
        assert!(CoreManager::new(valid).await.is_ok());
    }
}
