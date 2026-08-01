use crate::api::R;
use serde::{Deserialize, Serialize};
use std::{borrow::Cow, path::PathBuf};

pub const STATUS_ENDPOINT: &str = "/status";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "specta", derive(specta::Type))]
pub enum CoreState {
    Running,
    Stopped(Option<String>),
}

impl Default for CoreState {
    fn default() -> Self {
        Self::Stopped(None)
    }
}

/// The core control endpoint resolved by the service. Credentials are never
/// carried in this wire type.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "specta", derive(specta::Type))]
pub enum CoreControllerInfo {
    NamedPipe(PathBuf),
    UnixSocket(PathBuf),
    Http(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "specta", derive(specta::Type))]
pub enum CoreHealthState {
    Starting,
    Healthy,
    Unhealthy,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "specta", derive(specta::Type))]
pub struct CoreHealthInfo {
    pub state: CoreHealthState,
    pub changed_at: i64,
    pub consecutive_failures: u32,
    pub last_error: Option<String>,
    pub last_success_at: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "specta", derive(specta::Type))]
pub struct ConfigRevisionInfo {
    pub epoch: u64,
    pub generation: u64,
    pub source_hash: String,
    pub effective_hash: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "specta", derive(specta::Type))]
pub struct RevisionIdInfo {
    pub epoch: u64,
    pub generation: u64,
    pub effective_hash: String,
}

impl ConfigRevisionInfo {
    pub fn id(&self) -> RevisionIdInfo {
        RevisionIdInfo {
            epoch: self.epoch,
            generation: self.generation,
            effective_hash: self.effective_hash.clone(),
        }
    }
}

/// Faithful lifecycle state. `CoreState` remains the compatibility projection.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "specta", derive(specta::Type))]
pub enum CoreStateDetail {
    Stopped { reason: Option<String> },
    Starting { epoch: u64 },
    Running { epoch: u64, pid: u32 },
    Restarting { epoch: u64, attempt: u32 },
    Switching { from: Option<u64>, to: u64 },
    Stopping { epoch: u64 },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "specta", derive(specta::Type))]
pub struct CoreInfos {
    pub r#type: Option<chimera_utils::core::CoreType>,
    pub state: CoreState,
    pub state_changed_at: i64,
    pub config_path: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub controller: Option<CoreControllerInfo>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub health: Option<CoreHealthInfo>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revision: Option<ConfigRevisionInfo>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<CoreStateDetail>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "specta", derive(specta::Type))]
pub struct RuntimeInfos<'a> {
    pub service_data_dir: Cow<'a, PathBuf>,
    pub service_config_dir: Cow<'a, PathBuf>,
    pub nyanpasu_config_dir: Cow<'a, PathBuf>,
    pub nyanpasu_data_dir: Cow<'a, PathBuf>,
}

// TODO: more health check fields
#[derive(Debug, Serialize, Deserialize, Clone)]
#[cfg_attr(feature = "specta", derive(specta::Type))]
pub struct StatusResBody<'a> {
    pub version: Cow<'a, str>,
    pub core_infos: CoreInfos,
    pub runtime_infos: RuntimeInfos<'a>,
}

pub type StatusRes<'a> = R<'a, StatusResBody<'a>>;
