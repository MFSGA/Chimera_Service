use std::{
    collections::VecDeque,
    sync::{
        Arc,
        atomic::{AtomicU32, AtomicU64, Ordering},
    },
    time::Duration,
};

use tokio_util::sync::CancellationToken;

use super::{Command, ProcessError, ProcessEvent, ProcessHandle, TerminatedPayload};

type Factory = Arc<dyn Fn() -> Command + Send + Sync>;
type EventHook = Arc<dyn Fn(SupervisorEvent) + Send + Sync>;
type ProcessEventHook = Arc<dyn Fn(ProcessEvent) + Send + Sync>;

/// Controls whether and how often failed children are restarted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RestartPolicy {
    Never,
    OnFailure { max_restarts: u32 },
}

/// Bounds abnormal child exits even when readiness repeatedly resets the
/// consecutive restart attempt count.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RestartStormPolicy {
    max_failures: u32,
    window: Duration,
}

impl RestartStormPolicy {
    pub fn new(max_failures: u32, window: Duration) -> Self {
        Self {
            max_failures: max_failures.max(1),
            window: window.max(Duration::from_millis(1)),
        }
    }

    pub fn max_failures(&self) -> u32 {
        self.max_failures
    }

    pub fn window(&self) -> Duration {
        self.window
    }
}

impl Default for RestartStormPolicy {
    fn default() -> Self {
        Self::new(5, Duration::from_secs(5 * 60))
    }
}

#[derive(Debug, Clone, Copy)]
pub struct Backoff {
    initial: Duration,
    max: Duration,
    jitter: bool,
}

