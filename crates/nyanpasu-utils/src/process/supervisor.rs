use std::{
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
