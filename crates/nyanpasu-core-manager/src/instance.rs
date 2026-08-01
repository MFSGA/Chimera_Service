//! Single-epoch core instance: process supervision and health-probed state.

use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use nyanpasu_utils::process::{
    Command, EpochPidFile, ReadinessProbe, Supervisor, SupervisorEvent,
};
use tokio::sync::{mpsc, watch};
use tokio_util::sync::CancellationToken;

use crate::{
    ControllerVersionProbe, Error, ProbeHandle, ProbePhase, ProbeResult,
    health::{
        HealthTracker, TrackerState,
        driver::ProbeDriver,
    },
    kind::{self, CLICOLOR_FORCE_ENV_NAME, MIHOMO_SAFE_PATHS_ENV_NAME},
    spec::{InstanceSpec, ResolvedController},
    state::{HealthState, HealthStatus, InstanceState, InstanceStatus, StopReason},
};

pub struct Instance {
    epoch: u64,
    spec: Arc<InstanceSpec>,
    controller: Arc<ResolvedController>,
    state_rx: watch::Receiver<InstanceStatus>,
    shared: Arc<Shared>,
}

struct Shared {
    state_tx: watch::Sender<InstanceStatus>,
    user_stop: AtomicBool,
    cancel: CancellationToken,
    supervisor: tokio::sync::Mutex<Option<Supervisor>>,
    monitor: tokio::sync::Mutex<Option<tokio::task::JoinHandle<()>>>,
    probe_request_tx: mpsc::UnboundedSender<ProbeRequest>,
}

struct ProbeRequest {
    response: tokio::sync::oneshot::Sender<ProbeResult>,
}

pub struct InstanceBuilder {
    spec: InstanceSpec,
    epoch: u64,
    controller: ResolvedController,
    parent: CancellationToken,
    readiness_probe: Option<ProbeHandle>,
    liveness_probe: Option<ProbeHandle>,
    liveness_with_readiness: bool,
}

impl Instance {
    pub fn builder(
        spec: InstanceSpec,
        epoch: u64,
        controller: ResolvedController,
        parent: CancellationToken,
    ) -> InstanceBuilder {
        InstanceBuilder {
            spec,
            epoch,
            controller,
            parent,
            readiness_probe: None,
            liveness_probe: None,
            liveness_with_readiness: false,
        }
    }

    pub async fn spawn(
        spec: InstanceSpec,
        epoch: u64,
        controller: ResolvedController,
        parent: CancellationToken,
    ) -> Result<Self, Error> {
        Self::builder(spec, epoch, controller, parent)
            .spawn()
            .await
    }

    async fn spawn_configured(builder: InstanceBuilder) -> Result<Self, Error> {
        let InstanceBuilder {
            spec,
            epoch,
            controller,
            parent,
            readiness_probe,
            liveness_probe,
            liveness_with_readiness,
        } = builder;
        if tokio::fs::metadata(&spec.config_path).await.is_err() {
            return Err(Error::ConfigNotFound(spec.config_path.clone()));
        }
        if tokio::fs::metadata(&spec.core.binary_path).await.is_err() {
            return Err(Error::BinaryNotFound(spec.core.binary_path.clone()));
        }

        let readiness = match readiness_probe {
            Some(probe) => probe,
            None => ProbeHandle::new(
                "controller-version",
                ControllerVersionProbe::new(&controller)?,
            ),
        };
        let liveness = if liveness_with_readiness {
            Some(readiness.clone())
        } else {
            liveness_probe
        };
        let startup_timeout = spec.options.startup_timeout;
        let spec = Arc::new(spec);
        let controller = Arc::new(controller);
        let cancel = parent.child_token();
        let (state_tx, state_rx) = watch::channel(InstanceStatus::initial());
        let (probe_request_tx, probe_request_rx) = mpsc::unbounded_channel();
        let shared = Arc::new(Shared {
            state_tx,
            user_stop: AtomicBool::new(false),
            cancel: cancel.clone(),
            supervisor: tokio::sync::Mutex::new(None),
            monitor: tokio::sync::Mutex::new(None),
            probe_request_tx,
        });
        let (event_tx, event_rx) = mpsc::unbounded_channel();
        let supervisor = Supervisor::builder({
            let spec = spec.clone();
            let controller = controller.clone();
            move || build_command(&spec, epoch, &controller)
        })
        .restart_policy(spec.options.restart_policy)
        .backoff(spec.options.backoff)
        .readiness(ReadinessProbe::Acknowledged)
        .cancel_token(cancel.clone())
        .on_event(move |event| {
            let _ = event_tx.send(event);
        })
        .spawn()
        .await?;
        *shared.supervisor.lock().await = Some(supervisor);

        let instance = Self {
            epoch,
            spec: spec.clone(),
            controller: controller.clone(),
            state_rx,
            shared: shared.clone(),
        };
        let monitor = tokio::spawn(monitor_loop(
            event_rx,
            shared.clone(),
            epoch,
            controller,
            readiness,
            liveness,
            spec.options.health.clone(),
            probe_request_rx,
        ));
        *shared.monitor.lock().await = Some(monitor);

        if let Err(error) = instance.wait_until_ready(startup_timeout).await {
            let _ = instance.stop().await;
            return Err(error);
        }
        Ok(instance)
    }

