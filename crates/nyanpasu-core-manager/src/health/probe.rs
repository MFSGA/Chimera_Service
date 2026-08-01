//! Custom readiness and liveness probes.

use std::{future::Future, pin::Pin, sync::Arc};

use tokio_util::sync::CancellationToken;

use crate::ResolvedController;

/// The boxed future returned by an object-safe [`HealthProbe`].
pub type ProbeFuture<'a> = Pin<Box<dyn Future<Output = ProbeResult> + Send + 'a>>;

#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProbeResult {
    Healthy,
    Unhealthy { detail: Option<String> },
}

impl ProbeResult {
    pub fn is_healthy(&self) -> bool {
        matches!(self, Self::Healthy)
    }
}

#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProbePhase {
    Readiness,
    Liveness,
    Reconcile,
}

/// Context for one probe attempt. Deliberately not `Debug`: the controller may
/// carry an authentication secret.
#[derive(Clone)]
pub struct ProbeContext {
    pub epoch: u64,
    pub pid: u32,
    pub phase: ProbePhase,
    pub controller: Arc<ResolvedController>,
    pub cancel: CancellationToken,
}

pub trait HealthProbe: Send + Sync + 'static {
    fn check<'a>(&'a self, context: ProbeContext) -> ProbeFuture<'a>;
}

/// Cheaply cloneable, debug-safe handle to a custom probe.
#[derive(Clone)]
pub struct ProbeHandle {
    label: Arc<str>,
    inner: Arc<dyn HealthProbe>,
}

impl ProbeHandle {
    pub fn new(label: impl Into<Arc<str>>, probe: impl HealthProbe) -> Self {
        Self {
            label: label.into(),
            inner: Arc::new(probe),
        }
    }

    pub fn from_fn<F, Fut>(label: impl Into<Arc<str>>, function: F) -> Self
    where
        F: Fn(ProbeContext) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = ProbeResult> + Send + 'static,
    {
        Self::new(label, FnProbe(function))
    }

    pub fn label(&self) -> &str {
        &self.label
    }

    pub fn check(&self, context: ProbeContext) -> ProbeFuture<'_> {
        self.inner.check(context)
    }
}

impl std::fmt::Debug for ProbeHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProbeHandle")
            .field("label", &self.label)
            .finish()
    }
}

struct FnProbe<F>(F);

impl<F, Fut> HealthProbe for FnProbe<F>
where
    F: Fn(ProbeContext) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = ProbeResult> + Send + 'static,
{
    fn check<'a>(&'a self, context: ProbeContext) -> ProbeFuture<'a> {
        Box::pin((self.0)(context))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn context() -> ProbeContext {
        ProbeContext {
            epoch: 3,
            pid: 9,
            phase: ProbePhase::Readiness,
            controller: Arc::new(ResolvedController {
                host: clash_api::Host::http("127.0.0.1:9090").unwrap(),
                secret: Some("do-not-log".into()),
            }),
            cancel: CancellationToken::new(),
        }
    }

    #[tokio::test]
    async fn function_probe_receives_context() {
        let probe = ProbeHandle::from_fn("ready", |context| async move {
            if context.epoch == 3 && context.pid == 9 {
                ProbeResult::Healthy
            } else {
                ProbeResult::Unhealthy { detail: None }
            }
        });
        assert!(probe.check(context()).await.is_healthy());
    }

    #[test]
    fn handle_debug_prints_only_the_label() {
        let probe = ProbeHandle::from_fn("safe-label", |_| async { ProbeResult::Healthy });
        let debug = format!("{probe:?}");
        assert_eq!(debug, "ProbeHandle { label: \"safe-label\" }");
        assert!(!debug.contains("do-not-log"));
    }
}
