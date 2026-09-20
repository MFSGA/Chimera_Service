//! Additive v2 core-control wire for Chimera Service.
//!
//! The legacy /core/start|stop|restart routes remain available. This protocol
//! adds durable operation admission/query semantics without pretending the
//! current daemon already has ref's revision/digest model.

use std::{borrow::Cow, path::PathBuf};

use serde::{Deserialize, Serialize};

use crate::api::R;

pub const CORE_V2_SUBMIT_ENDPOINT: &str = "/v2/core/submit";
pub const CORE_V2_OPERATION_ENDPOINT: &str = "/v2/core/operation";
pub const CORE_V2_STATUS_ENDPOINT: &str = "/v2/core/status";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "specta", derive(specta::Type))]
pub struct CoreSubmitReq<'a> {
    pub operation_id: Cow<'a, str>,
    pub command: CoreCommandInfo<'a>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "specta", derive(specta::Type))]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum CoreCommandInfo<'a> {
    /// Compatibility reconcile while the daemon still consumes a promoted
    /// config path instead of config bytes.
    Reconcile {
        core_type: Cow<'a, chimera_utils::core::CoreType>,
        config_file: Cow<'a, PathBuf>,
    },
    Stop,
}

impl CoreCommandInfo<'_> {
    pub fn into_owned(self) -> CoreCommandInfo<'static> {
        match self {
            Self::Reconcile {
                core_type,
                config_file,
            } => CoreCommandInfo::Reconcile {
                core_type: Cow::Owned(core_type.into_owned()),
                config_file: Cow::Owned(config_file.into_owned()),
            },
            Self::Stop => CoreCommandInfo::Stop,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "specta", derive(specta::Type))]
pub struct CoreOperationReq<'a> {
    pub operation_id: Cow<'a, str>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wait_ms: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "specta", derive(specta::Type))]
#[serde(rename_all = "snake_case")]
pub enum OperationPhase {
    Queued,
    Running,
    Succeeded,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "specta", derive(specta::Type))]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum OperationOutputInfo {
    Reconciled,
    Stopped,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "specta", derive(specta::Type))]
pub struct OperationErrorInfo {
    pub message: String,
    pub retryable: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "specta", derive(specta::Type))]
pub struct OperationInfo {
    pub id: String,
    pub phase: OperationPhase,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<OperationOutputInfo>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<OperationErrorInfo>,
}

impl OperationInfo {
    pub fn queued(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            phase: OperationPhase::Queued,
            output: None,
            error: None,
        }
    }

    pub fn running(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            phase: OperationPhase::Running,
            output: None,
            error: None,
        }
    }

    pub fn succeeded(id: impl Into<String>, output: OperationOutputInfo) -> Self {
        Self {
            id: id.into(),
            phase: OperationPhase::Succeeded,
            output: Some(output),
            error: None,
        }
    }

    pub fn failed(id: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            phase: OperationPhase::Failed,
            output: None,
            error: Some(OperationErrorInfo {
                message: message.into(),
                retryable: false,
            }),
        }
    }

    pub fn is_terminal(&self) -> bool {
        matches!(
            self.phase,
            OperationPhase::Succeeded | OperationPhase::Failed
        )
    }
}

pub type CoreSubmitRes<'a> = R<'a, OperationInfo>;
pub type CoreOperationRes<'a> = R<'a, OperationInfo>;
pub type CoreStatusRes<'a> = R<'a, crate::api::status::CoreInfos>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn submit_and_operation_shapes_roundtrip() {
        let request = CoreSubmitReq {
            operation_id: Cow::Borrowed("00112233445566778899aabbccddeeff"),
            command: CoreCommandInfo::Stop,
        };
        let encoded = serde_json::to_string(&request).unwrap();
        assert!(encoded.contains("\"type\":\"stop\""));

        let query = CoreOperationReq {
            operation_id: Cow::Borrowed("00112233445566778899aabbccddeeff"),
            wait_ms: Some(250),
        };
        let encoded = serde_json::to_string(&query).unwrap();
        assert!(encoded.contains("\"wait_ms\":250"));

        let terminal = OperationInfo::succeeded(
            "00112233445566778899aabbccddeeff",
            OperationOutputInfo::Stopped,
        );
        let encoded = serde_json::to_string(&terminal).unwrap();
        assert_eq!(
            serde_json::from_str::<OperationInfo>(&encoded).unwrap(),
            terminal
        );
    }
}
