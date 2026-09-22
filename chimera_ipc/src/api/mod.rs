pub mod core;
pub mod log;
pub mod network;
pub mod status;
pub mod ws;

use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::{borrow::Cow, fmt::Debug, io::Error as IoError};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "specta", derive(specta::Type))]
#[serde(rename_all = "snake_case")]
pub enum CoreErrorKind {
    NotStarted,
    AlreadyRunning,
    RevisionConflict,
    Quarantined,
    ConfigCheckFailed,
    ConfigNotFound,
    BinaryNotFound,
    InvalidConfig,
    ControllerMissing,
    ApplyFailed,
    ApplyRollbackFailed,
    StopUnconfirmed,
    ShuttingDown,
    QueueFull,
    OperationConflict,
    BackendUnavailable,
    Internal,
}

impl CoreErrorKind {
    pub const ALL: &'static [Self] = &[
        Self::NotStarted,
        Self::AlreadyRunning,
        Self::RevisionConflict,
        Self::Quarantined,
        Self::ConfigCheckFailed,
        Self::ConfigNotFound,
        Self::BinaryNotFound,
        Self::InvalidConfig,
        Self::ControllerMissing,
        Self::ApplyFailed,
        Self::ApplyRollbackFailed,
        Self::StopUnconfirmed,
        Self::ShuttingDown,
        Self::QueueFull,
        Self::OperationConflict,
        Self::BackendUnavailable,
        Self::Internal,
    ];

    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::NotStarted => "not_started",
            Self::AlreadyRunning => "already_running",
            Self::RevisionConflict => "revision_conflict",
            Self::Quarantined => "quarantined",
            Self::ConfigCheckFailed => "config_check_failed",
            Self::ConfigNotFound => "config_not_found",
            Self::BinaryNotFound => "binary_not_found",
            Self::InvalidConfig => "invalid_config",
            Self::ControllerMissing => "controller_missing",
            Self::ApplyFailed => "apply_failed",
            Self::ApplyRollbackFailed => "apply_rollback_failed",
            Self::StopUnconfirmed => "stop_unconfirmed",
            Self::ShuttingDown => "shutting_down",
            Self::QueueFull => "queue_full",
            Self::OperationConflict => "operation_conflict",
            Self::BackendUnavailable => "backend_unavailable",
            Self::Internal => "internal",
        }
    }

    pub const fn default_retryable(&self) -> bool {
        matches!(self, Self::QueueFull | Self::BackendUnavailable)
    }

    pub fn from_wire(value: &str) -> Option<Self> {
        Self::ALL
            .iter()
            .copied()
            .find(|kind| kind.as_str() == value)
    }
}

impl std::fmt::Display for CoreErrorKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Serialize, Deserialize, Clone, Copy, Default, PartialEq)]
pub enum ResponseCode {
    #[default]
    Ok = 0,
    OtherError = -1,
}

/// ResponseCode message mapping
impl ResponseCode {
    pub const fn msg(&self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::OtherError => "other error",
        }
    }
}

/// The IPC Response body definition
#[derive(Debug, Serialize, Deserialize, Clone, Builder)]
#[builder(build_fn(validate = "Self::validate"))]
#[serde(bound = "T: Serialize + DeserializeOwned")]
pub struct R<'a, T: Serialize + DeserializeOwned + Debug> {
    pub code: ResponseCode,
    #[builder(default = "self.default_msg()")]
    pub msg: Cow<'a, str>,
    #[builder(setter(into, strip_option))]
    pub data: Option<T>,
    #[builder(setter(skip), default = "self.default_ts()")]
    pub ts: i64,
    #[builder(default)]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_kind: Option<Cow<'a, str>>,
    #[builder(default)]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retryable: Option<bool>,
}

impl<T: Serialize + DeserializeOwned + Debug> R<'_, T> {
    pub fn ok(self) -> Result<Self, IoError> {
        if self.code == ResponseCode::Ok {
            Ok(self)
        } else {
            Err(IoError::other(format!(
                "Response code is not Ok: {self:#?}"
            )))
        }
    }
}

impl<'a, T: Serialize + DeserializeOwned + Debug> RBuilder<'a, T> {
    fn default_ts(&self) -> i64 {
        crate::utils::get_current_ts()
    }

    fn default_msg(&self) -> Cow<'a, str> {
        Cow::Borrowed(if let Some(code) = self.code {
            code.msg()
        } else {
            ResponseCode::Ok.msg()
        })
    }

    fn validate(&self) -> Result<(), String> {
        if self.code.is_none() {
            return Err("code is required".to_string());
        }
        if self.msg.is_none() {
            return Err("msg is required".to_string());
        }
        Ok(())
    }

    pub fn other_error(msg: Cow<'a, str>) -> R<'a, T> {
        let code = ResponseCode::OtherError;
        R {
            code,
            msg,
            data: None,
            ts: crate::utils::get_current_ts(),
            error_kind: None,
            retryable: None,
        }
    }

    pub fn other_error_with_kind(
        msg: Cow<'a, str>,
        kind: Option<CoreErrorKind>,
        retryable: Option<bool>,
    ) -> R<'a, T> {
        let code = ResponseCode::OtherError;
        R {
            code,
            msg,
            data: None,
            ts: crate::utils::get_current_ts(),
            error_kind: kind.map(|kind| Cow::Borrowed(kind.as_str())),
            retryable,
        }
    }

    pub fn success(data: T) -> R<'a, T> {
        let code = ResponseCode::Ok;
        R {
            code,
            msg: Cow::Borrowed(code.msg()),
            data: Some(data),
            ts: crate::utils::get_current_ts(),
            error_kind: None,
            retryable: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn core_error_kind_wire_strings_and_defaults_match_ref() {
        for kind in CoreErrorKind::ALL {
            assert_eq!(
                serde_json::to_string(kind).unwrap(),
                format!("\"{}\"", kind.as_str())
            );
            assert_eq!(CoreErrorKind::from_wire(kind.as_str()), Some(*kind));
        }
        assert!(CoreErrorKind::QueueFull.default_retryable());
        assert!(CoreErrorKind::BackendUnavailable.default_retryable());
        assert!(!CoreErrorKind::RevisionConflict.default_retryable());
        assert!(!CoreErrorKind::OperationConflict.default_retryable());
    }

    #[test]
    fn typed_error_envelope_is_additive_and_old_shape_stays_omitted() {
        let typed = RBuilder::<Option<()>>::other_error_with_kind(
            Cow::Borrowed("conflict"),
            Some(CoreErrorKind::OperationConflict),
            Some(false),
        );
        let value = serde_json::to_value(&typed).unwrap();
        assert_eq!(
            value.get("error_kind").and_then(serde_json::Value::as_str),
            Some("operation_conflict")
        );
        assert_eq!(
            value.get("retryable").and_then(serde_json::Value::as_bool),
            Some(false)
        );

        let legacy = RBuilder::<Option<()>>::other_error(Cow::Borrowed("plain"));
        let value = serde_json::to_value(&legacy).unwrap();
        assert!(value.get("error_kind").is_none());
        assert!(value.get("retryable").is_none());
    }
}
