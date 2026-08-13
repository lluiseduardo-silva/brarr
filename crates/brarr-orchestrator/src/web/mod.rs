//! Axum-based admin web UI.
//!
//! Server-side rendered with Askama templates and HTMX for partial
//! updates. CSS is hand-authored and committed at `static/app.css` —
//! there is no frontend pipeline and no build step.

pub mod ip;
pub mod render;
pub mod routes;
pub mod templates;
pub mod torznab;
pub mod webhooks;

pub use routes::router;
pub use routes::serve;

/// Cache-busting stamp appended to every `/static` URL in `base.html`.
///
/// The crate version, because that is exactly what changes when the
/// assets change: a release rewrites `app.css` and the `<link>` becomes
/// a URL the browser has never seen. Together with the `no-cache`
/// header on the static tree it makes a stale stylesheet impossible —
/// the failure it replaces was silent, since fresh markup against old
/// CSS just renders every new class as a no-op.
pub const ASSET_VERSION: &str = env!("CARGO_PKG_VERSION");

/// TMDB's required attribution, re-exported for `base.html`.
///
/// **A licence condition, not a courtesy**, and it went unrendered for
/// as long as `brarr-tmdb` has existed: the constant was there, `pub`,
/// and no template read it. Verbatim on purpose — the shorter FAQ
/// wording is not the one the terms require.
pub const TMDB_ATTRIBUTION: &str = brarr_tmdb::ATTRIBUTION;

/// TheTVDB's required attribution.
///
/// The free tier — what a project under $50k/year uses — is conditioned
/// on it: "attribution with a direct link to TheTVDB.com must be
/// displayed to end users viewing metadata from our API". The allowance
/// for an about or readme page covers command line products and
/// libraries; brarr has a UI.
pub const TVDB_ATTRIBUTION: &str = brarr_tvdb::ATTRIBUTION;

/// The direct link TheTVDB's attribution must carry.
pub const TVDB_ATTRIBUTION_URL: &str = brarr_tvdb::ATTRIBUTION_URL;
