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
