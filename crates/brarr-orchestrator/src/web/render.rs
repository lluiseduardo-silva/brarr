//! Bridge between Askama templates and Axum responses.
//!
//! Askama 0.14 returns `Result<String, askama::Error>` from `render()`.
//! We wrap that in a tiny helper so handlers can write
//! `into_html(&MyTemplate { ... })` without sprinkling the same
//! status-code + content-type boilerplate everywhere.

use askama::Template;
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};

use crate::AppError;

/// Render `template` and return it as an HTML response (status 200).
///
/// # Errors
///
/// Propagates [`askama::Error`] through [`AppError::Template`].
pub fn html<T: Template>(template: &T) -> Result<Response, AppError> {
    let body = template.render()?;
    let mut res = (StatusCode::OK, body).into_response();
    res.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/html; charset=utf-8"),
    );
    Ok(res)
}
