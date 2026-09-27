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
    /// Optional machine-readable failure classification. Kept as a string at
    /// the IPC boundary so this crate remains independent from the app's
    /// domain metadata crate and newer services can add kinds safely.
    #[builder(default)]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_kind: Option<Cow<'a, str>>,
    /// The service's retryability decision, when it can make one. Missing is
    /// distinct from `false` for compatibility with older service versions.
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

    /// Builds an error envelope with optional structured failure metadata.
    /// Unknown `error_kind` values remain valid wire data for forward
    /// compatibility; consumers should preserve the raw value when they do
    /// not recognize it.
    pub fn other_error_with_kind(
        msg: Cow<'a, str>,
        error_kind: Option<Cow<'a, str>>,
        retryable: Option<bool>,
    ) -> R<'a, T> {
        let code = ResponseCode::OtherError;
        R {
            code,
            msg,
            data: None,
            ts: crate::utils::get_current_ts(),
            error_kind,
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
    fn legacy_response_envelopes_decode_without_error_metadata() {
        let legacy = serde_json::json!({
            "code": "Ok",
            "msg": "ok",
            "data": null,
            "ts": 12
        });

        let response: R<'_, Option<()>> = serde_json::from_value(legacy).unwrap();

        assert!(response.error_kind.is_none());
        assert!(response.retryable.is_none());
    }

    #[test]
    fn absent_error_metadata_stays_off_success_wire() {
        let response: R<'_, ()> = RBuilder::success(());

        let encoded = serde_json::to_value(response).unwrap();
        let object = encoded.as_object().unwrap();

        assert!(!object.contains_key("error_kind"));
        assert!(!object.contains_key("retryable"));
    }

    #[test]
    fn classified_error_metadata_roundtrips() {
        let response: R<'_, ()> = RBuilder::other_error_with_kind(
            Cow::Borrowed("configuration rejected"),
            Some(Cow::Borrowed("invalid_config")),
            Some(false),
        );

        let encoded = serde_json::to_value(response).unwrap();
        assert_eq!(encoded["error_kind"], "invalid_config");
        assert_eq!(encoded["retryable"], false);

        let decoded: R<'_, Option<()>> = serde_json::from_value(encoded).unwrap();
        assert_eq!(decoded.error_kind.as_deref(), Some("invalid_config"));
        assert_eq!(decoded.retryable, Some(false));
    }
}