    async fn wait_until_ready(&self, timeout: Duration) -> Result<(), Error> {
        let mut states = self.subscribe();
        let result = tokio::time::timeout(timeout, async {
            loop {
                match &states.borrow().state {
                    InstanceState::Running { .. } => return Ok(()),
                    InstanceState::Stopped(reason) => {
                        return Err(Error::StartupFailed {
                            stderr_tail: reason.to_string(),
                        });
                    }
                    _ => {}
                }
                states.changed().await.map_err(|_| Error::StartupFailed {
                    stderr_tail: "instance state channel closed".into(),
                })?;
            }
        })
        .await;
        match result {
            Ok(result) => result,
            Err(_) => Err(Error::StartupTimeout {
                stderr_tail: "controller readiness probe did not become healthy".into(),
            }),
        }
    }

    pub fn epoch(&self) -> u64 {
        self.epoch
    }

    pub fn spec(&self) -> &InstanceSpec {
        &self.spec
    }

    pub fn controller(&self) -> &ResolvedController {
        &self.controller
    }

    pub fn subscribe(&self) -> watch::Receiver<InstanceStatus> {
        self.state_rx.clone()
    }

    pub fn status(&self) -> InstanceStatus {
        self.state_rx.borrow().clone()
    }

    /// Request one serialized reconciliation probe for the active epoch.
    pub async fn probe_now(&self, phase: ProbePhase) -> ProbeResult {
        if phase != ProbePhase::Reconcile {
            return ProbeResult::Unhealthy {
                detail: Some("only reconciliation probes can be requested directly".into()),
            };
        }
        let (response, result) = tokio::sync::oneshot::channel();
        if self
            .shared
            .probe_request_tx
            .send(ProbeRequest { response })
            .is_err()
        {
            return ProbeResult::Unhealthy {
                detail: Some("instance probe monitor is not running".into()),
            };
        }
        result.await.unwrap_or_else(|_| ProbeResult::Unhealthy {
            detail: Some("instance probe driver stopped".into()),
        })
    }

    pub async fn stop(&self) -> Result<(), Error> {
        self.shared.user_stop.store(true, Ordering::SeqCst);
        self.shared.cancel.cancel();
        if let Some(supervisor) = self.shared.supervisor.lock().await.take() {
            supervisor.stop().await?;
        }
        if let Some(monitor) = self.shared.monitor.lock().await.take() {
            let _ = monitor.await;
        }
        Ok(())
    }
}

impl Drop for Instance {
    fn drop(&mut self) {
        self.shared.cancel.cancel();
    }
}

impl InstanceBuilder {
    pub fn readiness_probe(mut self, probe: ProbeHandle) -> Self {
        self.readiness_probe = Some(probe);
        self
    }

    pub fn liveness_probe(mut self, probe: ProbeHandle) -> Self {
        self.liveness_probe = Some(probe);
        self.liveness_with_readiness = false;
        self
    }

    pub fn liveness_with_readiness_probe(mut self) -> Self {
        self.liveness_probe = None;
        self.liveness_with_readiness = true;
        self
    }

    pub async fn spawn(self) -> Result<Instance, Error> {
        Instance::spawn_configured(self).await
    }
}

fn build_command(spec: &InstanceSpec, epoch: u64, controller: &ResolvedController) -> Command {
    let config_dir = spec.config_path.parent().unwrap_or(&spec.working_dir);
    let mut command = Command::new(spec.core.binary_path.as_str())
        .args(kind::run_args(
            spec.core.kind,
            &spec.working_dir,
            &spec.config_path,
        ))
        .args(kind::controller_args(spec.core.kind, &controller.host))
        .current_dir(spec.working_dir.as_std_path())
        .env(
            MIHOMO_SAFE_PATHS_ENV_NAME,
            kind::mihomo_safe_paths(&spec.working_dir, config_dir),
        )
        .env(CLICOLOR_FORCE_ENV_NAME, "0");
    if let Some(pid_file) = &spec.pid_file {
        command = command.epoch_pid_file(EpochPidFile::new(
            pid_file.as_std_path(),
            epoch,
            spec.config_path.as_std_path(),
        ));
    }
    command
}

