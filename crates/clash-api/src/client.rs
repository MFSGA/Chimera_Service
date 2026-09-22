use std::path::{Path, PathBuf};

use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::{
    Method, Request,
    header::{AUTHORIZATION, CONTENT_TYPE, HOST, HeaderValue},
};
use hyper_util::rt::TokioIo;
use interprocess::local_socket::{
    GenericFilePath, ToFsName,
    tokio::{Stream, prelude::*},
};
use serde::{Serialize, de::DeserializeOwned};
use tokio::io::{AsyncRead, AsyncWrite};
use url::Url;

use crate::{Error, Result};

/// Core version information returned by GET /version.
#[derive(Clone, Debug, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
#[cfg_attr(feature = "specta", derive(specta::Type))]
pub struct Version {
    #[serde(default)]
    pub meta: bool,
    pub version: String,
    #[serde(default)]
    pub premium: Option<bool>,
}

/// The transport used to connect to the Mihomo external controller.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Host {
    NamedPipe(PathBuf),
    UnixSocket(PathBuf),
    Http(Url),
}

/// More descriptive alias for Host.
pub type ControllerEndpoint = Host;

impl Host {
    pub fn named_pipe(path: impl Into<PathBuf>) -> Self {
        Self::NamedPipe(path.into())
    }

    pub fn unix_socket(path: impl Into<PathBuf>) -> Self {
        Self::UnixSocket(path.into())
    }

    /// Construct an HTTP endpoint from either host:port or a complete URL.
    pub fn http(base_url: impl AsRef<str>) -> Result<Self> {
        parse_controller_url(base_url.as_ref(), "http")
    }

    /// Parse an HTTPS controller endpoint.
    ///
    /// The lightweight Chimera transport currently supports HTTP and local
    /// sockets only, so requests to this endpoint fail closed.
    pub fn https(base_url: impl AsRef<str>) -> Result<Self> {
        parse_controller_url(base_url.as_ref(), "https")
    }

    /// Construct an endpoint from a complete HTTP(S) URL.
    pub fn url(base_url: impl AsRef<str>) -> Result<Self> {
        parse_complete_url(base_url.as_ref())
    }
}

/// Controller secret with redacted debug output.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct Secret(String);

impl Secret {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    fn authorization_value(&self) -> Result<Option<HeaderValue>> {
        if self.is_empty() {
            return Ok(None);
        }
        HeaderValue::from_str(&format!("Bearer {}", self.0))
            .map(Some)
            .map_err(|source| Error::InvalidHeader { source })
    }
}

impl From<String> for Secret {
    fn from(value: String) -> Self {
        Self(value)
    }
}

impl From<&str> for Secret {
    fn from(value: &str) -> Self {
        Self(value.to_owned())
    }
}

impl std::fmt::Debug for Secret {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("Secret([REDACTED])")
    }
}

/// Small transport-aware controller client.
///
/// This is the shared boundary used by daemon-side transactions and may be
/// reused by app-side controller access. HTTP and local IPC expose the same
/// request semantics; call sites do not branch on transport.
#[derive(Clone, Debug)]
pub struct Client {
    host: Host,
    secret: Secret,
}

impl Client {
    pub fn new(host: Host) -> Self {
        Self {
            host,
            secret: Secret::default(),
        }
    }

    pub fn with_secret(host: Host, secret: impl Into<Secret>) -> Self {
        Self {
            host,
            secret: secret.into(),
        }
    }

    pub fn host(&self) -> &Host {
        &self.host
    }

    /// Reload the complete runtime configuration through PUT /configs?force=true.
    pub async fn update_config_from_path(&self, path: impl AsRef<Path>) -> Result<()> {
        #[derive(Serialize)]
        struct UpdateConfigRequest<'a> {
            path: &'a str,
            payload: &'a str,
        }

