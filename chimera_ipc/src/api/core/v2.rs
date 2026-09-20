//! Additive v2 core-control wire for Chimera Service.
//!
//! The legacy /core/start|stop|restart routes remain available. V2 uses
//! durable operation admission/query semantics and portable reconcile input:
//! callers ship config text plus an optional digest/CAS token; the daemon
//! materializes its own private runtime file.

use std::borrow::Cow;

use serde::{Deserialize, Serialize};

use crate::api::{
    R,
    status::{ConfigRevisionInfo, RevisionIdInfo},
};

pub const CORE_V2_SUBMIT_ENDPOINT: &str = "/v2/core/submit";
pub const CORE_V2_OPERATION_ENDPOINT: &str = "/v2/core/operation";
pub const CORE_V2_STATUS_ENDPOINT: &str = "/v2/core/status";
pub const CORE_V2_API_ENDPOINT: &str = "/v2/core/api";

/// Stable FNV-1a digest used as the portable change identity.
pub fn payload_digest(bytes: &[u8]) -> String {
    let mut hash = 0xcbf29ce484222325u64;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("{hash:016x}")
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "specta", derive(specta::Type))]
#[serde(tag = "type", content = "value", rename_all = "snake_case")]
pub enum CoreControllerInfo {
    Http(String),
    UnixSocket(String),
    NamedPipe(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "specta", derive(specta::Type))]
pub struct CoreApiConnection {
    pub instance_id: String,
    pub controller: CoreControllerInfo,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub secret: Option<String>,
}

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
    Reconcile {
        core_type: Cow<'a, chimera_utils::core::CoreType>,
        /// Full config document; never a caller filesystem path.
        config: Cow<'a, str>,
        /// Digest of `config` as computed by the caller.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        expected_digest: Option<Cow<'a, str>>,
        /// Compare-and-swap token for the revision the caller believes is
        /// currently applied. None means unconditional reconcile.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        expected_applied: Option<RevisionIdInfo>,
    },
    Stop,
    Recover,
}

impl CoreCommandInfo<'_> {
    pub fn into_owned(self) -> CoreCommandInfo<'static> {
        match self {
            Self::Reconcile {
                core_type,
                config,
                expected_digest,
                expected_applied,
            } => CoreCommandInfo::Reconcile {
                core_type: Cow::Owned(core_type.into_owned()),
                config: Cow::Owned(config.into_owned()),
                expected_digest: expected_digest.map(|digest| Cow::Owned(digest.into_owned())),
                expected_applied,
            },
            Self::Stop => CoreCommandInfo::Stop,
            Self::Recover => CoreCommandInfo::Recover,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "specta", derive(specta::Type))]
#[serde(rename_all = "snake_case")]
pub enum ReconcileOutcomeKind {
    Started,
    Noop,
    Patched,
    Reloaded,
    Restarted,
    Switched,
    RolledBack,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "specta", derive(specta::Type))]
pub struct ReconcileOutcomeInfo {
    pub outcome: ReconcileOutcomeKind,
    pub revision: ConfigRevisionInfo,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "specta", derive(specta::Type))]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum OperationOutputInfo {
    Reconciled(ReconcileOutcomeInfo),
    Stopped,
    Recovered,
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
pub type CoreApiConnectionRes<'a> = R<'a, Option<CoreApiConnection>>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reconcile_shape_roundtrips_with_digest_and_cas() {
        let request = CoreSubmitReq {
            operation_id: Cow::Borrowed("00112233445566778899aabbccddeeff"),
            command: CoreCommandInfo::Reconcile {
                core_type: Cow::Owned(chimera_utils::core::CoreType::Clash(
                    chimera_utils::core::ClashCoreType::Mihomo,
                )),
                config: Cow::Borrowed("external-controller: 127.0.0.1:9090\n"),
                expected_digest: Some(Cow::Borrowed("cbf29ce484222325")),
                expected_applied: Some(RevisionIdInfo {
                    epoch: 3,
                    generation: 7,
                    effective_hash: "fedcba9876543210".into(),
                }),
            },
        };
        let encoded = serde_json::to_string(&request).unwrap();
        assert!(encoded.contains("\"type\":\"reconcile\""));
        assert!(encoded.contains("\"expected_applied\""));
        let decoded: CoreSubmitReq<'_> = serde_json::from_str(&encoded).unwrap();
        assert_eq!(decoded.operation_id, request.operation_id);
        assert!(matches!(decoded.command, CoreCommandInfo::Reconcile { .. }));
    }

    #[test]
    fn recover_shape_roundtrips() {
        let request = CoreSubmitReq {
            operation_id: Cow::Borrowed("00112233445566778899aabbccddeeff"),
            command: CoreCommandInfo::Recover,
        };
        let encoded = serde_json::to_string(&request).unwrap();
        assert!(encoded.contains("\"type\":\"recover\""));
        let decoded: CoreSubmitReq<'_> = serde_json::from_str(&encoded).unwrap();
        assert_eq!(decoded.operation_id, request.operation_id);
        assert!(matches!(decoded.command, CoreCommandInfo::Recover));

        let terminal = OperationInfo::succeeded(
            "00112233445566778899aabbccddeeff",
            OperationOutputInfo::Recovered,
        );
        let encoded = serde_json::to_string(&terminal).unwrap();
        assert_eq!(
            serde_json::from_str::<OperationInfo>(&encoded).unwrap(),
            terminal
        );
    }

    #[test]
    fn digest_is_stable_and_content_sensitive() {
        assert_eq!(payload_digest(b""), "cbf29ce484222325");
        assert_eq!(payload_digest(b"abc"), payload_digest(b"abc"));
        assert_ne!(payload_digest(b"abc"), payload_digest(b"abd"));
    }

    #[test]
    fn api_connection_roundtrips() {
        let connection = CoreApiConnection {
            instance_id: "00000000000000010000000000000002".to_string(),
            controller: CoreControllerInfo::Http("http://127.0.0.1:9090".to_string()),
            secret: Some("secret-token".to_string()),
        };
        let encoded = serde_json::to_string(&connection).unwrap();
        assert_eq!(
            serde_json::from_str::<CoreApiConnection>(&encoded).unwrap(),
            connection
        );
    }

    #[test]
    fn terminal_output_roundtrips() {
        let terminal = OperationInfo::succeeded(
            "00112233445566778899aabbccddeeff",
            OperationOutputInfo::Reconciled(ReconcileOutcomeInfo {
                outcome: ReconcileOutcomeKind::Started,
                revision: ConfigRevisionInfo {
                    epoch: 1,
                    generation: 1,
                    source_hash: "0123456789abcdef".into(),
                    effective_hash: "0123456789abcdef".into(),
                },
            }),
        );
        let encoded = serde_json::to_string(&terminal).unwrap();
        assert_eq!(
            serde_json::from_str::<OperationInfo>(&encoded).unwrap(),
            terminal
        );
    }
}
