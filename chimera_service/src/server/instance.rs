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

use nyanpasu_utils::core::instance::CoreInstance;
use tokio_util::sync::CancellationToken;

struct CoreManager {
    instance: Arc<CoreInstance>,
    cancel_token: CancellationToken,
    /* config_path: Utf8PathBuf,

    tracker: Option<TaskTracker>, */
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
}
