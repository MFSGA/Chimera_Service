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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "specta", derive(specta::Type))]
pub enum CoreHealthState {
    Starting,
    Healthy,
    Unhealthy,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "specta", derive(specta::Type))]
pub struct CoreHealthInfo {
    pub state: CoreHealthState,
    pub changed_at: i64,
    pub consecutive_failures: u32,
    pub last_error: Option<String>,
    pub last_success_at: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "specta", derive(specta::Type))]
pub struct ConfigRevisionInfo {
    pub epoch: u64,
    pub generation: u64,
    pub source_hash: String,
    pub effective_hash: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "specta", derive(specta::Type))]
pub struct CoreInfos {
    pub r#type: Option<chimera_utils::core::CoreType>,
    pub state: CoreState,
    pub state_changed_at: i64,
    pub config_path: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub health: Option<CoreHealthInfo>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revision: Option<ConfigRevisionInfo>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "specta", derive(specta::Type))]
pub struct RuntimeInfos<'a> {
    pub service_data_dir: Cow<'a, PathBuf>,
    pub service_config_dir: Cow<'a, PathBuf>,
    pub nyanpasu_config_dir: Cow<'a, PathBuf>,
    pub nyanpasu_data_dir: Cow<'a, PathBuf>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
#[cfg_attr(feature = "specta", derive(specta::Type))]
pub struct StatusResBody<'a> {
    pub version: Cow<'a, str>,
    pub core_infos: CoreInfos,
    pub runtime_infos: RuntimeInfos<'a>,
}

pub type StatusRes<'a> = R<'a, StatusResBody<'a>>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn core_health_wire_roundtrips_and_is_omitted_when_absent() {
        let health = CoreHealthInfo {
            state: CoreHealthState::Unhealthy,
            changed_at: 42,
            consecutive_failures: 3,
            last_error: Some("controller unavailable".into()),
            last_success_at: Some(7),
        };
        let info = CoreInfos {
            r#type: None,
            state: CoreState::Running,
            state_changed_at: 41,
            config_path: None,
            health: Some(health.clone()),
            revision: None,
        };
        let value = serde_json::to_value(&info).unwrap();
        assert!(value.get("health").is_some());
        let roundtrip: CoreInfos = serde_json::from_value(value).unwrap();
        assert_eq!(roundtrip.health, Some(health));

        let absent = CoreInfos {
            r#type: None,
            state: CoreState::Stopped(None),
            state_changed_at: 43,
            config_path: None,
            health: None,
            revision: None,
        };
        let value = serde_json::to_value(absent).unwrap();
        assert!(value.get("health").is_none());
    }
}
