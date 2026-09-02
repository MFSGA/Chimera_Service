//! Single-epoch core instance: process supervision and health-probed state.

use std::{
    collections::VecDeque,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use nyanpasu_utils::process::{
    Command, EpochPidFile, OrphanReapOutcome, ProcessError, ProcessEvent, ReadinessProbe,
    Supervisor, SupervisorEvent, reap_epoch_pid_file,
};
use tokio::sync::{broadcast, mpsc, watch};
use tokio_util::sync::CancellationToken;

use crate::{
    ControllerVersionProbe, Error, ProbeHandle, ProbePhase, ProbeResult,
    health::{HealthTracker, TrackerState, driver::ProbeDriver},
    kind::{self, CLICOLOR_FORCE_ENV_NAME, MIHOMO_SAFE_PATHS_ENV_NAME},
    log::{
        LOG_CHANNEL_CAPACITY, LogFrame, LogParser, LogStream, ParsedFrames, error_summary,
        format_tail,
    },
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

const LOG_TAIL_FRAMES: usize = 32;

struct Shared {
    state_tx: watch::Sender<InstanceStatus>,
    user_stop: AtomicBool,
    parser: std::sync::Mutex<LogParser>,
    log_tail: std::sync::Mutex<VecDeque<Arc<LogFrame>>>,
    log_tx: broadcast::Sender<Arc<LogFrame>>,
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
    log_tx: Option<broadcast::Sender<Arc<LogFrame>>>,
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
            log_tx: None,
        }
    }

    pub async fn spawn(
        spec: InstanceSpec,
        epoch: u64,
        controller: ResolvedController,
        parent: CancellationToken,
    ) -> Result<Self, Error> {
        Self::builder(spec, epoch, controller, parent).spawn().await
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
            log_tx,
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
        let spec = Arc::new(spec);
        let controller = Arc::new(controller);
        let cancel = parent.child_token();
        let (state_tx, state_rx) = watch::channel(InstanceStatus::initial());
        let (probe_request_tx, probe_request_rx) = mpsc::unbounded_channel();
        let shared = Arc::new(Shared {
            state_tx,
            user_stop: AtomicBool::new(false),
            parser: std::sync::Mutex::new(LogParser::new(spec.core.kind, epoch)),
            log_tail: std::sync::Mutex::new(VecDeque::with_capacity(LOG_TAIL_FRAMES)),
            log_tx: log_tx.unwrap_or_else(|| broadcast::channel(LOG_CHANNEL_CAPACITY).0),
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
        .on_process_event({
            let kind = spec.core.kind;
            let shared = shared.clone();
            move |event| {
                let frames = match event {
                    ProcessEvent::Stdout(line) => shared.parse(LogStream::Stdout, line),
                    ProcessEvent::Stderr(line) => shared.parse(LogStream::Stderr, line),
                    ProcessEvent::Terminated(_) => shared.finish_log_record(),
                    ProcessEvent::Error(error) => {
                        tracing::warn!(target: "core", epoch, %kind, "output pump: {error}");
                        [None, None]
                    }
                    _ => [None, None],
                };
                for frame in frames.into_iter().flatten() {
                    shared.publish_log_frame(frame);
                }
            }
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
                            stderr_tail: self.shared.failure_summary(&reason.to_string()),
                        });
                    }
                    _ => {}
                }
                states.changed().await.map_err(|_| Error::StartupFailed {
                    stderr_tail: self.shared.failure_summary("instance state channel closed"),
                })?;
            }
        })
        .await;
        match result {
            Ok(result) => result,
            Err(_) => Err(Error::StartupTimeout {
                stderr_tail: self
                    .shared
                    .failure_summary("controller readiness probe did not become healthy"),
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

    pub fn pid(&self) -> Option<u32> {
        match self.state_rx.borrow().state {
            InstanceState::Running { pid } => Some(pid),
            _ => None,
        }
    }

    /// Wait until the initial readiness probe has confirmed this epoch.
    pub async fn wait_ready(&self) -> Result<(), Error> {
        self.wait_until_ready(self.spec.options.startup_timeout)
            .await
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
        self.stop_and_confirm_dead(Duration::from_secs(10)).await
    }

    /// Stop supervision and prove the epoch process is dead before returning.
    pub async fn stop_and_confirm_dead(&self, timeout: Duration) -> Result<(), Error> {
        // A terminal state is published only after the supervisor has observed
        // the child exit. That is stronger evidence than reopening the pid file
        // afterwards, which races process-handle teardown on Windows.
        if self.state_rx.borrow().state.is_terminal() {
            return Ok(());
        }

        self.shared.user_stop.store(true, Ordering::SeqCst);
        self.shared.publish_state(InstanceState::Stopping);
        self.shared.cancel.cancel();
        let supervisor = self.shared.supervisor.lock().await.take();
        let stop_result = match supervisor {
            Some(supervisor) => match tokio::time::timeout(timeout, supervisor.stop()).await {
                Ok(Ok(())) | Ok(Err(ProcessError::AlreadyExited)) => Ok(()),
                Ok(Err(error)) => Err(format!("supervisor stop failed: {error}")),
                Err(_) => Err(format!("supervisor stop exceeded {timeout:?}")),
            },
            None => Ok(()),
        };

        let mut monitor_confirmed = false;
        if let Some(mut monitor) = self.shared.monitor.lock().await.take() {
            match tokio::time::timeout(timeout, &mut monitor).await {
                Ok(_) => monitor_confirmed = true,
                Err(_) => {
                    monitor.abort();
                    let _ = monitor.await;
                }
            }
        }
        let terminal = self.state_rx.borrow().state.is_terminal();
        if stop_result.is_ok() && (monitor_confirmed || terminal) {
            if !terminal {
                self.shared
                    .publish_state(InstanceState::Stopped(StopReason::User));
            }
            return Ok(());
        }

        let stop_error = stop_result
            .err()
            .unwrap_or_else(|| "instance monitor did not confirm termination".to_owned());
        let Some(pid_file) = self.spec.pid_file.as_ref() else {
            return Err(Error::StopUnconfirmed(stop_error));
        };
        let runtime_dir = self.spec.config_path.parent().ok_or_else(|| {
            Error::StopUnconfirmed("runtime config has no parent directory".into())
        })?;
        let reaped = reap_epoch_pid_file(pid_file.as_std_path(), runtime_dir.as_std_path())
            .await
            .map_err(|error| {
                Error::StopUnconfirmed(format!(
                    "{stop_error}; epoch identity reaper failed: {error}"
                ))
            })?;
        if matches!(
            reaped,
            OrphanReapOutcome::AlreadyExited | OrphanReapOutcome::Killed
        ) || (matches!(reaped, OrphanReapOutcome::NotFound) && terminal)
        {
            if !terminal {
                self.shared
                    .publish_state(InstanceState::Stopped(StopReason::User));
            }
            return Ok(());
        }
        Err(Error::StopUnconfirmed(stop_error))
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

    pub(crate) fn log_sender(mut self, sender: broadcast::Sender<Arc<LogFrame>>) -> Self {
        self.log_tx = Some(sender);
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
    shared.flush_log_record();
    if let Some(driver) = driver {
        driver.stop().await;
    }
}

impl Shared {
    fn parse(&self, stream: LogStream, line: String) -> ParsedFrames {
        self.parser
            .lock()
            .expect("core log parser poisoned")
            .push(stream, line)
    }

    fn finish_log_record(&self) -> ParsedFrames {
        self.parser
            .lock()
            .expect("core log parser poisoned")
            .finish()
    }

    fn publish_log_frame(&self, frame: LogFrame) {
        let frame = Arc::new(frame);
        let mut tail = self.log_tail.lock().expect("core log tail poisoned");
        if tail.len() == LOG_TAIL_FRAMES {
            tail.pop_front();
        }
        tail.push_back(Arc::clone(&frame));
        drop(tail);
        let _ = self.log_tx.send(frame);
    }

    fn flush_log_record(&self) {
        for frame in self.finish_log_record().into_iter().flatten() {
            self.publish_log_frame(frame);
        }
    }

    fn diagnostic_frames(&self) -> Vec<Arc<LogFrame>> {
        self.flush_log_record();
        self.log_tail
            .lock()
            .expect("core log tail poisoned")
            .iter()
            .cloned()
            .collect()
    }

    fn diagnostics(&self) -> String {
        format_tail(&self.diagnostic_frames())
    }

    fn failure_summary(&self, fallback: &str) -> String {
        error_summary(&self.diagnostic_frames()).unwrap_or_else(|| {
            let tail = self.diagnostics();
            if tail.is_empty() {
                fallback.to_owned()
            } else {
                tail
            }
        })
    }

    fn publish_state(&self, state: InstanceState) {
        self.state_tx
            .send_modify(|status| status.state = state.clone());
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
