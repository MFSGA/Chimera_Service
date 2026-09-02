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
}
