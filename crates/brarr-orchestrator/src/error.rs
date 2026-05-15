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

    /// Configuration/parse error (URL, etc.).
    #[error("invalid input: {0}")]
    InvalidInput(String),

    /// Requested entity does not exist (HTTP 404).
    #[error("not found: {0}")]
    NotFound(String),

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
            _ => tonic::Code::Internal,
        };
        Self::new(code, err.to_string())
    }
}