fn time_entropy() -> u64 {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_nanos() as u64)
        .unwrap_or(0x9E37_79B9_7F4A_7C15);
    let counter = COUNTER.fetch_add(1, Ordering::Relaxed);
    let mut value = nanos
        .wrapping_add(counter.wrapping_mul(0x9E37_79B9_7F4A_7C15))
        .wrapping_add(0x9E37_79B9_7F4A_7C15);
    value = (value ^ (value >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    value = (value ^ (value >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    value ^ (value >> 31)
}

impl Backoff {
    pub fn exponential(initial: Duration, max: Duration) -> Self {
        Self {
            initial,
            max,
            jitter: false,
        }
    }

    pub fn with_jitter(mut self) -> Self {
        self.jitter = true;
        self
    }

    pub fn delay_for(&self, attempt: u32) -> Duration {
        let base = self
            .initial
            .saturating_mul(2u32.saturating_pow(attempt.min(30)))
            .min(self.max);
        if !self.jitter {
            return base;
        }
        let base_ns = base.as_nanos().max(1) as u64;
        let span = base_ns / 2;
        let offset = time_entropy() % span.max(1);
        Duration::from_nanos(base_ns - span / 2 + offset)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadinessProbe {
    AliveAfter(Duration),
    Acknowledged,
}

#[non_exhaustive]
#[derive(Debug, Clone)]
pub enum SupervisorEvent {
    Started { pid: u32 },
    Ready,
    Exited(TerminatedPayload),
    Restarting { attempt: u32, delay: Duration },
    GaveUp,
    Stopped,
}

pub struct SupervisorBuilder {
    factory: Factory,
    policy: RestartPolicy,
    backoff: Backoff,
    readiness: ReadinessProbe,
    storm_policy: RestartStormPolicy,
    on_event: Option<EventHook>,
    on_process_event: Option<ProcessEventHook>,
    cancel_token: Option<CancellationToken>,
}

pub struct Supervisor {
    token: CancellationToken,
    current: Arc<tokio::sync::Mutex<Option<ProcessHandle>>>,
    ready_tx: tokio::sync::mpsc::UnboundedSender<u32>,
    ready_pending: Arc<AtomicU32>,
    task: Option<tokio::task::JoinHandle<()>>,
}

async fn stop_process(handle: ProcessHandle) -> Result<(), ProcessError> {
    if handle.graceful_kill().await.is_err() {
        match handle.kill().await {
            Err(ProcessError::AlreadyExited) => Ok(()),
            result => result,
        }
    } else {
        Ok(())
    }
}

impl Supervisor {
    pub fn builder<F>(factory: F) -> SupervisorBuilder
    where
        F: Fn() -> Command + Send + Sync + 'static,
    {
        SupervisorBuilder {
            factory: Arc::new(factory),
            policy: RestartPolicy::OnFailure { max_restarts: 5 },
            backoff: Backoff::exponential(Duration::from_secs(1), Duration::from_secs(30))
                .with_jitter(),
            readiness: ReadinessProbe::AliveAfter(Duration::from_millis(1500)),
            storm_policy: RestartStormPolicy::default(),
            on_event: None,
            on_process_event: None,
            cancel_token: None,
        }
    }

    pub async fn acknowledge_ready(&self, pid: u32) -> bool {
        let is_current = self
            .current
            .lock()
            .await
            .as_ref()
            .is_some_and(|handle| handle.pid() == pid && handle.terminated.borrow().is_none());
        if !is_current
            || self
                .ready_pending
                .compare_exchange(pid, 0, Ordering::SeqCst, Ordering::SeqCst)
                .is_err()
        {
            return false;
        }
        if self.ready_tx.send(pid).is_ok() {
            true
        } else {
            self.ready_pending.store(pid, Ordering::SeqCst);
            false
        }
    }

    pub async fn stop(mut self) -> Result<(), ProcessError> {
        self.token.cancel();
        let current = self.current.lock().await.take();
        let stop_result = match current {
            Some(handle) => stop_process(handle).await,
            None => Ok(()),
        };
        if let Some(task) = self.task.take() {
            task.await.map_err(|error| {
                ProcessError::Engine(format!("supervisor task failed: {error}"))
            })?;
        }
        stop_result
    }
}

impl Drop for Supervisor {
    fn drop(&mut self) {
        self.token.cancel();
    }
}

impl SupervisorBuilder {
    pub fn restart_policy(mut self, policy: RestartPolicy) -> Self {
        self.policy = policy;
        self
    }

    pub fn backoff(mut self, backoff: Backoff) -> Self {
        self.backoff = backoff;
        self
    }

    pub fn readiness(mut self, readiness: ReadinessProbe) -> Self {
        self.readiness = readiness;
        self
    }

    pub fn restart_storm_policy(mut self, policy: RestartStormPolicy) -> Self {
        self.storm_policy = policy;
        self
    }

    pub fn on_event(mut self, hook: impl Fn(SupervisorEvent) + Send + Sync + 'static) -> Self {
        self.on_event = Some(Arc::new(hook));
        self
    }

    pub fn on_process_event(
        mut self,
        hook: impl Fn(ProcessEvent) + Send + Sync + 'static,
    ) -> Self {
        self.on_process_event = Some(Arc::new(hook));
        self
    }

    pub fn cancel_token(mut self, token: CancellationToken) -> Self {
        self.cancel_token = Some(token);
        self
    }

    pub async fn spawn(self) -> Result<Supervisor, ProcessError> {
        if self
            .cancel_token
            .as_ref()
            .is_some_and(CancellationToken::is_cancelled)
        {
            return Err(ProcessError::Engine(
                "supervisor started with cancelled token".into(),
            ));
        }
        let token = self.cancel_token.unwrap_or_default().child_token();
        let current: Arc<tokio::sync::Mutex<Option<ProcessHandle>>> = Arc::default();
        let (ready_tx, mut ready_rx) = tokio::sync::mpsc::unbounded_channel();
        let ready_pending = Arc::new(AtomicU32::new(0));
        let emit = {
            let hook = self.on_event.clone();
            move |event: SupervisorEvent| {
                if let Some(hook) = &hook {
                    hook(event);
                }
            }
        };

        let (first_handle, first_rx) = (self.factory)().spawn().await?;
        let first_pid = first_handle.pid();
        if matches!(self.readiness, ReadinessProbe::Acknowledged) {
            ready_pending.store(first_pid, Ordering::SeqCst);
        }
        emit(SupervisorEvent::Started { pid: first_pid });
        *current.lock().await = Some(first_handle);

        let factory = self.factory;
        let policy = self.policy;
        let backoff = self.backoff;
        let readiness = self.readiness;
        let storm_policy = self.storm_policy;
        let on_process_event = self.on_process_event;
        let token_for_task = token.clone();
        let current_for_task = current.clone();
        let pending_for_task = ready_pending.clone();

        let task = tokio::spawn(async move {
            let mut attempt = 0u32;
            let mut next_process = Some((first_pid, first_rx));
            let mut abnormal_exits = VecDeque::new();

            loop {
                let (pid, mut events) = next_process.take().expect("active process events");
                let ready_at = match readiness {
                    ReadinessProbe::AliveAfter(delay) => {
                        Some(tokio::time::Instant::now() + delay)
                    }
                    ReadinessProbe::Acknowledged => None,
                };
                let mut readiness_pending = true;

                let payload = loop {
                    tokio::select! {
                        biased;
                        _ = token_for_task.cancelled() => {
                            if let Some(handle) = current_for_task.lock().await.take() {
                                let _ = stop_process(handle).await;
                            }
                            emit(SupervisorEvent::Stopped);
                            return;
                        }
                        _ = sleep_until(ready_at), if readiness_pending && ready_at.is_some() => {
                            readiness_pending = false;
                            attempt = 0;
                            emit(SupervisorEvent::Ready);
                        }
                        maybe_pid = ready_rx.recv(), if readiness_pending && ready_at.is_none() => {
                            match maybe_pid {
                                Some(ready_pid) if ready_pid == pid => {
                                    readiness_pending = false;
                                    attempt = 0;
                                    emit(SupervisorEvent::Ready);
                                }
                                Some(_) => continue,
                                None => {
                                    emit(SupervisorEvent::GaveUp);
                                    return;
                                }
                            }
                        }
                        event = events.recv() => match event {
                            Some(ProcessEvent::Terminated(payload)) => {
                                if let Some(hook) = &on_process_event {
                                    hook(ProcessEvent::Terminated(payload.clone()));
                                }
                                break payload;
                            }
                            Some(event) => {
                                if let Some(hook) = &on_process_event {
                                    hook(event);
                                }
                            }
                            None => {
                                let handle = current_for_task.lock().await.clone();
                                let Some(handle) = handle else {
                                    emit(SupervisorEvent::Stopped);
                                    return;
                                };
                                match handle.wait().await {
                                    Ok(payload) => break payload,
                                    Err(error) => {
                                        if let Some(hook) = &on_process_event {
                                            hook(ProcessEvent::Error(error.to_string()));
                                        }
                                        break TerminatedPayload { code: None, signal: None };
                                    }
                                }
                            }
                        }
                    }
                };

                current_for_task.lock().await.take();
                pending_for_task.store(0, Ordering::SeqCst);
                emit(SupervisorEvent::Exited(payload.clone()));
                if payload.code == Some(0) {
                    emit(SupervisorEvent::Stopped);
                    return;
                }

                let now = tokio::time::Instant::now();
                abnormal_exits.push_back(now);
                while abnormal_exits
                    .front()
                    .is_some_and(|seen| now.duration_since(*seen) > storm_policy.window())
                {
                    abnormal_exits.pop_front();
                }
                if abnormal_exits.len() as u32 >= storm_policy.max_failures() {
                    emit(SupervisorEvent::GaveUp);
                    return;
                }

                let max_restarts = match policy {
                    RestartPolicy::Never => 0,
                    RestartPolicy::OnFailure { max_restarts } => max_restarts,
                };
                if attempt >= max_restarts {
                    emit(SupervisorEvent::GaveUp);
                    return;
                }

                loop {
                    let delay = backoff.delay_for(attempt);
                    attempt = attempt.saturating_add(1);
                    emit(SupervisorEvent::Restarting { attempt, delay });
                    tokio::select! {
                        _ = token_for_task.cancelled() => {
                            emit(SupervisorEvent::Stopped);
                            return;
                        }
                        _ = tokio::time::sleep(delay) => {}
                    }

                    match (factory)().spawn().await {
                        Ok((handle, events)) => {
                            let pid = handle.pid();
                            if matches!(readiness, ReadinessProbe::Acknowledged) {
                                pending_for_task.store(pid, Ordering::SeqCst);
                            }
                            emit(SupervisorEvent::Started { pid });
                            *current_for_task.lock().await = Some(handle);
                            next_process = Some((pid, events));
                            break;
                        }
                        Err(error) => {
                            if let Some(hook) = &on_process_event {
                                hook(ProcessEvent::Error(error.to_string()));
                            }
                            if attempt >= max_restarts {
                                emit(SupervisorEvent::GaveUp);
                                return;
                            }
                        }
                    }
                }
            }
        });

        Ok(Supervisor {
            token,
            current,
            ready_tx,
            ready_pending,
            task: Some(task),
        })
    }
}

async fn sleep_until(deadline: Option<tokio::time::Instant>) {
    match deadline {
        Some(deadline) => tokio::time::sleep_until(deadline).await,
        None => std::future::pending().await,
    }
}
