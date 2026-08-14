//! Crate-wide error type.
//!
//! Library code returns [`AppError`] (typed via `thiserror`); the binary
//! converts it to `anyhow::Error` at the boundary. HTTP handlers map it
//! to a status code through an `IntoResponse` impl so we never panic on
//! a SQL miss or template render failure.

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};

/// Errors that can bubble up out of the orchestrator's library surface.
///
/// Most variants wrap a foreign error; we keep the variants narrow so
/// the HTTP/gRPC translation layer can pick an appropriate status code
/// without `match`ing on string contents.
#[derive(Debug, thiserror::Error)]
pub enum AppError {
    /// Database error from `sqlx`.
    #[error("database error: {0}")]
    Database(#[from] sqlx::Error),

    /// Schema migration error at boot.
    #[error("migration error: {0}")]
    Migrate(#[from] sqlx::migrate::MigrateError),

    /// Template render failure.
    #[error("template error: {0}")]
    Template(#[from] askama::Error),

    /// JSON (de)serialization error.
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),

    /// UNIT3D HTTP client error.
    #[error("tracker error: {0}")]
    Tracker(#[from] brarr_tracker_unit3d::ClientError),

    /// TMDB metadata client error. Deliberately **not** `#[from]`: the
    /// conversion in `tmdb_sync` peels off 401 and 404 first so the
    /// operator gets "check the read access token" instead of a bare
    /// HTTP status, and only the rest lands here.
    #[error("tmdb error: {0}")]
    Tmdb(#[source] brarr_tmdb::TmdbError),

    /// A failure from a metadata provider, in the neutral vocabulary.
    ///
    /// Replaces the two divergent conversions the provider crates each
    /// had — one peeling 401 into a sentence about the read access token,
    /// the other into a sentence about the API key — with one boundary
    /// that already knows which source it is about. `#[from]`, because
    /// the peeling now happens inside the provider impls where the
    /// distinction is made, not at the call site.
    #[error("{0}")]
    Metadata(#[from] brarr_core::MetadataError),

    /// Configuration/parse error (URL, etc.).
    #[error("invalid input: {0}")]
    InvalidInput(String),

    /// Requested entity does not exist (HTTP 404).
    #[error("not found: {0}")]
    NotFound(String),

    /// Refused because the global pause is on.
    ///
    /// Its own variant rather than an [`Self::InvalidInput`] because the
    /// request was not invalid — brarr is switched off, and telling the
    /// operator they sent something wrong is a different sentence with a
    /// different fix. `action` names what was refused: this repository
    /// has twice paid for one message covering several conditions (the
    /// scan badge's "nada encontrado", the numbering panel's sentence
    /// joined by "ou"), and a pause the operator has forgotten is the
    /// worst kind of defect precisely because nothing errors.
    ///
    /// The string is Portuguese because it reaches the screen verbatim
    /// through [`IntoResponse`].
    #[error("o brarr está pausado — retome em Configurações para {action}")]
    Paused {
        /// What was refused, in the infinitive: `"adotar arquivos"`.
        action: &'static str,
    },

    /// Generic I/O.
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

impl AppError {
    /// HTTP status code this error maps to.
    #[must_use]
    pub fn status_code(&self) -> StatusCode {
        match self {
            Self::NotFound(_) => StatusCode::NOT_FOUND,
            Self::InvalidInput(_) => StatusCode::BAD_REQUEST,
            // Conflict, not 503: the state is deliberate and the operator
            // is the one who clears it. 503 reads as "try again shortly",
            // which is the opposite of what a pause means.
            Self::Paused { .. } => StatusCode::CONFLICT,
            // A refused or missing credential is the operator's to fix,
            // and an unknown id is a 404 whoever asked. Everything else
            // is upstream being upstream.
            Self::Metadata(e) => match e {
                brarr_core::MetadataError::Unauthorized { .. } => StatusCode::UNAUTHORIZED,
                brarr_core::MetadataError::NotFound { .. } => StatusCode::NOT_FOUND,
                brarr_core::MetadataError::BadId(_)
                | brarr_core::MetadataError::UnknownOrdering { .. } => StatusCode::BAD_REQUEST,
                _ => StatusCode::BAD_GATEWAY,
            },
            _ => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let status = self.status_code();
        let body = self.to_string();
        tracing::warn!(target: "brarr_orchestrator", %status, error = %body, "request failed");
        (status, body).into_response()
    }
}

impl From<AppError> for tonic::Status {
    fn from(err: AppError) -> Self {
        let code = match err {
            AppError::NotFound(_) => tonic::Code::NotFound,
            AppError::InvalidInput(_) => tonic::Code::InvalidArgument,
            // Not `Unavailable`: that one tells a client to retry, and a
            // pause is cleared by a person, not by waiting.
            AppError::Paused { .. } => tonic::Code::FailedPrecondition,
            AppError::Metadata(_) => tonic::Code::Unavailable,
            _ => tonic::Code::Internal,
        };
        Self::new(code, err.to_string())
    }
}
