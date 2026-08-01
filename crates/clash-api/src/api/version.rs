use reqwest::Method;

use crate::{Client, Result};

#[derive(Clone, Debug, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
pub struct Version {
    #[serde(default)]
    pub meta: bool,
    pub version: String,
}

impl Client {
    pub async fn version(&self) -> Result<Version> {
        self.send_json("version", Method::GET, "/version").await
    }
}
