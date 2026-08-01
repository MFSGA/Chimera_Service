use std::{sync::Arc, time::Duration};

use tokio::{sync::mpsc, task::JoinHandle};
use tokio_util::sync::CancellationToken;

use crate::{
    health::{
        HealthPolicy,
        probe::{ProbeContext, ProbeHandle, ProbePhase, ProbeResult},
    },
    spec::ResolvedController,
};

#[derive(Debug, Clone)]
pub(crate) struct ProbeObservation {
    pub(crate) run_id: u64,
    pub(crate) pid: u32,
    pub(crate) phase: ProbePhase,
    pub(crate) completed_at: std::time::Instant,
    pub(crate) completed_at_ms: i64,
    pub(crate) result: ProbeResult,
}

enum DriverCommand {
    UseLiveness,
    Reconcile {
        response: tokio::sync::oneshot::Sender<ProbeResult>,
    },
}

pub(crate) struct ProbeDriver {
    command_tx: mpsc::UnboundedSender<DriverCommand>,
    cancel: CancellationToken,
    task: Option<JoinHandle<()>>,
}

impl ProbeDriver {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn start(
        epoch: u64,
        run_id: u64,
        pid: u32,
        controller: Arc<ResolvedController>,
        readiness: ProbeHandle,
        liveness: Option<ProbeHandle>,
        policy: HealthPolicy,
        observation_tx: mpsc::UnboundedSender<ProbeObservation>,
    ) -> Self {
        let (command_tx, mut command_rx) = mpsc::unbounded_channel();
        let cancel = CancellationToken::new();
        let task_cancel = cancel.clone();
        let task = tokio::spawn(async move {
            let mut periodic = Some((readiness.clone(), ProbePhase::Readiness));
            let mut next_probe = tokio::time::Instant::now();

            loop {
                tokio::select! {
                    biased;
                    _ = task_cancel.cancelled() => break,
                    command = command_rx.recv() => match command {
                        Some(DriverCommand::UseLiveness) => {
                            periodic = liveness
                                .clone()
                                .map(|probe| (probe, ProbePhase::Liveness));
                            next_probe = tokio::time::Instant::now() + policy.interval();
                        }
                        Some(DriverCommand::Reconcile { response }) => {
                            let probe = liveness.as_ref().unwrap_or(&readiness).clone();
                            if let Some(observation) = run_attempt(
                                &probe,
                                epoch,
                                run_id,
                                pid,
                                ProbePhase::Reconcile,
                                controller.clone(),
                                policy.timeout(),
                                &task_cancel,
                            ).await {
                                let _ = response.send(observation.result.clone());
                                let _ = observation_tx.send(observation);
                            }
                        }
                        None => break,
                    },
                    _ = tokio::time::sleep_until(next_probe), if periodic.is_some() => {
                        let (probe, phase) = periodic.as_ref().expect("guarded by periodic");
                        if let Some(observation) = run_attempt(
                            probe,
                            epoch,
                            run_id,
                            pid,
                            *phase,
                            controller.clone(),
                            policy.timeout(),
                            &task_cancel,
                        ).await {
                            let _ = observation_tx.send(observation);
                        }
                        next_probe = tokio::time::Instant::now() + policy.interval();
                    }
                }
            }
        });
        Self {
            command_tx,
            cancel,
            task: Some(task),
        }
    }

    pub(crate) fn use_liveness(&self) {
        let _ = self.command_tx.send(DriverCommand::UseLiveness);
    }

    pub(crate) fn reconcile(&self, response: tokio::sync::oneshot::Sender<ProbeResult>) {
        let _ = self.command_tx.send(DriverCommand::Reconcile { response });
    }

    pub(crate) async fn stop(mut self) {
        self.cancel.cancel();
        if let Some(task) = self.task.take() {
            let _ = task.await;
        }
    }
}

impl Drop for ProbeDriver {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_attempt(
    probe: &ProbeHandle,
    epoch: u64,
    run_id: u64,
    pid: u32,
    phase: ProbePhase,
    controller: Arc<ResolvedController>,
    timeout: Duration,
    driver_cancel: &CancellationToken,
) -> Option<ProbeObservation> {
    let attempt_cancel = driver_cancel.child_token();
    let context = ProbeContext {
        epoch,
        pid,
        phase,
        controller,
        cancel: attempt_cancel.clone(),
    };
    let mut future = probe.check(context);
    let result = tokio::select! {
        biased;
        _ = driver_cancel.cancelled() => {
            attempt_cancel.cancel();
            return None;
        }
        timed = tokio::time::timeout(timeout, &mut future) => match timed {
            Ok(result) => result,
            Err(_) => {
                attempt_cancel.cancel();
                ProbeResult::Unhealthy {
                    detail: Some(format!("probe timed out after {timeout:?}")),
                }
            }
        },
    };
    drop(future);
    Some(ProbeObservation {
        run_id,
        pid,
        phase,
        completed_at: std::time::Instant::now(),
        completed_at_ms: crate::state::now_ms(),
        result,
    })
}

#[cfg(test)]
mod tests {
    use std::{
        future::Future,
        pin::Pin,
        sync::{
            Arc,
            atomic::{AtomicBool, AtomicUsize, Ordering},
        },
        task::{Context, Poll},
    };

