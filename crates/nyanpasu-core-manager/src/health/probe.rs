//! Custom readiness and liveness probes.

use std::{future::Future, pin::Pin, sync::Arc, time::Duration};

use tokio_util::sync::CancellationToken;

use crate::{Error, ResolvedController};

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

/// Default readiness probe: healthy when the controller answers `/version`.
pub struct ControllerVersionProbe {
    client: clash_api::Client,
}

impl ControllerVersionProbe {
    pub fn new(controller: &ResolvedController) -> Result<Self, Error> {
        let mut builder =
            clash_api::Client::builder(controller.host.clone()).timeout(Duration::from_secs(1));
        if let Some(secret) = &controller.secret {
            builder = builder.secret(secret.as_str());
        }
        Ok(Self {
            client: builder.build()?,
        })
    }
}

impl HealthProbe for ControllerVersionProbe {
    fn check<'a>(&'a self, _context: ProbeContext) -> ProbeFuture<'a> {
        Box::pin(async move {
            match self.client.version().await {
                Ok(_) => ProbeResult::Healthy,
                Err(error) => ProbeResult::Unhealthy {
                    detail: Some(error.to_string()),
                },
            }
        })
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

    #[tokio::test]
    async fn controller_version_probe_reflects_endpoint_health() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = [0_u8; 1024];
            let _ = stream.read(&mut request).await;
            let body = r#"{"meta":true,"version":"test"}"#;
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            stream.write_all(response.as_bytes()).await.unwrap();
        });
        let controller = ResolvedController {
            host: clash_api::Host::http(address.to_string()).unwrap(),
            secret: None,
        };
        let probe = ControllerVersionProbe::new(&controller).unwrap();
        let mut context = context();
        context.controller = Arc::new(controller);
        assert!(probe.check(context).await.is_healthy());
    }

    #[test]
    fn handle_debug_prints_only_the_label() {
        let probe = ProbeHandle::from_fn("safe-label", |_| async { ProbeResult::Healthy });
        let debug = format!("{probe:?}");
        assert_eq!(debug, "ProbeHandle { label: \"safe-label\" }");
        assert!(!debug.contains("do-not-log"));
    }
}
