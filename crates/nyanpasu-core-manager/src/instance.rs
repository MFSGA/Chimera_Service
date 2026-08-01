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
    ControllerVersionProbe, Error, ProbeHandle, ProbePhase,
    health::{
        HealthTracker, TrackerState,
        driver::{ProbeDriver, ProbeObservation},
    },
    kind::{self, CLICOLOR_FORCE_ENV_NAME, MIHOMO_SAFE_PATHS_ENV_NAME},
    spec::{InstanceSpec, ResolvedController},
    state::{HealthState, HealthStatus, InstanceState, InstanceStatus, StopReason, now_ms},
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
        let shared = Arc::new(Shared {
            state_tx,
            user_stop: AtomicBool::new(false),
            cancel: cancel.clone(),
            supervisor: tokio::sync::Mutex::new(None),
            monitor: tokio::sync::Mutex::new(None),
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
