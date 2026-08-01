use reqwest::Method;

use crate::{Client, Result};

/// Body accepted by `PUT /configs`.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize)]
pub struct UpdateConfigRequest {
    pub path: String,
    pub payload: String,
}

impl UpdateConfigRequest {
    pub fn from_path(path: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            payload: String::new(),
        }
    }

    pub fn from_payload(payload: impl Into<String>) -> Self {
        Self {
            path: String::new(),
            payload: payload.into(),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize)]
pub struct UpdateConfigOptions {
    pub force: bool,
}

impl Client {
    pub async fn update_config(
        &self,
        request: &UpdateConfigRequest,
        options: UpdateConfigOptions,
    ) -> Result<()> {
        let request = self
            .request(Method::PUT, "/configs")?
            .query(&options)
            .json(request);
        self.send_empty("update_config", request).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn update_config_sends_force_query_and_json_body() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = vec![0_u8; 4096];
            let size = stream.read(&mut request).await.unwrap();
            let request = String::from_utf8_lossy(&request[..size]);
            assert!(request.starts_with("PUT /configs?force=true HTTP/1.1"));
            assert!(request.contains(r#"{"path":"config.yaml","payload":""}"#));
            stream
                .write_all(b"HTTP/1.1 204 No Content\r\nConnection: close\r\n\r\n")
                .await
                .unwrap();
        });

        let client = Client::new_http(address.to_string()).unwrap();
        client
            .update_config(
                &UpdateConfigRequest::from_path("config.yaml"),
                UpdateConfigOptions { force: true },
            )
            .await
            .unwrap();
        server.await.unwrap();
    }

    #[test]
    fn request_constructors_select_one_source() {
        assert_eq!(
            UpdateConfigRequest::from_path("config.yaml"),
            UpdateConfigRequest {
                path: "config.yaml".into(),
                payload: String::new(),
            }
        );
        assert_eq!(
            UpdateConfigRequest::from_payload("port: 7890"),
            UpdateConfigRequest {
                path: String::new(),
                payload: "port: 7890".into(),
            }
        );
    }
}
