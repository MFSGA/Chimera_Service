use crate::api::{
    R,
    status::{ConfigRevisionInfo, RevisionIdInfo},
};
use serde::{Deserialize, Serialize};
use std::{borrow::Cow, path::PathBuf};

pub const CORE_APPLY_ENDPOINT: &str = "/core/apply";

/// Apply a config to an already-running core.
#[derive(Debug, Serialize, Deserialize, Clone)]
#[cfg_attr(feature = "specta", derive(specta::Type))]
pub struct CoreApplyReq<'n> {
    pub core_type: Cow<'n, chimera_utils::core::CoreType>,
    pub config_file: Cow<'n, PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_revision: Option<RevisionIdInfo>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "specta", derive(specta::Type))]
#[serde(rename_all = "snake_case")]
pub enum ApplyOutcomeKind {
    Noop,
    Patched,
    Reloaded,
    Restarted,
    Switched,
    RolledBack,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "specta", derive(specta::Type))]
pub struct CoreApplyData {
    pub outcome: ApplyOutcomeKind,
    pub revision: ConfigRevisionInfo,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub warning: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failed_apply: Option<String>,
}

pub type CoreApplyRes<'a> = R<'a, CoreApplyData>;
