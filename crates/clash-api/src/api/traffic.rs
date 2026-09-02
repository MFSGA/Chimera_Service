use std::fmt;

use reqwest::Method;

use crate::{Client, HttpStream, Result, retry::RequestMetadata};

#[derive(
    Clone,
    Copy,
    Debug,
    Default,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    serde::Deserialize,
    serde::Serialize,
    specta::Type,
)]
#[serde(transparent)]
pub struct Bytes(i64);
impl Bytes {
    pub const fn new(value: i64) -> Self {
        Self(value)
    }
    pub const fn get(self) -> i64 {
        self.0
    }
}
impl From<i64> for Bytes {
    fn from(value: i64) -> Self {
        Self(value)
    }
}
impl From<Bytes> for i64 {
    fn from(value: Bytes) -> Self {
        value.0
    }
}
impl fmt::Display for Bytes {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

#[derive(
    Clone,
    Copy,
    Debug,
    Default,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    serde::Deserialize,
    serde::Serialize,
    specta::Type,
)]
#[serde(transparent)]
pub struct BytesPerSecond(i64);
impl BytesPerSecond {
    pub const fn new(value: i64) -> Self {
        Self(value)
    }
    pub const fn get(self) -> i64 {
        self.0
    }
}
impl From<i64> for BytesPerSecond {
    fn from(value: i64) -> Self {
        Self(value)
    }
}
impl From<BytesPerSecond> for i64 {
    fn from(value: BytesPerSecond) -> Self {
        value.0
    }
}
impl fmt::Display for BytesPerSecond {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, serde::Deserialize, serde::Serialize, specta::Type,
)]
#[serde(rename_all = "camelCase")]
pub struct Traffic {
    pub up: BytesPerSecond,
    pub down: BytesPerSecond,
    pub up_total: Bytes,
    pub down_total: Bytes,
}

impl Client {
    pub async fn traffic(&self) -> Result<HttpStream<Traffic>> {
        const OPERATION: &str = "traffic";
        let response = self
            .send(RequestMetadata::new(OPERATION, Method::GET, true), || {
                self.get("/traffic")
            })
            .await?;
        Ok(HttpStream::from_response(response, OPERATION))
    }
    pub async fn traffic_ws(&self) -> Result<reqwest_websocket::WebSocket> {
        self.websocket(
            RequestMetadata::new("traffic_ws", Method::GET, true),
            || self.get("/traffic"),
        )
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn traffic_uses_signed_fields() {
        let traffic: Traffic =
            serde_json::from_str(r#"{"up":-1,"down":2,"upTotal":3,"downTotal":4}"#).unwrap();
        assert_eq!(traffic.up.get(), -1);
        assert_eq!(traffic.down_total.get(), 4);
    }
}