        let path = path
            .as_ref()
            .to_str()
            .ok_or_else(|| Error::InvalidConfigPath {
                path: path.as_ref().to_path_buf(),
            })?;
        let body = serde_json::to_vec(&UpdateConfigRequest { path, payload: "" })?;
        self.request(Method::PUT, "/configs?force=true", Some(body))
            .await
            .map(|_| ())
    }

    pub async fn version(&self) -> Result<Version> {
        self.get_json("/version").await
    }

    /// Read the controller's effective config.
    pub async fn configs(&self) -> Result<crate::RuntimeConfig> {
        self.get_json("/configs").await
    }

    pub async fn get_json<T>(&self, path: &str) -> Result<T>
    where
        T: DeserializeOwned,
    {
        let bytes = self.request(Method::GET, path, None).await?;
        Ok(serde_json::from_slice(&bytes)?)
    }

    pub async fn get_json_query<T, Q>(&self, path: &str, query: &Q) -> Result<T>
    where
        T: DeserializeOwned,
        Q: Serialize + ?Sized,
    {
        let query = serde_urlencoded::to_string(query)?;
        let target = if query.is_empty() {
            path.to_owned()
        } else {
            format!("{path}?{query}")
        };
        self.get_json(&target).await
    }

    pub async fn delete(&self, path: &str) -> Result<()> {
        self.request(Method::DELETE, path, None).await.map(|_| ())
    }

    pub async fn put_json<D>(&self, path: &str, data: &D) -> Result<()>
    where
        D: Serialize + ?Sized,
    {
        let body = serde_json::to_vec(data)?;
        self.request(Method::PUT, path, Some(body))
            .await
            .map(|_| ())
    }

    pub async fn patch_json<D>(&self, path: &str, data: &D) -> Result<()>
    where
        D: Serialize + ?Sized,
    {
        let body = serde_json::to_vec(data)?;
        self.request(Method::PATCH, path, Some(body))
            .await
            .map(|_| ())
    }

    pub async fn patch_config(&self, patch: &crate::ConfigPatch) -> Result<()> {
        self.patch_json("/configs", patch).await
    }

    async fn request(
        &self,
        method: Method,
        path_and_query: &str,
        body: Option<Vec<u8>>,
    ) -> Result<Vec<u8>> {
        match &self.host {
            Host::Http(base) => self.request_http(base, method, path_and_query, body).await,
            Host::NamedPipe(path) => {
                #[cfg(windows)]
                {
                    self.request_local(path, true, method, path_and_query, body)
                        .await
                }
                #[cfg(not(windows))]
                {
                    let _ = (path, method, path_and_query, body);
                    Err(Error::UnsupportedTransport {
                        transport: "named pipe",
                    })
                }
            }
            Host::UnixSocket(path) => {
                #[cfg(unix)]
                {
                    self.request_local(path, false, method, path_and_query, body)
                        .await
                }
                #[cfg(not(unix))]
                {
                    let _ = (path, method, path_and_query, body);
                    Err(Error::UnsupportedTransport {
                        transport: "unix socket",
                    })
                }
            }
        }
    }

    async fn request_http(
        &self,
        base: &Url,
        method: Method,
        path_and_query: &str,
        body: Option<Vec<u8>>,
    ) -> Result<Vec<u8>> {
        if base.scheme() != "http" {
            return Err(Error::UnsupportedTransport { transport: "https" });
        }
        let host = base
            .host_str()
            .ok_or_else(|| Error::MissingHost { url: base.clone() })?;
        let port = base
            .port_or_known_default()
            .ok_or_else(|| Error::MissingPort { url: base.clone() })?;
        let stream = tokio::net::TcpStream::connect((host, port)).await?;
        let joined = base
            .join(path_and_query.trim_start_matches('/'))
            .map_err(|source| Error::InvalidBaseUrl {
                value: base.to_string(),
                source,
            })?;
        let target = match joined.query() {
            Some(query) => format!("{}?{query}", joined.path()),
            None => joined.path().to_owned(),
        };
        let authority = match joined.port() {
            Some(port) => format!("{}:{port}", joined.host_str().unwrap_or(host)),
            None => joined.host_str().unwrap_or(host).to_owned(),
        };
        self.send_over_io(stream, method, &target, &authority, body)
            .await
    }

    #[cfg(any(windows, unix))]
    async fn request_local(
        &self,
        path: &Path,
        named_pipe: bool,
        method: Method,
        path_and_query: &str,
        body: Option<Vec<u8>>,
    ) -> Result<Vec<u8>> {
        let endpoint = if named_pipe && cfg!(windows) {
            let raw = path.to_string_lossy();
            if raw.starts_with(r"\\.\pipe\") {
                raw.into_owned()
            } else {
                format!(r"\\.\pipe\{raw}")
            }
        } else {
            path.to_string_lossy().into_owned()
        };
        let name = endpoint.to_fs_name::<GenericFilePath>()?;
        let stream = Stream::connect(name).await?;
        self.send_over_io(stream, method, path_and_query, "localhost", body)
            .await
    }

    async fn send_over_io<T>(
        &self,
        io: T,
        method: Method,
        target: &str,
        authority: &str,
        body: Option<Vec<u8>>,
    ) -> Result<Vec<u8>>
    where
        T: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        let io = TokioIo::new(io);
        let (mut sender, connection) =
            hyper::client::conn::http1::handshake::<_, Full<Bytes>>(io).await?;
        tokio::spawn(async move {
            if let Err(error) = connection.await {
                tracing::debug!("controller HTTP connection ended: {error}");
            }
        });

        let mut builder = Request::builder()
            .method(method)
            .uri(target)
            .header(HOST, authority);
        if let Some(value) = self.secret.authorization_value()? {
            builder = builder.header(AUTHORIZATION, value);
        }
        let body = match body {
            Some(body) => {
                builder = builder.header(CONTENT_TYPE, "application/json");
                Full::new(Bytes::from(body))
            }
            None => Full::new(Bytes::new()),
        };
        let response = sender.send_request(builder.body(body)?).await?;
        let status = response.status();
        let bytes = response.into_body().collect().await?.to_bytes().to_vec();
        if !status.is_success() {
            return Err(Error::HttpStatus {
                status,
                body: String::from_utf8_lossy(&bytes).into_owned(),
            });
        }
        Ok(bytes)
    }
}

