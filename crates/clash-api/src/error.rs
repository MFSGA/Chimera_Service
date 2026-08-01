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

    #[error("{transport} transport is not yet supported by this client")]
    UnsupportedTransport { transport: &'static str },

    #[error("the Clash API secret cannot be represented as an HTTP header")]
    InvalidSecret(#[source] reqwest::header::InvalidHeaderValue),

    #[error("failed to build the Clash API HTTP client: {0}")]
    BuildClient(#[source] reqwest::Error),

    #[error("Clash API request `{operation}` failed: {source}")]
    Request {
        operation: &'static str,
        #[source]
        source: reqwest::Error,
    },

    #[error("Clash API request `{operation}` returned HTTP {status}")]
    HttpStatus {
        operation: &'static str,
        status: reqwest::StatusCode,
    },

    #[error("failed to decode Clash API response for `{operation}`: {source}")]
    Decode {
        operation: &'static str,
        #[source]
        source: serde_json::Error,
    },
}
