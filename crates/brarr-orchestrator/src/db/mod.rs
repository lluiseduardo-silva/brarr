//! Persistence layer. Thin typed wrappers around `sqlx::SqlitePool` —
//! one module per logical entity. Migrations are bundled via
//! `sqlx::migrate!` (embedded at compile time from `../migrations/`).
//!
//! All exposed functions return [`crate::AppError`] so callers can
//! propagate uniformly. Timestamps are `time::OffsetDateTime` on the
//! Rust side; sqlx encodes them to `INTEGER` (Unix seconds) per the
//! `STRICT` table schema.

pub mod arr_instances;
pub mod arr_root_mappings;
pub mod decisions;
pub mod download_clients;
pub mod grabs;
pub mod ignored_paths;
pub mod item_ids;
pub mod library;
pub mod maintenance;
pub mod media_server_mappings;
pub mod media_servers;
pub mod metrics;
pub mod path_mappings;
pub mod providers;
pub mod push_history;
pub mod quality_profiles;
pub mod root_folders;
pub mod scan_attempts;
pub mod searches;
#[cfg(test)]
pub(crate) mod seed;
pub mod settings;
pub mod sources;
pub mod webhook_events;

use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use sqlx::{ConnectOptions, Sqlite, SqlitePool, Transaction};
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
    // After the migrations and before anything can reference a source.
    // The seed lives in a migration; this reconciles it with the enum, so
    // a provider added in Rust after that migration shipped has its row
    // by the time the first write points a foreign key at it.
    sources::ensure(&pool).await?;

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
    sources::ensure(&pool).await?;
    Ok(pool)
}

/// Re-export so callers can keep `use crate::db::Pool` style imports.
pub type Pool = SqlitePool;

/// Begin a transaction that takes `SQLite`'s write lock at `BEGIN`.
///
/// **Every write transaction goes through here rather than
/// `Pool::begin`**, and the reason is a failure mode `busy_timeout` does
/// not cover.
///
/// sqlx emits a bare `BEGIN`, which is *deferred*: it acquires no lock
/// until the transaction's first statement. The write transactions in
/// [`library`] all read before they write — `upsert` looks the title up,
/// then inserts or updates — so they open as readers, pin a WAL
/// snapshot, and only then ask to become writers. When another
/// connection has committed inside that window SQLite cannot grant the
/// promotion, because the snapshot it read from is already stale, and it
/// answers `SQLITE_BUSY` **without invoking the busy handler**: waiting
/// could never succeed, and two connections doing this at once would
/// deadlock. So the 5s `busy_timeout` sqlx configures by default is not
/// consulted at all — the call fails in *zero* seconds with the
/// thoroughly misleading `database is locked`.
///
/// Measured in production, which is how this was found: the passive
/// \*arr sweep calls `upsert` once per title, 367 a pass, while the
/// scanner, the importer and the two metrics tables write single
/// statements across the other seven pooled connections. One pass
/// dropped 33 titles this way, each reported as a title the import
/// "could not take".
///
/// `BEGIN IMMEDIATE` takes the write lock up front, so there is no
/// promotion left to refuse and contention becomes an ordinary wait that
/// the busy handler *does* serve. It serialises writers, which costs
/// nothing that was not already being paid: SQLite has one writer either
/// way.
///
/// # Errors
///
/// Surfaces a [`sqlx::Error`] when no connection can be acquired, or
/// when the write lock is still held once `busy_timeout` has elapsed.
pub(crate) async fn begin_write(pool: &Pool) -> Result<Transaction<'static, Sqlite>, AppError> {
    Ok(pool.begin_with("BEGIN IMMEDIATE").await?)
}

/// `:memory:` URI helper that doesn't bind to a filesystem path.
#[must_use]
pub fn is_memory_path(path: &Path) -> bool {
    path.to_string_lossy() == ":memory:"
}

#[cfg(test)]
#[allow(clippy::expect_used, reason = "tests assert on happy paths")]
mod tests {
    use super::*;

    /// **The lock has to be held from `BEGIN`, not from the first write.**
    ///
    /// This is the only test in the crate that opens a database *file*.
    /// It has to: the defect is contention between two connections, and
    /// [`open_memory`] runs at `max_connections(1)`, where a second
    /// caller queues on the pool instead of on SQLite and the property
    /// under test cannot be observed at all.
    ///
    /// The observation is a timeout rather than an error because that is
    /// what the two behaviours actually differ on. Under a deferred
    /// `BEGIN` the second transaction opens *instantly*, having taken no
    /// lock; under `BEGIN IMMEDIATE` it blocks on the busy handler for
    /// the full `busy_timeout`. 300ms tells those apart with three
    /// orders of magnitude to spare on one side and sixteen on the
    /// other.
    #[tokio::test]
    async fn a_write_transaction_holds_the_lock_from_begin() {
        let dir = std::env::temp_dir().join(format!("brarr-beginwrite-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let path = dir.join("brarr.db");
        let pool = open(&path.to_string_lossy()).await.expect("open");

        let held = begin_write(&pool).await.expect("first write transaction");

        let second =
            tokio::time::timeout(std::time::Duration::from_millis(300), begin_write(&pool)).await;
        let started = second.is_ok();

        // `second` still owns whatever it was handed. Dropping it before
        // `close` is not tidiness: `close` waits for every connection to
        // come back, so a live transaction here hangs the test forever
        // instead of failing it.
        drop(second);
        drop(held);
        pool.close().await;
        let _ = std::fs::remove_dir_all(&dir);

        assert!(
            !started,
            "a second write transaction opened while the first was still held \
             — the write lock is not being taken at BEGIN, so a read-then-write \
             transaction will fail its promotion instead of waiting"
        );
    }
}
