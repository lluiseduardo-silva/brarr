//! Database maintenance — history retention pruning and space reclaim.
//!
//! The orchestrator's poller re-evaluates every wanted item on each
//! cycle and persists a `decisions` row per release, so the table grows
//! unbounded without a retention policy. These helpers delete rows older
//! than a configured window and reclaim the freed pages.
//!
//! Two invariants the prune respects:
//! - **Pushed decisions survive.** Any `decisions` row referenced by a
//!   `push_history` entry is kept regardless of age, so the `*arr`
//!   download proxy and the `already_tried_release` dedup guard keep
//!   working. (It also avoids the `push_history` `ON DELETE CASCADE`
//!   ever firing.)
//! - **Searches outlive their decisions.** A `searches` row is only
//!   pruned once it has no remaining `decisions` children, so deep links
//!   to a still-relevant search detail page don't 404.

use time::OffsetDateTime;

use crate::{AppError, db::Pool};

/// Seconds in a day — retention windows are expressed in whole days.
const SECS_PER_DAY: i64 = 86_400;

/// Row counts removed by a single [`run_prune`] pass.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MaintenanceOutcome {
    /// `decisions` rows deleted (excludes pushed rows, which are kept).
    pub decisions_deleted: u64,
    /// `searches` rows deleted (only those left with no decisions).
    pub searches_deleted: u64,
}

impl MaintenanceOutcome {
    /// `true` when nothing was deleted — handy for skipping a `VACUUM`
    /// or a noisy log line.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.decisions_deleted == 0 && self.searches_deleted == 0
    }
}

/// Prune `decisions` and `searches` older than `retention_days`.
///
/// `retention_days == 0` disables pruning (keep forever) and returns a
/// zeroed [`MaintenanceOutcome`] without touching the database.
///
/// # Errors
///
/// Returns [`AppError::Database`] on SQL failure.
pub async fn run_prune(pool: &Pool, retention_days: u32) -> Result<MaintenanceOutcome, AppError> {
    if retention_days == 0 {
        return Ok(MaintenanceOutcome::default());
    }
    let cutoff =
        OffsetDateTime::now_utc().unix_timestamp() - i64::from(retention_days) * SECS_PER_DAY;
    let decisions_deleted = prune_decisions(pool, cutoff).await?;
    let searches_deleted = prune_searches(pool, cutoff).await?;
    Ok(MaintenanceOutcome {
        decisions_deleted,
        searches_deleted,
    })
}

/// Delete `decisions` older than `cutoff_unix` that were never pushed.
///
/// # Errors
///
/// Returns [`AppError::Database`] on SQL failure.
pub async fn prune_decisions(pool: &Pool, cutoff_unix: i64) -> Result<u64, AppError> {
    let res = sqlx::query(
        "DELETE FROM decisions \
         WHERE decided_at < ? \
           AND NOT EXISTS (SELECT 1 FROM push_history ph WHERE ph.decision_id = decisions.id)",
    )
    .bind(cutoff_unix)
    .execute(pool)
    .await?;
    Ok(res.rows_affected())
}

/// Delete `searches` older than `cutoff_unix` that have no surviving
/// `decisions` children.
///
/// # Errors
///
/// Returns [`AppError::Database`] on SQL failure.
pub async fn prune_searches(pool: &Pool, cutoff_unix: i64) -> Result<u64, AppError> {
    let res = sqlx::query(
        "DELETE FROM searches \
         WHERE submitted_at < ? \
           AND NOT EXISTS (SELECT 1 FROM decisions d WHERE d.search_id = searches.id)",
    )
    .bind(cutoff_unix)
    .execute(pool)
    .await?;
    Ok(res.rows_affected())
}

/// Force a WAL checkpoint that truncates the `-wal` file, keeping it
/// from growing without bound after large deletes.
///
/// # Errors
///
/// Returns [`AppError::Database`] on SQL failure.
pub async fn checkpoint_wal(pool: &Pool) -> Result<(), AppError> {
    sqlx::query("PRAGMA wal_checkpoint(TRUNCATE)")
        .execute(pool)
        .await?;
    Ok(())
}

/// Return free pages to the OS. Only does work when the database was
/// created (or `VACUUM`ed) with `auto_vacuum = INCREMENTAL`; otherwise
/// it's a harmless no-op.
///
/// # Errors
///
/// Returns [`AppError::Database`] on SQL failure.
pub async fn incremental_vacuum(pool: &Pool) -> Result<(), AppError> {
    sqlx::query("PRAGMA incremental_vacuum")
        .execute(pool)
        .await?;
    Ok(())
}

