use std::{
    borrow::Cow,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicI64, Ordering},
    },
};

use chimera_ipc::api::status::CoreState;
use tokio::{
    spawn,
    sync::{Mutex, mpsc::Sender as MpscSender},
};
use camino::{Utf8Path, Utf8PathBuf};
use nyanpasu_utils::core::instance::CoreInstance;
use tokio_util::sync::CancellationToken;

struct CoreManager {
    instance: Arc<CoreInstance>,
    cancel_token: CancellationToken,
    config_path: Utf8PathBuf,
    /* tracker: Option<TaskTracker>, */
}

#[derive(Clone)]
pub struct CoreManagerService {
    manager: Arc<Mutex<Option<CoreManager>>>,
    state_changed_at: Arc<AtomicI64>,
    state_changed_notify: Arc<Option<MpscSender<CoreState>>>,
    cancel_token: CancellationToken,
}

impl CoreManagerService {
    pub fn new_with_notify(notify: MpscSender<CoreState>, cancel_token: CancellationToken) -> Self {
        Self {
            manager: Arc::new(Mutex::new(None)),
            state_changed_at: Arc::new(AtomicI64::new(0)),
            state_changed_notify: Arc::new(Some(notify)),
            cancel_token,
        }
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
                state,
                state_changed_at,
                config_path: Some(manager.config_path.clone().into()),
            },
            None => chimera_ipc::api::status::CoreInfos {
                r#type: None,
                state,
                state_changed_at,
                config_path: None,
            },
        }
    }

    fn state_(manager: Option<&CoreManager>) -> Cow<'static, CoreState> {
        match manager {
            None => Cow::Borrowed(&CoreState::Stopped(None)),
            Some(manager) => Cow::Owned(match manager.instance.state() {
                nyanpasu_utils::core::instance::CoreInstanceState::Running => CoreState::Running,
                nyanpasu_utils::core::instance::CoreInstanceState::Stopped => {
                    CoreState::Stopped(None)
                }
            }),
        }
    }
}
