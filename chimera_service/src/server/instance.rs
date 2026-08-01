use std::{
    borrow::Cow,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicI64, AtomicU64, Ordering},
    },
};
use camino::{Utf8Path, Utf8PathBuf};
use chimera_ipc::{
    api::{
        core::apply::{ApplyOutcomeKind, CoreApplyData},
        error_kind,
        status::{ConfigRevisionInfo, CoreState, CoreStateDetail, RevisionIdInfo},
    },
    utils::get_current_ts,
};
use chimera_utils::core::{
    ClashCoreType, CommandEvent, CoreType,
    instance::{CoreInstance, CoreInstanceBuilder, MIHOMO_SAFE_PATHS_ENV_NAME},
};
use nyanpasu_core_metadata::ClashCoreResourceVariant;
use tokio::{
    process::Command,
    spawn,
    sync::{Mutex, Semaphore, mpsc::Sender as MpscSender},
};
use tokio_util::{sync::CancellationToken, task::task_tracker::TaskTracker};
use tracing::instrument;

use super::consts;

struct CoreManager {
    instance: Arc<CoreInstance>,
    cancel_token: CancellationToken,
    config_path: Utf8PathBuf,
    tracker: Option<TaskTracker>,
    revision: ConfigRevisionInfo,
}

const SIGKILL: i32 = 9;
const SIGTERM: i32 = 15;
const MAX_CONCURRENT_CHECKS: usize = 2;
const CONFIG_CHECK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);
#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x08000000;

