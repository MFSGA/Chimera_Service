use std::{
    borrow::Cow,
    collections::{HashMap, VecDeque},
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicI64, AtomicU64, Ordering},
    },
};

use camino::{Utf8Path, Utf8PathBuf};
use chimera_ipc::{
    api::{
        core::v2::{
            CoreCommandInfo, CoreOperationReq, CoreSubmitReq, OperationInfo, OperationOutputInfo,
            ReconcileOutcomeInfo, ReconcileOutcomeKind, payload_digest,
        },
        status::{ConfigRevisionInfo, CoreState},
    },
    utils::get_current_ts,
};
use chimera_utils::core::{
    CommandEvent, CoreType,
    instance::{CoreInstance, CoreInstanceBuilder},
};
use tokio::{
    spawn,
    sync::{Mutex, mpsc::Sender as MpscSender, watch},
};
use tokio_util::{sync::CancellationToken, task::task_tracker::TaskTracker};
use tracing::instrument;

use super::consts;

const OPERATION_HISTORY_LIMIT: usize = 64;
const OPERATION_WAIT_LIMIT: std::time::Duration = std::time::Duration::from_secs(60);

#[derive(Debug)]
struct OperationRecord {
    fingerprint: String,
    receiver: watch::Receiver<OperationInfo>,
}

#[derive(Debug, Default)]
struct OperationRegistryState {
    records: HashMap<String, OperationRecord>,
    order: VecDeque<String>,
}

struct CoreManager {
    instance: Arc<CoreInstance>,
    cancel_token: CancellationToken,
    config_path: Utf8PathBuf,
    tracker: Option<TaskTracker>,
}

const SIGKILL: i32 = 9;
const SIGTERM: i32 = 15;

#[derive(Clone)]
pub struct CoreManagerService {
    manager: Arc<Mutex<Option<CoreManager>>>,
    state_changed_at: Arc<AtomicI64>,
    state_changed_notify: Arc<Option<MpscSender<CoreState>>>,
    cancel_token: CancellationToken,
    operations: Arc<parking_lot::Mutex<OperationRegistryState>>,
    operation_lock: Arc<Mutex<()>>,
    applied_revision: Arc<parking_lot::Mutex<Option<ConfigRevisionInfo>>>,
    next_epoch: Arc<AtomicU64>,
}

impl CoreManagerService {
    pub fn new_with_notify(notify: MpscSender<CoreState>, cancel_token: CancellationToken) -> Self {
        Self {
            manager: Arc::new(Mutex::new(None)),
            state_changed_at: Arc::new(AtomicI64::new(0)),
            state_changed_notify: Arc::new(Some(notify)),
            cancel_token,
            operations: Arc::new(parking_lot::Mutex::new(OperationRegistryState::default())),
            operation_lock: Arc::new(Mutex::new(())),
            applied_revision: Arc::new(parking_lot::Mutex::new(None)),
            next_epoch: Arc::new(AtomicU64::new(1)),
        }
    }

