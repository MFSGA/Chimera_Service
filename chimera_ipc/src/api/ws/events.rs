use std::sync::Arc;

use serde::{Deserialize, Serialize};

pub use nyanpasu_core_metadata::{
    ClashCoreKind, LogField, LogFrame, LogLevel, LogStream, LogTimestamp,
};

use crate::api::status::{CoreInfos, CoreState};

pub const EVENT_URI: &str = "/ws/events";

#[derive(Debug, Serialize, Deserialize, Clone)]
pub enum Event {
    CoreStateChanged(CoreState),
    CoreStatusChanged(CoreInfos),
    CoreLog(Arc<LogFrame>),
}

impl Event {
    pub fn new_core_state_changed(state: CoreState) -> Self {
        Self::CoreStateChanged(state)
    }

    pub fn new_core_status_changed(infos: CoreInfos) -> Self {
        Self::CoreStatusChanged(infos)
    }

    pub fn new_core_log(frame: Arc<LogFrame>) -> Self {
        Self::CoreLog(frame)
    }
}