#[derive(Debug, thiserror::Error)]
pub enum CoreOperationError {
    #[error("core have not been started yet")]
    NotStarted,
    #[error("the running config revision has changed")]
    RevisionConflict,
    #[error("config file is unavailable: {0}")]
    ConfigNotFound(#[source] anyhow::Error),
    #[error("core binary is unavailable: {0}")]
    BinaryNotFound(#[source] anyhow::Error),
    #[error("core rejected the config: {0}")]
    ConfigCheckFailed(#[source] anyhow::Error),
    #[error("at most {MAX_CONCURRENT_CHECKS} config checks may run at once; retry")]
    CheckBusy,
    #[error("failed to apply config: {0}")]
    ApplyFailed(#[source] anyhow::Error),
}

impl CoreOperationError {
    pub fn kind(&self) -> Option<&'static str> {
        match self {
            Self::NotStarted => Some(error_kind::NOT_STARTED),
            Self::RevisionConflict => Some(error_kind::REVISION_CONFLICT),
            Self::ConfigNotFound(_) => Some(error_kind::CONFIG_NOT_FOUND),
            Self::BinaryNotFound(_) => Some(error_kind::BINARY_NOT_FOUND),
            Self::ConfigCheckFailed(_) => Some(error_kind::CONFIG_CHECK_FAILED),
            Self::ApplyFailed(_) => Some(error_kind::APPLY_FAILED),
            Self::CheckBusy => None,
        }
    }
}

#[derive(Clone)]
pub struct CoreManagerService {
    manager: Arc<Mutex<Option<CoreManager>>>,
    state_changed_at: Arc<AtomicI64>,
    state_changed_notify: Arc<Option<MpscSender<CoreState>>>,
    cancel_token: CancellationToken,
    next_epoch: Arc<AtomicU64>,
    check_slots: Arc<Semaphore>,
}

impl CoreManagerService {
    pub fn new_with_notify(notify: MpscSender<CoreState>, cancel_token: CancellationToken) -> Self {
        Self {
            manager: Arc::new(Mutex::new(None)),
            state_changed_at: Arc::new(AtomicI64::new(0)),
            state_changed_notify: Arc::new(Some(notify)),
            cancel_token,
            next_epoch: Arc::new(AtomicU64::new(1)),
            check_slots: Arc::new(Semaphore::new(MAX_CONCURRENT_CHECKS)),
        }
    }

    /// Get the status of the core instance
    pub async fn status(&self) -> chimera_ipc::api::status::CoreInfos {
        let manager = self.manager.lock().await;
        let state_changed_at = self
            .state_changed_at
            .load(std::sync::atomic::Ordering::Relaxed);
        let state = Self::state_(manager.as_ref()).into_owned();
        let detail = match &state {
            CoreState::Stopped(reason) => Some(CoreStateDetail::Stopped {
                reason: reason.clone(),
            }),
            CoreState::Running => None,
        };
        match *manager {
            Some(ref manager) => chimera_ipc::api::status::CoreInfos {
                r#type: Some(manager.instance.core_type.clone()),
                state,
                state_changed_at,
                config_path: Some(manager.config_path.clone().into()),
                controller: None,
                health: None,
                revision: Some(manager.revision.clone()),
                detail,
            },
            None => chimera_ipc::api::status::CoreInfos {
                r#type: None,
                state,
                state_changed_at,
                config_path: None,
                controller: None,
                health: None,
                revision: None,
                detail,
            },
        }
    }

    fn state_(manager: Option<&CoreManager>) -> Cow<'static, CoreState> {
        match manager {
            None => Cow::Borrowed(&CoreState::Stopped(None)),
            Some(manager) => Cow::Owned(match manager.instance.state() {
                chimera_utils::core::instance::CoreInstanceState::Running => CoreState::Running,
                chimera_utils::core::instance::CoreInstanceState::Stopped => {
                    CoreState::Stopped(None)
                }
            }),
        }
    }

    fn notify_state_changed(tx: Arc<Option<MpscSender<CoreState>>>, state: CoreState) {
        tokio::spawn(async move {
            if let Some(notify) = tx.as_ref() {
                let _ = notify.send(state).await;
            }
        });
    }

    #[allow(clippy::manual_async_fn)]
    fn recover_core(self, counter: usize) -> impl Future<Output = ()> + Send + Sync + 'static {
        async move {
            tracing::info!("Try to recover the core instance");
            if let Err(e) = self.restart().await {
                tracing::error!("Failed to recover the core instance: {}", e);
                tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                if counter < 5 {
                    Box::pin(self.recover_core(counter + 1)).await;
                } else {
                    tracing::error!("Failed to recover the core instance after 5 times");
                }
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn handle_command_event(
        break_loop: &mut bool,
        err_buf: &mut Vec<String>,
        state_changed_at: &AtomicI64,
        state_changed_notify: &Arc<Option<MpscSender<CoreState>>>,
        tx: &MpscSender<anyhow::Result<()>>,
        cancel_token: &CancellationToken,
        service_manager: CoreManagerService,
        event: CommandEvent,
    ) {
        match event {
            CommandEvent::Stdout(line) => {
                tracing::info!("{}", line);
            }
            CommandEvent::Stderr(line) => {
                tracing::error!("{}", line);
                err_buf.push(line);
            }
            CommandEvent::Error(e) => {
                tracing::error!("{}", e);
                let err = anyhow::anyhow!(format!("{}\n{}", e, err_buf.join("\n")));
                let _ = tx.send(Err(err)).await;
                Self::notify_state_changed(state_changed_notify.clone(), CoreState::Stopped(None));
                state_changed_at.store(get_current_ts(), Ordering::Relaxed);
                *break_loop = true;
            }
            CommandEvent::Terminated(status) => {
                tracing::info!("core terminated with status: {:?}", status);
                state_changed_at.store(get_current_ts(), Ordering::Relaxed);
                if status.code != Some(0) || !matches!(status.signal, Some(SIGKILL) | Some(SIGTERM))
                {
                    let err = anyhow::anyhow!(format!(
                        "core terminated with status: {:?}\n{}",
                        status,
                        err_buf.join("\n")
                    ));
                    tracing::error!("{}\n{}", err, err_buf.join("\n"));
                    Self::notify_state_changed(
                        state_changed_notify.clone(),
                        CoreState::Stopped(None),
                    );
                    if tx.send(Err(err)).await.is_err() && !cancel_token.is_cancelled() {
                        tokio::spawn(async move {
                            service_manager.recover_core(0).await;
                        });
                    }
                }
                *break_loop = true;
            }
            CommandEvent::DelayCheckpointPass => {
                tracing::debug!("delay checkpoint pass");
                state_changed_at.store(get_current_ts(), Ordering::Relaxed);
                tx.send(Ok(())).await.unwrap();
            }
        }
    }

    #[instrument(skip(self))]
    pub async fn start(
        &self,
        core_type: &CoreType,
        config_path: &Utf8Path,
    ) -> Result<(), anyhow::Error> {
        let mut manager = self.manager.lock().await;
        let state = Self::state_(manager.as_ref());
        if matches!(state.as_ref(), CoreState::Running) {
            anyhow::bail!("core is already running");
        }

        // check config_path
        let config_path = config_path.canonicalize_utf8()?;
        let config_path =
            Utf8PathBuf::from_path_buf(dunce::simplified(config_path.as_std_path()).to_path_buf())
                .unwrap();
        tokio::fs::metadata(&config_path).await?; // check if the file exists
        let config_bytes = tokio::fs::read(&config_path).await?;
        let config_hash = fnv1a_hex(&config_bytes);
        let revision = ConfigRevisionInfo {
            epoch: self.next_epoch.fetch_add(1, Ordering::Relaxed),
            generation: 1,
            source_hash: config_hash.clone(),
            effective_hash: config_hash,
        };
        let infos = consts::RuntimeInfos::global();
        let app_dir = infos.nyanpasu_data_dir.clone();
        let binary_path = find_binary_path(core_type)?;
        let pid_path = crate::utils::dirs::service_core_pid_file();
        let app_dir = Utf8PathBuf::from_path_buf(app_dir)
            .map_err(|_| anyhow::anyhow!("failed to convert app_dir to Utf8PathBuf"))?;
        let binary_path = Utf8PathBuf::from_path_buf(binary_path)
            .map_err(|_| anyhow::anyhow!("failed to convert binary_path to Utf8PathBuf"))?;
        let pid_path = Utf8PathBuf::from_path_buf(pid_path)
            .map_err(|_| anyhow::anyhow!("failed to convert pid_path to Utf8PathBuf"))?;
        tracing::info!(
            core_type = ?core_type,
            app_dir = %app_dir,
            binary_path = %binary_path,
            pid_path = %pid_path,
            config_path = %config_path,
            "Starting Core"
        );
        let cancel_token = self.cancel_token.child_token();
        let instance = CoreInstanceBuilder::default()
            .core_type(core_type.clone())
            .app_dir(app_dir)
            .binary_path(binary_path)
            .config_path(config_path.clone())
            .pid_path(pid_path)
            .build()?;
        let instance = Arc::new(instance);

        // start the core instance
        let state_changed_at = self.state_changed_at.clone();
        let cancel_token_clone = cancel_token.clone();
        let (tx, mut rx) = tokio::sync::mpsc::channel::<anyhow::Result<()>>(1); // use mpsc channel just to avoid type moved error, though it never fails
        let service = self.clone();
        let state_changed_notify = self.state_changed_notify.clone();
        let instance_clone = instance.clone();
        let tracker = TaskTracker::new();
        tracker.spawn(async move {
            match instance_clone.run().await {
                Ok((_, mut rx)) => {
                    let mut err_buf: Vec<String> = Vec::with_capacity(6);
                    let mut break_loop = false;

                    while let Some(event) = rx.recv().await {
                        Self::handle_command_event(
                            &mut break_loop,
                            &mut err_buf,
                            &state_changed_at,
                            &state_changed_notify,
                            &tx,
                            &cancel_token_clone,
                            service.clone(),
                            event,
                        )
                        .await;
                        if break_loop {
                            break;
                        }
                    }
                }
                Err(err) => {
                    spawn(async move {
                        tx.send(Err(err.into())).await.unwrap();
                    });
                }
            }
        });
        // Create a task to check cancel token called
        let cancel_token_clone = cancel_token.clone();
        let service = self.clone();
        tracker.spawn(async move {
            cancel_token_clone.cancelled().await;
            if service.manager.try_lock().is_ok() {
                let _ = service.stop().await;
            }
        });
        tracker.close();
        rx.recv().await.unwrap()?;
        drop(rx);
        Self::notify_state_changed(self.state_changed_notify.clone(), CoreState::Running);
        *manager = Some(CoreManager {
            instance,
            config_path: config_path.to_path_buf(),
            cancel_token,
            tracker: Some(tracker),
        });
        Ok(())
    }

    pub async fn restart(&self) -> Result<(), anyhow::Error> {
        let mut manager_guard = self.manager.lock().await;
        let manager = manager_guard.take();
        match manager {
            None => anyhow::bail!("core have not been started yet"),
            Some(manager) => {
                let state = Self::state_(Some(&manager));
                if matches!(state.as_ref(), CoreState::Running) {
                    self.stop().await?;
                }
                drop(manager_guard);
                self.start(&manager.instance.core_type, manager.config_path.as_path())
                    .await
            }
        }
    }

    pub async fn stop(&self) -> Result<(), anyhow::Error> {
        let mut manager = self.manager.lock().await;
        let state = Self::state_(manager.as_ref());
        if matches!(state.as_ref(), CoreState::Stopped(_)) {
            anyhow::bail!("core is already stopped");
        }

        if let Some(manager) = manager.as_mut() {
            manager.cancel_token.cancel();
            manager.instance.kill().await?;
            if let Some(tracker) = manager.tracker.take() {
                tracker.wait().await;
            }
        }

        Self::notify_state_changed(self.state_changed_notify.clone(), CoreState::Stopped(None));
        Ok(())
    }
}

// TODO: support system path search via a config or flag
/// Search the binary path of the core: Data Dir -> Sidecar Dir
pub fn find_binary_path(core_type: &CoreType) -> std::io::Result<PathBuf> {
    let infos = consts::RuntimeInfos::global();
    let binary_name = resource_variant(core_type)
        .map(|variant| variant.binary_name())
        .unwrap_or_else(|| core_type.get_executable_name());
    let data_dir = &infos.nyanpasu_data_dir;
    let binary_path = data_dir.join(binary_name);
    if binary_path.exists() {
        return Ok(binary_path);
    }
    let app_dir = &infos.nyanpasu_app_dir;
    let binary_path = app_dir.join(binary_name);
    if binary_path.exists() {
        return Ok(binary_path);
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::NotFound,
        format!("{binary_name} not found"),
    ))
}

fn resource_variant(core_type: &CoreType) -> Option<ClashCoreResourceVariant> {
    match core_type {
        CoreType::Clash(ClashCoreType::Mihomo) => Some(ClashCoreResourceVariant::Mihomo),
        CoreType::Clash(ClashCoreType::MihomoAlpha) => {
            Some(ClashCoreResourceVariant::MihomoAlpha)
        }
        CoreType::Clash(ClashCoreType::ClashRust) => Some(ClashCoreResourceVariant::ClashRust),
        CoreType::Clash(ClashCoreType::ClashRustAlpha) => {
            Some(ClashCoreResourceVariant::ClashRustAlpha)
        }
        CoreType::Clash(ClashCoreType::ClashPremium) => {
            Some(ClashCoreResourceVariant::ClashPremium)
        }
        CoreType::Clash(ClashCoreType::ChimeraClient) | CoreType::SingBox => None,
    }
}

async fn run_config_check(
    core_type: &CoreType,
    config_path: &Utf8Path,
    binary_path: &Utf8Path,
    app_dir: &Utf8Path,
) -> Result<(), CoreOperationError> {
    let config_dir = config_path.parent().ok_or_else(|| {
        CoreOperationError::ConfigCheckFailed(anyhow::anyhow!(
            "config path has no parent directory"
        ))
    })?;
    let mut command = Command::new(binary_path.as_std_path());
    command
        .arg("-t")
        .arg("-d")
        .arg(app_dir.as_std_path())
        .arg("-f")
        .arg(config_path.as_std_path())
        .env(
            MIHOMO_SAFE_PATHS_ENV_NAME,
            CoreInstance::get_mihomo_safe_paths(app_dir, config_dir, None),
        )
        .kill_on_drop(true);
    #[cfg(windows)]
    command.creation_flags(CREATE_NO_WINDOW);

    let output = tokio::time::timeout(CONFIG_CHECK_TIMEOUT, command.output())
        .await
        .map_err(|_| {
            CoreOperationError::ConfigCheckFailed(anyhow::anyhow!(
                "config check timed out after {} seconds",
                CONFIG_CHECK_TIMEOUT.as_secs()
            ))
        })?
        .map_err(|error| CoreOperationError::ConfigCheckFailed(error.into()))?;

    if output.status.success() {
        return Ok(());
    }
    let diagnostic = if !matches!(core_type, CoreType::Clash(ClashCoreType::ClashRust)) {
        chimera_utils::core::utils::parse_check_output(
            String::from_utf8_lossy(&output.stdout).into_owned(),
        )
    } else {
        format!(
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
    };
    Err(CoreOperationError::ConfigCheckFailed(anyhow::anyhow!(
        diagnostic
    )))
}

fn fnv1a_hex(bytes: &[u8]) -> String {
    let mut hash = 0xcbf29ce484222325u64;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("{hash:016x}")
}
