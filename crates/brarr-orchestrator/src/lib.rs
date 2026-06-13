//! `brarr-orchestrator` — long-running service that exposes the brarr
//! pipeline over both gRPC (consumed by the CLI and external integrations)
//! and a server-rendered admin web UI (Axum + Askama + HTMX, Tailwind via
//! CDN per the project spec).
//!
//! ## Layering
//!
//! ```text
//! ┌────────────────────────┐ ┌────────────────────────┐
//! │       grpc::server     │ │       web::router      │
//! │   (tonic Brarr svc)    │ │  (axum SSR + htmx UI)  │
//! └──────────┬─────────────┘ └────────────┬───────────┘
//!            │                            │
//!            └──────────────┬─────────────┘
//!                           ▼
//!                ┌──────────────────────┐
//!                │     AppState         │
//!                │  • Db pool           │
//!                │  • Engine (rules)    │
//!                │  • search runner     │
//!                └──────────┬───────────┘
//!                           ▼
//!     ┌──────────────────────┬─────────────────────────┐
//!     │  brarr_tracker_unit3d│  brarr_decision_service │
//!     └──────────────────────┴─────────────────────────┘
//! ```
//!
//! ## Persistence
//!
//! SQLite via `sqlx`. Schema lives in `migrations/` and is applied on
//! startup with `sqlx::migrate!`. Three tables:
//! - `trackers` — configured UNIT3D endpoints (replaces the TOML-driven
//!   tracker list from `brarr-cli` for orchestrator-managed deployments).
//! - `searches` — one row per user-initiated search.
//! - `decisions` — per-release outcome rows (score, tags, rejected flag,
//!   matched rule names).
//!
//! See [`db`] for the typed access layer.
//!
//! ## Why both gRPC and HTTP?
//!
//! gRPC is the **machine API** (CLI, future automations). The HTTP server
//! is the **human API** (admin UI). They share an [`AppState`] so a
//! search triggered via HTMX form lands in the same SQLite rows a gRPC
//! `Search` call would produce.

#![allow(
    clippy::module_name_repetitions,
    clippy::doc_markdown,
    reason = "TMDb/IMDb/SQLite appear too often in user-facing module docs to be worth backticking each time"
)]

pub mod auth;
pub mod db;
pub mod error;
pub mod grpc;
pub mod maintenance;
pub mod poll;
pub mod push;
pub mod search;
pub mod state;
pub mod web;

pub use auth::{AuthConfig, BypassConfig, TrustedPeers};
pub use error::AppError;
pub use state::AppState;
