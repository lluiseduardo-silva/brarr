//! Axum-based admin web UI.
//!
//! Server-side rendered with Askama templates and HTMX for partial
//! updates. Tailwind ships via CDN per the project spec — no frontend
//! build pipeline.

pub mod render;
pub mod routes;
pub mod templates;

pub use routes::router;
pub use routes::serve;
