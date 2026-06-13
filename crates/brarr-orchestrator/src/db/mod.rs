//! Persistence layer. Thin typed wrappers around `sqlx::SqlitePool` —
//! one module per logical entity. Migrations are bundled via
//! `sqlx::migrate!` (embedded at compile time from `../migrations/`).
//!
//! All exposed functions return [`crate::AppError`] so callers can
//! propagate uniformly. Timestamps are `time::OffsetDateTime` on the
//! Rust side; sqlx encodes them to `INTEGER` (Unix seconds) per the
//! `STRICT` table schema.

pub mod arr_instances;
pub mod decisions;
pub mod maintenance;
pub mod providers;
pub mod push_history;
pub mod quality_profiles;
pub mod searches;
pub mod settings;
pub mod webhook_events;

use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use sqlx::{ConnectOptions, SqlitePool};
use std::path::Path;
use std::str::FromStr;

use crate::AppError;

/// Open (and create-on-missing) a SQLite database at `path`, run pending
/// migrations, and return a connection pool.
///
/// `path` may also be `:memory:` for in-process tests.
///
/// # Errors
///
/// Surfaces a [`sqlx::Error`] for connection issues and a
/// [`sqlx::migrate::MigrateError`] if migrations fail to apply.
pub async fn open(path: &str) -> Result<SqlitePool, AppError> {
    let mut opts = SqliteConnectOptions::from_str(&format!("sqlite://{path}"))?
        .create_if_missing(true)
        .foreign_keys(true)
        .journal_mode(sqlx::sqlite::SqliteJournalMode::Wal)
        // Let the maintenance task's `PRAGMA incremental_vacuum` return
        // freed pages to the OS. Takes effect immediately on fresh DBs;
        // pre-existing files convert on their next full `VACUUM`.
        .auto_vacuum(sqlx::sqlite::SqliteAutoVacuum::Incremental);
    // Quiet sqlx's verbose per-query logging at INFO — we keep it at
    // DEBUG so `RUST_LOG=brarr_orchestrator=debug` still gives visibility.
    opts = opts.log_statements(tracing::log::LevelFilter::Debug);

    let pool = SqlitePoolOptions::new()
        .max_connections(8)
        .connect_with(opts)
        .await?;

    sqlx::migrate!("./migrations").run(&pool).await?;

    Ok(pool)
}

/// Convenience constructor for the in-memory pool used by integration
/// tests. Each call returns a fresh, isolated database.
///
/// # Errors
///
/// Same as [`open`].
pub async fn open_memory() -> Result<SqlitePool, AppError> {
    let opts = SqliteConnectOptions::from_str("sqlite::memory:")?
        .create_if_missing(true)
        .foreign_keys(true);
    let pool = SqlitePoolOptions::new()
        .max_connections(1) // shared in-memory DB → one connection.
        .connect_with(opts)
        .await?;
    sqlx::migrate!("./migrations").run(&pool).await?;
    Ok(pool)
}

/// Re-export so callers can keep `use crate::db::Pool` style imports.
pub type Pool = SqlitePool;

/// `:memory:` URI helper that doesn't bind to a filesystem path.
#[must_use]
pub fn is_memory_path(path: &Path) -> bool {
    path.to_string_lossy() == ":memory:"
}
