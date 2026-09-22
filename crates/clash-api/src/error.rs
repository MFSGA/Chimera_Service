pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    #[error("invalid Clash API base URL `{value}`: {source}")]
    InvalidBaseUrl {
        value: String,
        #[source]
        source: url::ParseError,
    },

    #[error("unsupported Clash API URL scheme `{scheme}`")]
    UnsupportedUrlScheme { scheme: String },

    #[error("URL cannot be used as a Clash API base URL: {url}")]
    UrlCannotBeABase { url: url::Url },

    #[error("Clash API base URL must not contain a query or fragment: {url}")]
    BaseUrlHasQueryOrFragment { url: url::Url },

    #[error("unsupported Clash API transport: {transport}")]
    UnsupportedTransport { transport: &'static str },

    #[error("Clash API URL is missing a host: {url}")]
    MissingHost { url: url::Url },

    #[error("Clash API URL is missing a port: {url}")]
    MissingPort { url: url::Url },

    #[error("invalid Clash API authorization header: {source}")]
    InvalidHeader {
        #[source]
        source: hyper::header::InvalidHeaderValue,
    },

    #[error("invalid UTF-8 Clash config path: {path:?}")]
    InvalidConfigPath { path: std::path::PathBuf },

    #[error("Clash API request failed with status {status}: {body}")]
    HttpStatus {
        status: hyper::StatusCode,
        body: String,
    },

    #[error("Clash API I/O failed: {0}")]
    Io(#[from] std::io::Error),

    #[error("Clash API HTTP connection failed: {0}")]
    Hyper(#[from] hyper::Error),

    #[error("failed to build Clash API HTTP request: {0}")]
    Http(#[from] hyper::http::Error),

    #[error("failed to encode/decode Clash API JSON: {0}")]
    Json(#[from] serde_json::Error),

    #[error("failed to encode Clash API query: {0}")]
    Query(#[from] serde_urlencoded::ser::Error),
}
