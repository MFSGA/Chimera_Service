use std::{path::PathBuf, time::Duration};

use reqwest::{
    Method,
    header::{AUTHORIZATION, HeaderValue},
};
use url::Url;

use crate::{Error, Result};

const LOCAL_TRANSPORT_BASE_URL: &str = "http://localhost/";

/// The transport used to connect to the Mihomo external controller.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Host {
    NamedPipe(PathBuf),
    UnixSocket(PathBuf),
    Http(Url),
}

/// More descriptive alias for [`Host`].
pub type ControllerEndpoint = Host;

impl Host {
    pub fn named_pipe(path: impl Into<PathBuf>) -> Self {
        Self::NamedPipe(path.into())
    }

    pub fn unix_socket(path: impl Into<PathBuf>) -> Self {
        Self::UnixSocket(path.into())
    }

    /// Construct an HTTP endpoint from either `host:port` or a complete URL.
    pub fn http(base_url: impl AsRef<str>) -> Result<Self> {
        parse_controller_url(base_url.as_ref(), "http")
    }

    /// Construct an HTTPS endpoint from either `host:port` or a complete URL.
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

    pub fn as_str(&self) -> &str {
        &self.0
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

#[derive(Clone)]
pub struct Client {
    client: reqwest::Client,
    host: Host,
    base_url: Url,
    secret: Secret,
}

pub struct ClientBuilder {
    host: Host,
    secret: Secret,
    timeout: Duration,
}

impl Client {
    pub fn builder(host: Host) -> ClientBuilder {
        ClientBuilder {
            host,
            secret: Secret::default(),
            timeout: Duration::from_secs(10),
        }
    }

    pub fn new_http(base_url: impl AsRef<str>) -> Result<Self> {
        Self::builder(Host::http(base_url)?).build()
    }

    pub fn host(&self) -> &Host {
        &self.host
    }

    pub fn base_url(&self) -> Result<&Url> {
        Ok(&self.base_url)
    }

    pub(crate) fn request(&self, method: Method, endpoint: &str) -> Result<reqwest::RequestBuilder> {
        let endpoint = endpoint.trim_start_matches('/');
        let url = self
            .base_url()?
            .join(endpoint)
            .map_err(|source| Error::InvalidBaseUrl {
                value: endpoint.to_owned(),
                source,
            })?;
        let mut request = self.client.request(method, url);
        if matches!(&self.host, Host::Http(_)) && !self.secret.is_empty() {
            let mut value = HeaderValue::from_str(&format!("Bearer {}", self.secret.as_str()))
                .map_err(Error::InvalidSecret)?;
            value.set_sensitive(true);
            request = request.header(AUTHORIZATION, value);
        }
        Ok(request)
    }

    pub(crate) async fn send_json<T: serde::de::DeserializeOwned>(
        &self,
        operation: &'static str,
        method: Method,
        endpoint: &str,
    ) -> Result<T> {
        let response = self
            .request(method, endpoint)?
            .send()
            .await
            .map_err(|source| Error::Request { operation, source })?;
        let status = response.status();
        let bytes = response
            .bytes()
            .await
            .map_err(|source| Error::Request { operation, source })?;
        if !status.is_success() {
            return Err(Error::HttpStatus { operation, status });
        }
        serde_json::from_slice(&bytes).map_err(|source| Error::Decode { operation, source })
    }
}

impl std::fmt::Debug for Client {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Client")
            .field("host", &self.host)
            .field("secret", &self.secret)
            .finish_non_exhaustive()
    }
}

impl ClientBuilder {
    pub fn secret(mut self, secret: impl Into<Secret>) -> Self {
        self.secret = secret.into();
        self
    }

    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    pub fn build(self) -> Result<Client> {
        let mut builder = reqwest::Client::builder().timeout(self.timeout);
        let (host, base_url, secret) = match self.host {
            Host::Http(base_url) => {
                let base_url = normalize_base_url(base_url)?;
                (Host::Http(base_url.clone()), base_url, self.secret)
            }
            Host::NamedPipe(path) => {
                #[cfg(windows)]
                {
                    builder = builder.windows_named_pipe(path.as_path());
                    (
                        Host::NamedPipe(path),
                        Url::parse(LOCAL_TRANSPORT_BASE_URL)
                            .expect("valid local transport base URL"),
                        Secret::default(),
                    )
                }
                #[cfg(not(windows))]
                {
                    let _ = (path, builder);
                    return Err(Error::UnsupportedTransport {
                        transport: "Windows named pipe",
                    });
                }
            }
            Host::UnixSocket(path) => {
                #[cfg(unix)]
                {
                    builder = builder.unix_socket(path.as_path());
                    (
                        Host::UnixSocket(path),
                        Url::parse(LOCAL_TRANSPORT_BASE_URL)
                            .expect("valid local transport base URL"),
                        Secret::default(),
                    )
                }
                #[cfg(not(unix))]
                {
                    let _ = (path, builder);
                    return Err(Error::UnsupportedTransport {
                        transport: "Unix domain socket",
                    });
                }
            }
        };
        let client = builder.build().map_err(Error::BuildClient)?;
        Ok(Client {
            client,
            host,
            base_url,
            secret,
        })
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
        let client = Client::builder(Host::http("127.0.0.1:9090").unwrap())
            .secret("do-not-log-me")
            .build()
            .unwrap();
        assert!(!format!("{client:?}").contains("do-not-log-me"));
    }

    #[tokio::test]
    async fn version_request_uses_bearer_auth_and_decodes_response() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = vec![0_u8; 4096];
            let size = stream.read(&mut request).await.unwrap();
            let request = String::from_utf8_lossy(&request[..size]);
            assert!(request.starts_with("GET /version HTTP/1.1"));
            assert!(request.to_ascii_lowercase().contains("authorization: bearer secret"));
            let body = r#"{"meta":true,"version":"1.18.9"}"#;
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            stream.write_all(response.as_bytes()).await.unwrap();
        });

        let client = Client::builder(Host::http(address.to_string()).unwrap())
            .secret("secret")
            .build()
            .unwrap();
        let version = client.version().await.unwrap();
        assert!(version.meta);
        assert_eq!(version.version, "1.18.9");
        server.await.unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn named_pipe_uses_the_synthetic_url_without_auth() {
        let client = Client::builder(Host::named_pipe(r"\\.\pipe\controller"))
            .secret("ignored")
            .build()
            .unwrap();
        assert_eq!(client.base_url().unwrap().as_str(), LOCAL_TRANSPORT_BASE_URL);
        let request = client.request(Method::GET, "/version").unwrap().build().unwrap();
        assert_eq!(request.url().as_str(), "http://localhost/version");
        assert!(!request.headers().contains_key(AUTHORIZATION));
    }

    #[cfg(unix)]
    #[test]
    fn unix_socket_uses_the_synthetic_url_without_auth() {
        let client = Client::builder(Host::unix_socket("/tmp/controller.sock"))
            .secret("ignored")
            .build()
            .unwrap();
        assert_eq!(client.base_url().unwrap().as_str(), LOCAL_TRANSPORT_BASE_URL);
        let request = client.request(Method::GET, "/version").unwrap().build().unwrap();
        assert_eq!(request.url().as_str(), "http://localhost/version");
        assert!(!request.headers().contains_key(AUTHORIZATION));
    }
}
