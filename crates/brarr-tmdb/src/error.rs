//! Errors raised by the TMDB client.

/// Failure modes of a [`TmdbClient`](crate::TmdbClient) call.
#[derive(Debug, thiserror::Error)]
pub enum TmdbError {
    /// The read access token contained bytes that cannot become an HTTP
    /// header value (non-ASCII, control characters).
    #[error("invalid TMDB token: must be ASCII")]
    InvalidToken,

    /// `reqwest::Client::builder()` failed — broken system TLS config
    /// and similar.
    #[error("HTTP client builder failed: {0}")]
    ClientBuild(#[source] reqwest::Error),

    /// Could not assemble the absolute endpoint URL.
    #[error("could not build endpoint URL: {0}")]
    BadUrl(#[from] url::ParseError),

    /// TMDB answered 401. Almost always a wrong or revoked token — the
    /// v3 API key and the v4 read access token are not interchangeable
    /// in the `Authorization: Bearer` header.
    #[error("TMDB rejected the credentials (401) — check the read access token")]
    Unauthorized,

    /// TMDB answered 404 for the requested id.
    #[error("not found on TMDB: {0}")]
    NotFound(String),

    /// TMDB answered 429. The documented limit is generous (the old
    /// 40-requests-per-10-seconds cap was retired in 2019), so this
    /// usually means a burst worth slowing down rather than a quota.
    #[error("TMDB rate limited the request (429)")]
    RateLimited,

    /// Transport failure or an unhandled non-2xx status.
    #[error("HTTP error: {0}")]
    Http(#[from] reqwest::Error),

    /// The body did not match the expected JSON shape.
    #[error("response did not match expected JSON shape: {0}")]
    BadJson(#[source] serde_json::Error),
}
