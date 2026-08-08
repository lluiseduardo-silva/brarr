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