fn parse_controller_url(value: &str, default_scheme: &str) -> Result<Host> {
    if value.contains("://") {
        return parse_complete_url(value);
    }
    parse_complete_url(&format!("{default_scheme}://{value}"))
}

fn parse_complete_url(value: &str) -> Result<Host> {
    let url = Url::parse(value).map_err(|source| Error::InvalidBaseUrl {
        value: value.to_owned(),
        source,
    })?;
    Ok(Host::Http(normalize_base_url(url)?))
}

fn normalize_base_url(mut base_url: Url) -> Result<Url> {
    if !matches!(base_url.scheme(), "http" | "https") {
        return Err(Error::UnsupportedUrlScheme {
            scheme: base_url.scheme().to_owned(),
        });
    }
    if base_url.cannot_be_a_base() {
        return Err(Error::UrlCannotBeABase { url: base_url });
    }
    if base_url.query().is_some() || base_url.fragment().is_some() {
        return Err(Error::BaseUrlHasQueryOrFragment { url: base_url });
    }
    if !base_url.path().ends_with('/') {
        let mut path = base_url.path().to_owned();
        path.push('/');
        base_url.set_path(&path);
    }
    Ok(base_url)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_port_is_ergonomic_and_normalized() {
        let Host::Http(url) = Host::http("127.0.0.1:9090/api").unwrap() else {
            panic!("expected http host");
        };
        assert_eq!(url.as_str(), "http://127.0.0.1:9090/api/");
    }

    #[test]
    fn invalid_controller_urls_are_rejected() {
        assert!(matches!(
            Host::url("ftp://127.0.0.1/api"),
            Err(Error::UnsupportedUrlScheme { .. })
        ));
        assert!(matches!(
            Host::url("http://127.0.0.1/api?secret=x"),
            Err(Error::BaseUrlHasQueryOrFragment { .. })
        ));
    }

    #[test]
    fn secret_debug_is_redacted() {
        let secret = Secret::new("do-not-log-me");
        assert!(!format!("{secret:?}").contains("do-not-log-me"));
    }

    #[test]
    fn local_endpoint_kinds_are_preserved() {
        assert!(matches!(
            Host::named_pipe("chimera-test"),
            Host::NamedPipe(path) if path == PathBuf::from("chimera-test")
        ));
        assert!(matches!(
            Host::unix_socket("/tmp/chimera-test.sock"),
            Host::UnixSocket(path) if path == PathBuf::from("/tmp/chimera-test.sock")
        ));
    }

    #[tokio::test]
    async fn http_transport_updates_and_verifies_configs() {
        use std::{collections::HashMap, sync::Arc};

        use axum::{
            Json, Router,
            extract::Query,
            http::{HeaderMap, StatusCode},
            routing::get,
        };
        use tokio::sync::Mutex;

        let seen = Arc::new(Mutex::new(None::<serde_json::Value>));
        let seen_put = seen.clone();
        let app = Router::new().route(
            "/configs",
            get(|| async { Json(serde_json::json!({"mode": "rule"})) }).put(
                move |Query(query): Query<HashMap<String, String>>,
                      headers: HeaderMap,
                      Json(body): Json<serde_json::Value>| {
                    let seen = seen_put.clone();
                    async move {
                        assert_eq!(query.get("force").map(String::as_str), Some("true"));
                        assert_eq!(
                            headers
                                .get(AUTHORIZATION)
                                .and_then(|value| value.to_str().ok()),
                            Some("Bearer secret")
                        );
                        *seen.lock().await = Some(body);
                        StatusCode::NO_CONTENT
                    }
                },
            ),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let client = Client::with_secret(
            Host::http(format!("http://{address}")).unwrap(),
            Secret::new("secret"),
        );
        client
            .update_config_from_path(Path::new("runtime.yaml"))
            .await
            .unwrap();
        assert_eq!(
            client.configs().await.unwrap().mode.as_deref(),
            Some("rule")
        );
        assert_eq!(
            seen.lock().await.as_ref().unwrap()["path"],
            serde_json::Value::String("runtime.yaml".into())
        );

        server.abort();
    }
}
