//! Cross-epoch orchestration and atomic status publication.

use std::sync::Arc;

use tokio::sync::watch;

use crate::{
    Error, ProbeHandle,
    capability::{ResolvedFeatures, VersionCache, resolve_features},
    spec::{CoreSpec, ManagerOptions},
    state::{ConfigRevision, CoreStatus},
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
            }),
        })
    }

    pub fn subscribe(&self) -> watch::Receiver<CoreStatus> {
        self.inner.status_tx.subscribe()
    }

    pub fn status(&self) -> CoreStatus {
        self.inner.status_tx.borrow().clone()
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
