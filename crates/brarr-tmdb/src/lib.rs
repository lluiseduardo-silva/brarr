//! `brarr-tmdb` — async client for The Movie Database v3 API.
//!
//! Supplies the metadata for brarr's own library: titles, synopses,
//! posters, external ids, release dates and the season/episode tree.
//!
//! ## Why a hand-rolled client
//!
//! The Rust ecosystem has no maintained TMDB crate — the leading one has
//! sat in alpha for over a year and the runner-up is generated code. This
//! crate is modelled on `brarr-tracker-unit3d` instead: `reqwest`,
//! `thiserror`, tolerant deserialisation, and the same retry policy.
//!
//! ## Authentication
//!
//! Uses the **v4 read access token** as `Authorization: Bearer <token>`.
//! The v3 API key is a different string from the same account page and
//! is sent as a query parameter; passing it as a bearer token yields a
//! 401. Both are free for personal use and need no approval.
//!
//! ## Two upstream behaviours worth knowing
//!
//! - **No automatic language fallback.** `language=pt-BR` returns an
//!   empty `overview` when no Portuguese translation exists rather than
//!   falling back. The client appends `translations` and resolves
//!   pt-BR → pt-PT → en-US in code.
//! - **`tvdb_id` exists for series only.** Movies have no TVDB mapping,
//!   so [`MovieDetails`] deliberately has no such field.
//!
//! ## Terms of use
//!
//! TMDB requires attribution wherever their data is shown — use
//! [`ATTRIBUTION`] verbatim rather than paraphrasing it, alongside their
//! official logo, and keep it less prominent than the brarr mark. Their
//! terms also forbid caching metadata for more than six months and
//! forbid using TMDB as an image host, which is why brarr stores only
//! `poster_path` and loads images straight from their CDN.

mod client;
mod dto;
mod error;
mod model;
mod retry;

pub use client::TmdbClient;
pub use error::TmdbError;
pub use model::{
    Episode, FindResults, MovieDetails, MovieSummary, SeasonDetails, SeasonSummary, TvDetails,
    TvSummary,
};
pub use retry::RetryConfig;

/// Attribution string required by the TMDB terms of use. Rendered
/// verbatim on the "Sobre" page — the shorter wording that circulates in
/// their FAQ is not the one the terms specify.
pub const ATTRIBUTION: &str = "This product uses TMDB and the TMDB APIs but is not endorsed, certified, or otherwise approved by TMDB.";

/// Base URL of the TMDB image CDN. Callers append a size segment
/// (`w92`, `w185`, `w342`, `w780`, `original`) and the `poster_path`
/// straight from a record.
pub const IMAGE_BASE_URL: &str = "https://image.tmdb.org/t/p/";

/// Build a full poster / backdrop URL from a stored path.
///
/// Returns `None` for an absent path so templates can branch on it.
///
/// ```
/// use brarr_tmdb::image_url;
/// assert_eq!(
///     image_url(Some("/abc.jpg"), "w185").as_deref(),
///     Some("https://image.tmdb.org/t/p/w185/abc.jpg")
/// );
/// assert_eq!(image_url(None, "w185"), None);
/// ```
#[must_use]
pub fn image_url(path: Option<&str>, size: &str) -> Option<String> {
    let path = path?;
    if path.is_empty() {
        return None;
    }
    // TMDB paths already start with '/', so joining naively would double it.
    let trimmed = path.strip_prefix('/').unwrap_or(path);
    Some(format!("{IMAGE_BASE_URL}{size}/{trimmed}"))
}
