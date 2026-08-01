pub mod contract;
pub mod core;
pub mod log;
pub mod network;
pub mod status;
pub mod ws;

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
    pub const NOT_STARTED: &str = "not_started";
    pub const ALREADY_RUNNING: &str = "already_running";
    pub const REVISION_CONFLICT: &str = "revision_conflict";
    pub const QUARANTINED: &str = "quarantined";
    pub const CONFIG_CHECK_FAILED: &str = "config_check_failed";
    pub const CONFIG_NOT_FOUND: &str = "config_not_found";
    pub const BINARY_NOT_FOUND: &str = "binary_not_found";
    pub const INVALID_CONFIG: &str = "invalid_config";
    pub const CONTROLLER_MISSING: &str = "controller_missing";
    pub const APPLY_FAILED: &str = "apply_failed";
    pub const APPLY_ROLLBACK_FAILED: &str = "apply_rollback_failed";
    pub const STOP_UNCONFIRMED: &str = "stop_unconfirmed";
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
        }
    }

    pub fn other_error_with_kind(msg: Cow<'a, str>, kind: Option<Cow<'a, str>>) -> R<'a, T> {
        let code = ResponseCode::OtherError;
        R {
            code,
            msg,
            data: None,
            ts: crate::utils::get_current_ts(),
            error_kind: kind,
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
        }
    }
}
