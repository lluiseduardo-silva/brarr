//! Axum-based admin web UI.
//!
//! Server-side rendered with Askama templates and HTMX for partial
//! updates. O CSS é artesanal e versionado em `static/app.css` — não há
//! pipeline de frontend nem passo de build.

pub mod ip;
pub mod render;
pub mod routes;
pub mod templates;
pub mod torznab;
pub mod webhooks;

pub use routes::router;
pub use routes::serve;
