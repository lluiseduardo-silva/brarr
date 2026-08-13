//! Errors raised by the TVDB client.

/// Failure modes of a [`TvdbClient`](crate::TvdbClient) call.
#[derive(Debug, thiserror::Error)]
pub enum TvdbError {
    /// `reqwest::Client::builder()` failed — broken system TLS config
    /// and similar.
    #[error("HTTP client builder failed: {0}")]
    ClientBuild(#[source] reqwest::Error),

    /// Could not assemble the absolute endpoint URL.
    #[error("could not build endpoint URL: {0}")]
    BadUrl(#[from] url::ParseError),

    /// `/login` refused the API key.
    ///
    /// Its own variant because the two credentials `TheTVDB` issues are
    /// not interchangeable and the failure looks identical: a
    /// **project** key funded by the revenue tier needs no `pin`, while
    /// a **user-supported** key needs the end user's subscriber PIN
    /// alongside it. Sending a PIN with the first, or omitting it with
    /// the second, both come back as a refused login.
    #[error("TheTVDB rejected the API key — a user-supported key also needs the subscriber PIN")]
    Unauthorized,

    /// The bearer token was rejected mid-session. Tokens last a month,
    /// so this is a revoked key rather than ordinary expiry.
    #[error("TheTVDB rejected the session token (401)")]
    TokenRejected,

    /// `TheTVDB` answered 404 for the requested id.
    #[error("not found on TheTVDB: {0}")]
    NotFound(String),

    /// `TheTVDB` answered 429.
    #[error("TheTVDB rate limited the request (429)")]
    RateLimited,

    /// Transport failure or an unhandled non-2xx status.
    #[error("HTTP error: {0}")]
    Http(#[from] reqwest::Error),

    /// The body did not match the expected JSON shape.
    #[error("response did not match expected JSON shape: {0}")]
    BadJson(#[source] serde_json::Error),

    /// A paginated walk did not terminate. Guards against a `links.next`
    /// that points at itself — a runaway loop against someone else's API
    /// is the one failure worth refusing outright.
    #[error("pagination did not terminate after {0} pages")]
    RunawayPagination(usize),
}
