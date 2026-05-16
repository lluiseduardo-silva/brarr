//! Errors raised by [`crate::NewznabClient`].

/// Errors specific to the Newznab HTTP client.
#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    /// API key contained characters incompatible with a URL query parameter.
    #[error("invalid API key: must be alphanumeric")]
    InvalidApiKey,

    /// `reqwest::Client::builder()` failed.
    #[error("HTTP client builder failed: {0}")]
    ClientBuild(#[source] reqwest::Error),

    /// Building the request URL from the configured base + query failed.
    #[error("could not build endpoint URL: {0}")]
    BadUrl(#[from] url::ParseError),

    /// Transport or 4xx/5xx response.
    #[error("HTTP error: {0}")]
    Http(#[from] reqwest::Error),

    /// Response body was not valid UTF-8.
    #[error("response was not valid UTF-8: {0}")]
    BadUtf8(#[from] std::string::FromUtf8Error),

    /// XML parse failure (malformed feed, unexpected schema).
    #[error("XML parse error: {0}")]
    Xml(String),
}