    fn validate_operation_id(id: &str) -> anyhow::Result<()> {
        anyhow::ensure!(
            id.len() == 32
                && id
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)),
            "operation id must be exactly 32 lowercase hexadecimal characters"
        );
        Ok(())
    }

    #[cfg(test)]
    fn operation_snapshot(&self, id: &str) -> Option<OperationInfo> {
        self.operations
            .lock()
            .records
            .get(id)
            .map(|record| record.receiver.borrow().clone())
    }

    fn insert_operation(
        &self,
        id: String,
        fingerprint: String,
        receiver: watch::Receiver<OperationInfo>,
    ) {
        let mut operations = self.operations.lock();
        operations.records.insert(
            id.clone(),
            OperationRecord {
                fingerprint,
                receiver,
            },
        );
        operations.order.push_back(id);
        while operations.records.len() > OPERATION_HISTORY_LIMIT {
            let Some(position) = operations.order.iter().position(|candidate| {
                operations
                    .records
                    .get(candidate)
                    .is_some_and(|record| record.receiver.borrow().is_terminal())
            }) else {
                break;
            };
            if let Some(evicted) = operations.order.remove(position) {
                operations.records.remove(&evicted);
            }
        }
    }

    async fn execute_v2(
        &self,
        command: CoreCommandInfo<'static>,
    ) -> anyhow::Result<OperationOutputInfo> {
        let _guard = self.operation_lock.lock().await;
        match command {
            CoreCommandInfo::Reconcile {
                core_type,
                config,
                expected_digest,
                expected_applied,
            } => {
                let computed_digest = payload_digest(config.as_bytes());
                if let Some(expected_digest) = expected_digest {
                    anyhow::ensure!(
                        expected_digest.as_ref() == computed_digest,
                        "config digest mismatch: declared {}, computed {}",
                        expected_digest,
                        computed_digest
                    );
                }

                let status = self.status().await;
                let was_running = matches!(status.state, CoreState::Running);
                let current_applied = status.revision.as_ref().map(ConfigRevisionInfo::id);
                if let Some(expected) = expected_applied {
                    anyhow::ensure!(
                        current_applied.as_ref() == Some(&expected),
                        "revision conflict: expected {:?}, applied {:?}",
                        expected,
                        current_applied
                    );
                }

                let config_dir = crate::utils::dirs::service_config_dir();
                tokio::fs::create_dir_all(&config_dir).await?;
                let config_path = config_dir.join(format!("runtime-{computed_digest}.yaml"));
                tokio::fs::write(&config_path, config.as_bytes()).await?;
                let config_path = Utf8PathBuf::from_path_buf(config_path)
                    .map_err(|_| anyhow::anyhow!("service config path is not valid UTF-8"))?;

                if was_running {
                    self.stop().await?;
                }
                self.start(&core_type, config_path.as_path()).await?;

                let revision = ConfigRevisionInfo {
                    epoch: self.next_epoch.fetch_add(1, Ordering::Relaxed),
                    generation: 1,
                    source_hash: computed_digest.clone(),
                    effective_hash: computed_digest,
                };
                *self.applied_revision.lock() = Some(revision.clone());
                Ok(OperationOutputInfo::Reconciled(ReconcileOutcomeInfo {
                    outcome: if was_running {
                        ReconcileOutcomeKind::Restarted
                    } else {
                        ReconcileOutcomeKind::Started
                    },
                    revision,
                }))
            }
            CoreCommandInfo::Stop => {
                if matches!(self.status().await.state, CoreState::Running) {
                    self.stop().await?;
                } else {
                    *self.applied_revision.lock() = None;
                }
                Ok(OperationOutputInfo::Stopped)
            }
        }
    }

    pub async fn submit_v2(&self, request: &CoreSubmitReq<'_>) -> anyhow::Result<OperationInfo> {
        let id = request.operation_id.as_ref();
        Self::validate_operation_id(id)?;
        let command = request.command.clone().into_owned();
        let fingerprint = serde_json::to_string(&command)?;

        if let Some(existing) = self.operations.lock().records.get(id) {
            anyhow::ensure!(
                existing.fingerprint == fingerprint,
                "operation conflict: id already exists with a different command"
            );
            return Ok(existing.receiver.borrow().clone());
        }

        let id = id.to_owned();
        let queued = OperationInfo::queued(id.clone());
        let (sender, receiver) = watch::channel(queued.clone());
        self.insert_operation(id.clone(), fingerprint, receiver);

        let service = self.clone();
        tokio::spawn(async move {
            sender.send_replace(OperationInfo::running(id.clone()));
            let terminal = match service.execute_v2(command).await {
                Ok(output) => OperationInfo::succeeded(id.clone(), output),
                Err(error) => OperationInfo::failed(id.clone(), error.to_string()),
            };
            sender.send_replace(terminal);
        });

        Ok(queued)
    }

    pub async fn operation_v2(
        &self,
        request: &CoreOperationReq<'_>,
    ) -> anyhow::Result<OperationInfo> {
        let id = request.operation_id.as_ref();
        Self::validate_operation_id(id)?;
        let mut receiver = self
            .operations
            .lock()
            .records
            .get(id)
            .map(|record| record.receiver.clone())
            .ok_or_else(|| anyhow::anyhow!("unknown operation id: {id}"))?;
        let current = receiver.borrow().clone();
        if current.is_terminal() || request.wait_ms.unwrap_or(0) == 0 {
            return Ok(current);
        }

        let wait = std::time::Duration::from_millis(request.wait_ms.unwrap_or(0))
            .min(OPERATION_WAIT_LIMIT);
        let _ = tokio::time::timeout(wait, async {
            loop {
                if receiver.borrow().is_terminal() {
                    break;
                }
                if receiver.changed().await.is_err() {
                    break;
                }
            }
        })
        .await;
        Ok(receiver.borrow().clone())
    }

    /// Get the status of the core instance
    pub async fn status(&self) -> chimera_ipc::api::status::CoreInfos {
        let manager = self.manager.lock().await;
        let state_changed_at = self
            .state_changed_at
            .load(std::sync::atomic::Ordering::Relaxed);
        let state = Self::state_(manager.as_ref()).into_owned();
        match *manager {
            Some(ref manager) => chimera_ipc::api::status::CoreInfos {
                r#type: Some(manager.instance.core_type.clone()),
                revision: matches!(state, CoreState::Running)
                    .then(|| self.applied_revision.lock().clone())
                    .flatten(),
                state,
                state_changed_at,
                config_path: Some(manager.config_path.clone().into()),
            },
            None => chimera_ipc::api::status::CoreInfos {
                r#type: None,
                state,
                state_changed_at,
                config_path: None,
                revision: None,
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
        *self.applied_revision.lock() = None;

        // check config_path
        let config_path = config_path.canonicalize_utf8()?;
        let config_path =
            Utf8PathBuf::from_path_buf(dunce::simplified(config_path.as_std_path()).to_path_buf())
                .unwrap();
        tokio::fs::metadata(&config_path).await?; // check if the file exists
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
        *self.applied_revision.lock() = None;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::borrow::Cow;

    use chimera_ipc::api::{
        core::v2::{
            CoreCommandInfo, CoreOperationReq, CoreSubmitReq, OperationOutputInfo, OperationPhase,
            payload_digest,
        },
        status::{CoreState, RevisionIdInfo},
    };
    use tokio_util::sync::CancellationToken;

    use super::CoreManagerService;

    const OPERATION_ID: &str = "00112233445566778899aabbccddeeff";

    fn service() -> CoreManagerService {
        let (notify, _receiver) = tokio::sync::mpsc::channel(4);
        CoreManagerService::new_with_notify(notify, CancellationToken::new())
    }

    fn stop_request() -> CoreSubmitReq<'static> {
        CoreSubmitReq {
            operation_id: Cow::Borrowed(OPERATION_ID),
            command: CoreCommandInfo::Stop,
        }
    }

    #[tokio::test]
    async fn v2_stop_is_durable_idempotent_and_queryable() {
        let service = service();
        let request = stop_request();

        let admitted = service.submit_v2(&request).await.unwrap();
        assert_eq!(admitted.id, OPERATION_ID);

        let attached = service.submit_v2(&request).await.unwrap();
        assert_eq!(attached.id, OPERATION_ID);

        let terminal = service
            .operation_v2(&CoreOperationReq {
                operation_id: Cow::Borrowed(OPERATION_ID),
                wait_ms: Some(1_000),
            })
            .await
            .unwrap();
        assert_eq!(terminal.phase, OperationPhase::Succeeded);
        assert_eq!(terminal.output, Some(OperationOutputInfo::Stopped));
        assert_eq!(service.operation_snapshot(OPERATION_ID), Some(terminal));
    }

    #[tokio::test]
    async fn v2_reconcile_digest_mismatch_fails_before_mutation() {
        let service = service();
        let request = CoreSubmitReq {
            operation_id: Cow::Borrowed(OPERATION_ID),
            command: CoreCommandInfo::Reconcile {
                core_type: Cow::Owned(chimera_utils::core::CoreType::Clash(
                    chimera_utils::core::ClashCoreType::Mihomo,
                )),
                config: Cow::Borrowed("mode: rule\n"),
                expected_digest: Some(Cow::Borrowed("0000000000000000")),
                expected_applied: None,
            },
        };

        service.submit_v2(&request).await.unwrap();
        let terminal = service
            .operation_v2(&CoreOperationReq {
                operation_id: Cow::Borrowed(OPERATION_ID),
                wait_ms: Some(1_000),
            })
            .await
            .unwrap();
        assert_eq!(terminal.phase, OperationPhase::Failed);
        assert!(
            terminal
                .error
                .as_ref()
                .is_some_and(|error| error.message.contains("config digest mismatch"))
        );
        let status = service.status().await;
        assert!(matches!(status.state, CoreState::Stopped(_)));
        assert!(status.revision.is_none());
    }

    #[tokio::test]
    async fn v2_reconcile_stale_cas_fails_before_mutation() {
        let service = service();
        let config = "mode: rule\n";
        let digest = payload_digest(config.as_bytes());
        let request = CoreSubmitReq {
            operation_id: Cow::Borrowed(OPERATION_ID),
            command: CoreCommandInfo::Reconcile {
                core_type: Cow::Owned(chimera_utils::core::CoreType::Clash(
                    chimera_utils::core::ClashCoreType::Mihomo,
                )),
                config: Cow::Borrowed(config),
                expected_digest: Some(Cow::Owned(digest)),
                expected_applied: Some(RevisionIdInfo {
                    epoch: 9,
                    generation: 1,
                    effective_hash: "deadbeefdeadbeef".to_string(),
                }),
            },
        };

        service.submit_v2(&request).await.unwrap();
        let terminal = service
            .operation_v2(&CoreOperationReq {
                operation_id: Cow::Borrowed(OPERATION_ID),
                wait_ms: Some(1_000),
            })
            .await
            .unwrap();
        assert_eq!(terminal.phase, OperationPhase::Failed);
        assert!(
            terminal
                .error
                .as_ref()
                .is_some_and(|error| error.message.contains("revision conflict"))
        );
        let status = service.status().await;
        assert!(matches!(status.state, CoreState::Stopped(_)));
        assert!(status.revision.is_none());
    }

    #[tokio::test]
    async fn v2_operation_id_validation_and_conflict_fail_closed() {
        let service = service();
        let invalid = CoreSubmitReq {
            operation_id: Cow::Borrowed("not-hex"),
            command: CoreCommandInfo::Stop,
        };
        assert!(service.submit_v2(&invalid).await.is_err());

        let request = stop_request();
        service.submit_v2(&request).await.unwrap();
        service
            .operations
            .lock()
            .records
            .get_mut(OPERATION_ID)
            .unwrap()
            .fingerprint = "different-command".to_string();

        let error = service.submit_v2(&request).await.unwrap_err();
        assert!(error.to_string().contains("operation conflict"));
    }
}

// TODO: support system path search via a config or flag
/// Search the binary path of the core: Data Dir -> Sidecar Dir
pub fn find_binary_path(core_type: &CoreType) -> std::io::Result<PathBuf> {
    let infos = consts::RuntimeInfos::global();
    let data_dir = &infos.nyanpasu_data_dir;
    let binary_path = data_dir.join(core_type.get_executable_name());
    if binary_path.exists() {
        return Ok(binary_path);
    }
    let app_dir = &infos.nyanpasu_app_dir;
    let binary_path = app_dir.join(core_type.get_executable_name());
    if binary_path.exists() {
        return Ok(binary_path);
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::NotFound,
        format!("{} not found", core_type.get_executable_name()),
    ))
}
