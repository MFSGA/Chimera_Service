pub mod contract;
pub mod core;
pub mod log;
pub mod network;
pub mod status;
pub mod ws;

pub use nyanpasu_core_metadata::CoreErrorKind;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::{borrow::Cow, fmt::Debug, io::Error as IoError};

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

/// Stable machine-readable failure kinds carried by `R::error_kind`.
pub mod error_kind {
    use super::CoreErrorKind;

    pub const NOT_STARTED: &str = CoreErrorKind::NotStarted.as_str();
    pub const ALREADY_RUNNING: &str = CoreErrorKind::AlreadyRunning.as_str();
    pub const REVISION_CONFLICT: &str = CoreErrorKind::RevisionConflict.as_str();
    pub const QUARANTINED: &str = CoreErrorKind::Quarantined.as_str();
    pub const CONFIG_CHECK_FAILED: &str = CoreErrorKind::ConfigCheckFailed.as_str();
    pub const CONFIG_NOT_FOUND: &str = CoreErrorKind::ConfigNotFound.as_str();
    pub const BINARY_NOT_FOUND: &str = CoreErrorKind::BinaryNotFound.as_str();
    pub const INVALID_CONFIG: &str = CoreErrorKind::InvalidConfig.as_str();
    pub const CONTROLLER_MISSING: &str = CoreErrorKind::ControllerMissing.as_str();
    pub const APPLY_FAILED: &str = CoreErrorKind::ApplyFailed.as_str();
    pub const APPLY_ROLLBACK_FAILED: &str = CoreErrorKind::ApplyRollbackFailed.as_str();
    pub const STOP_UNCONFIRMED: &str = CoreErrorKind::StopUnconfirmed.as_str();
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

    pub fn other_error_with_kind(msg: Cow<'a, str>, kind: Option<CoreErrorKind>) -> R<'a, T> {
        Self::other_error_with_kind_and_retryable(msg, kind, None)
    }

    pub fn other_error_with_kind_and_retryable(
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
