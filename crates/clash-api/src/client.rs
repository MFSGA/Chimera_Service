use std::path::PathBuf;

use url::Url;

use crate::{Error, Result};

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
}