/// Run a full `VACUUM` — rewrites the database file, reclaiming all
/// free space. Expensive (exclusive lock + ~2× disk) but the only way
/// to shrink a file that predates `auto_vacuum`. Exposed for the
/// on-demand "Compactar" action, not the periodic task.
///
/// # Errors
///
/// Returns [`AppError::Database`] on SQL failure.
pub async fn full_vacuum(pool: &Pool) -> Result<(), AppError> {
    sqlx::query("VACUUM").execute(pool).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use crate::db::{Pool, open_memory};
    use time::OffsetDateTime;
    use uuid::Uuid;

    /// Insert a search row directly (bypassing the typed helper so we can
    /// backdate `submitted_at`).
    async fn insert_search(pool: &Pool, id: Uuid, submitted_at: i64) {
        sqlx::query(
            "INSERT INTO searches (id, tmdb_id, imdb_id, submitted_at, result_count, request_json) \
             VALUES (?, NULL, NULL, ?, 0, '{}')",
        )
        .bind(id.to_string())
        .bind(submitted_at)
        .execute(pool)
        .await
        .unwrap();
    }

    /// Insert a minimal decision row with a chosen `decided_at`.
    async fn insert_decision(pool: &Pool, id: Uuid, search_id: Uuid, decided_at: i64) {
        sqlx::query(
            "INSERT INTO decisions \
               (id, search_id, provider_id, provider_name, release_name, release_id_remote, \
                score, rejected, decided_at) \
             VALUES (?, ?, NULL, 'p', 'r', 0, 0, 0, ?)",
        )
        .bind(id.to_string())
        .bind(search_id.to_string())
        .bind(decided_at)
        .execute(pool)
        .await
        .unwrap();
    }

    async fn count(pool: &Pool, table: &str) -> i64 {
        let sql = format!("SELECT COUNT(*) AS c FROM {table}");
        let row: (i64,) = sqlx::query_as(&sql).fetch_one(pool).await.unwrap();
        row.0
    }

    fn days_ago(days: i64) -> i64 {
        OffsetDateTime::now_utc().unix_timestamp() - days * SECS_PER_DAY
    }

    #[tokio::test]
    async fn prune_removes_old_but_keeps_recent_decisions() {
        let pool = open_memory().await.unwrap();
        let search = Uuid::new_v4();
        insert_search(&pool, search, days_ago(30)).await;
        insert_decision(&pool, Uuid::new_v4(), search, days_ago(30)).await; // old
        insert_decision(&pool, Uuid::new_v4(), search, days_ago(1)).await; // recent

        let out = run_prune(&pool, 7).await.unwrap();
        assert_eq!(out.decisions_deleted, 1);
        assert_eq!(count(&pool, "decisions").await, 1);
        // search still has a recent child → kept
        assert_eq!(out.searches_deleted, 0);
        assert_eq!(count(&pool, "searches").await, 1);
    }

    #[tokio::test]
    async fn prune_keeps_pushed_decisions_regardless_of_age() {
        let pool = open_memory().await.unwrap();
        let search = Uuid::new_v4();
        insert_search(&pool, search, days_ago(30)).await;
        let pushed = Uuid::new_v4();
        insert_decision(&pool, pushed, search, days_ago(30)).await;
        // Reference it from push_history so it must survive the prune.
        sqlx::query(
            "INSERT INTO push_history \
               (id, decision_id, arr_instance_id, arr_instance_name, arr_kind, pushed_at, status) \
             VALUES (?, ?, NULL, 'radarr-1', 'radarr', ?, 'ok')",
        )
        .bind(Uuid::new_v4().to_string())
        .bind(pushed.to_string())
        .bind(days_ago(30))
        .execute(&pool)
        .await
        .unwrap();

        let out = run_prune(&pool, 7).await.unwrap();
        assert_eq!(out.decisions_deleted, 0);
        assert_eq!(count(&pool, "decisions").await, 1);
    }

    #[tokio::test]
    async fn prune_removes_orphan_searches() {
        let pool = open_memory().await.unwrap();
        let search = Uuid::new_v4();
        insert_search(&pool, search, days_ago(30)).await; // no decisions at all
        let out = run_prune(&pool, 7).await.unwrap();
        assert_eq!(out.searches_deleted, 1);
        assert_eq!(count(&pool, "searches").await, 0);
    }

    #[tokio::test]
    async fn retention_zero_is_a_noop() {
        let pool = open_memory().await.unwrap();
        let search = Uuid::new_v4();
        insert_search(&pool, search, days_ago(999)).await;
        insert_decision(&pool, Uuid::new_v4(), search, days_ago(999)).await;
        let out = run_prune(&pool, 0).await.unwrap();
        assert!(out.is_empty());
        assert_eq!(count(&pool, "decisions").await, 1);
        assert_eq!(count(&pool, "searches").await, 1);
    }

    #[tokio::test]
    async fn pragmas_execute_cleanly() {
        let pool = open_memory().await.unwrap();
        checkpoint_wal(&pool).await.unwrap();
        incremental_vacuum(&pool).await.unwrap();
        full_vacuum(&pool).await.unwrap();
    }
}