    use super::*;
    use crate::health::probe::{HealthProbe, ProbeFuture};

    fn controller() -> Arc<ResolvedController> {
        Arc::new(ResolvedController {
            host: clash_api::Host::http("127.0.0.1:1").unwrap(),
            secret: None,
        })
    }

    fn policy(interval: Duration, timeout: Duration) -> HealthPolicy {
        HealthPolicy::new(
            interval,
            timeout,
            std::num::NonZeroU32::MIN,
            std::num::NonZeroU32::MIN,
            Duration::ZERO,
        )
        .unwrap()
    }

    #[tokio::test]
    async fn periodic_and_reconcile_attempts_are_serialized() {
        let active = Arc::new(AtomicUsize::new(0));
        let maximum = Arc::new(AtomicUsize::new(0));
        let release = Arc::new(tokio::sync::Semaphore::new(0));
        let probe = ProbeHandle::from_fn("serial", {
            let active = active.clone();
            let maximum = maximum.clone();
            let release = release.clone();
            move |_| {
                let active = active.clone();
                let maximum = maximum.clone();
                let release = release.clone();
                async move {
                    let current = active.fetch_add(1, Ordering::SeqCst) + 1;
                    maximum.fetch_max(current, Ordering::SeqCst);
                    let permit = release.acquire().await.unwrap();
                    permit.forget();
                    active.fetch_sub(1, Ordering::SeqCst);
                    ProbeResult::Healthy
                }
            }
        });
        let (observation_tx, mut observations) = mpsc::unbounded_channel();
        let driver = ProbeDriver::start(
            1,
            2,
            10,
            controller(),
            probe.clone(),
            Some(probe),
            policy(Duration::from_millis(1), Duration::from_secs(2)),
            observation_tx,
        );
        driver.use_liveness();
        tokio::time::sleep(Duration::from_millis(20)).await;
        let (response_tx, mut response_rx) = tokio::sync::oneshot::channel();
        driver.reconcile(response_tx);
        assert!(response_rx.try_recv().is_err());
        release.add_permits(1);
        let first = tokio::time::timeout(Duration::from_secs(1), observations.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(first.phase, ProbePhase::Liveness);
        assert_eq!(first.run_id, 2);
        assert_eq!(first.pid, 10);
        assert!(first.completed_at_ms > 0);
        assert!(first.completed_at <= std::time::Instant::now());
        release.add_permits(1);
        assert!(
            tokio::time::timeout(Duration::from_secs(1), response_rx)
                .await
                .unwrap()
                .unwrap()
                .is_healthy()
        );
        assert_eq!(maximum.load(Ordering::SeqCst), 1);
        driver.stop().await;
    }

    struct PendingProbe {
        dropped_after_cancel: Arc<AtomicBool>,
    }

    struct PendingFuture {
        cancel: CancellationToken,
        dropped_after_cancel: Arc<AtomicBool>,
    }

    impl Future for PendingFuture {
        type Output = ProbeResult;

        fn poll(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Self::Output> {
            Poll::Pending
        }
    }

    impl Drop for PendingFuture {
        fn drop(&mut self) {
            self.dropped_after_cancel
                .store(self.cancel.is_cancelled(), Ordering::SeqCst);
        }
    }

    impl HealthProbe for PendingProbe {
        fn check<'a>(&'a self, context: ProbeContext) -> ProbeFuture<'a> {
            Box::pin(PendingFuture {
                cancel: context.cancel,
                dropped_after_cancel: self.dropped_after_cancel.clone(),
            })
        }
    }

    #[tokio::test]
    async fn timeout_cancels_before_dropping_the_probe_future() {
        let dropped_after_cancel = Arc::new(AtomicBool::new(false));
        let probe = ProbeHandle::new(
            "pending",
            PendingProbe {
                dropped_after_cancel: dropped_after_cancel.clone(),
            },
        );
        let (tx, mut observations) = mpsc::unbounded_channel();
        let driver = ProbeDriver::start(
            1,
            1,
            10,
            controller(),
            probe,
            None,
            policy(Duration::from_millis(1), Duration::from_millis(20)),
            tx,
        );
        let observation = tokio::time::timeout(Duration::from_secs(1), observations.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(!observation.result.is_healthy());
        assert!(dropped_after_cancel.load(Ordering::SeqCst));
        driver.stop().await;
        assert!(observations.try_recv().is_err());
    }
}