#[allow(clippy::too_many_arguments)]
async fn monitor_loop(
    mut events: mpsc::UnboundedReceiver<SupervisorEvent>,
    shared: Arc<Shared>,
    epoch: u64,
    controller: Arc<ResolvedController>,
    readiness: ProbeHandle,
    liveness: Option<ProbeHandle>,
    policy: crate::HealthPolicy,
    mut probe_requests: mpsc::UnboundedReceiver<ProbeRequest>,
) {
    let (observation_tx, mut observations) = mpsc::unbounded_channel();
    let mut driver: Option<ProbeDriver> = None;
    let mut tracker: Option<HealthTracker> = None;
    let mut run_id = 0_u64;
    let mut pid = 0_u32;

    loop {
        tokio::select! {
            biased;
            _ = shared.cancel.cancelled() => break,
            event = events.recv() => match event {
                Some(SupervisorEvent::Started { pid: started_pid }) => {
                    if let Some(old) = driver.take() {
                        old.stop().await;
                    }
                    run_id = run_id.saturating_add(1);
                    pid = started_pid;
                    tracker = Some(HealthTracker::new(policy.clone(), std::time::Instant::now()));
                    shared.publish(InstanceState::Starting, Some(HealthStatus::starting()));
                    driver = Some(ProbeDriver::start(
                        epoch,
                        run_id,
                        pid,
                        controller.clone(),
                        readiness.clone(),
                        liveness.clone(),
                        policy.clone(),
                        observation_tx.clone(),
                    ));
                }
                Some(SupervisorEvent::Ready) => {
                    shared.publish_state(InstanceState::Running { pid });
                    if let Some(driver) = &driver {
                        driver.use_liveness();
                    }
                }
                Some(SupervisorEvent::Restarting { attempt, .. }) => {
                    shared.publish_state(InstanceState::Restarting { attempt });
                }
                Some(SupervisorEvent::GaveUp) => {
                    shared.publish(InstanceState::Stopped(StopReason::Error(
                        "restart policy exhausted".into(),
                    )), None);
                    break;
                }
                Some(SupervisorEvent::Stopped) | None => {
                    let reason = if shared.user_stop.load(Ordering::SeqCst) {
                        StopReason::User
                    } else {
                        StopReason::Finished
                    };
                    shared.publish(InstanceState::Stopped(reason), None);
                    break;
                }
                Some(SupervisorEvent::Exited(_)) => {}
                Some(_) => {}
            },
            request = probe_requests.recv() => {
                let Some(request) = request else { continue };
                match &driver {
                    Some(driver) => driver.reconcile(request.response),
                    None => {
                        let _ = request.response.send(ProbeResult::Unhealthy {
                            detail: Some("instance probe driver is not ready".into()),
                        });
                    }
                }
            }
            observation = observations.recv() => {
                let Some(observation) = observation else { continue };
                if observation.run_id != run_id || observation.pid != pid {
                    continue;
                }
                let Some(tracker) = tracker.as_mut() else { continue };
                let update = tracker.observe(observation.completed_at, &observation.result);
                let readiness_succeeded = observation.phase == ProbePhase::Readiness
                    && update.state == TrackerState::Healthy;
                shared.publish_health(update, observation.completed_at_ms);
                if readiness_succeeded
                    && let Some(supervisor) = shared.supervisor.lock().await.as_ref()
                {
                    let _ = supervisor.acknowledge_ready(pid).await;
                }
            }
        }
    }
    if let Some(driver) = driver {
        driver.stop().await;
    }
}

impl Shared {
    fn publish_state(&self, state: InstanceState) {
        self.state_tx.send_modify(|status| status.state = state.clone());
    }

    fn publish(&self, state: InstanceState, health: Option<HealthStatus>) {
        self.state_tx.send_modify(|status| {
            status.state = state.clone();
            status.health = health.clone();
        });
    }

    fn publish_health(&self, update: crate::health::TrackerUpdate, observed_at: i64) {
        self.state_tx.send_modify(|status| {
            let previous = status.health.as_ref();
            let state = match update.state {
                TrackerState::Starting => HealthState::Starting,
                TrackerState::Healthy => HealthState::Healthy,
                TrackerState::Unhealthy => HealthState::Unhealthy,
            };
            status.health = Some(HealthStatus {
                state,
                changed_at: if update.transitioned {
                    observed_at
                } else {
                    previous.map_or(observed_at, |health| health.changed_at)
                },
                consecutive_failures: update.consecutive_failures,
                last_error: update.last_error,
                last_success_at: if state == HealthState::Healthy {
                    Some(observed_at)
                } else {
                    previous.and_then(|health| health.last_success_at)
                },
            });
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{CoreSpec, InstanceOptions, kind::CoreKind};

    #[tokio::test]
    async fn missing_artifacts_fail_before_spawning() {
        let spec = InstanceSpec {
            core: CoreSpec {
                kind: CoreKind::Mihomo,
                binary_path: "definitely-missing-core".into(),
                version: Some("1.18.9".into()),
                features: Vec::new(),
            },
            config_path: "definitely-missing-config.yaml".into(),
            working_dir: ".".into(),
            pid_file: None,
            options: InstanceOptions::default(),
        };
        let controller = ResolvedController {
            host: clash_api::Host::http("127.0.0.1:1").unwrap(),
            secret: None,
        };
        assert!(matches!(
            Instance::spawn(spec, 1, controller, CancellationToken::new()).await,
            Err(Error::ConfigNotFound(_))
        ));
    }
}
