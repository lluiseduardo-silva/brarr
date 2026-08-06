//! `ignored_paths` — files the operator told brarr to stop offering.
//!
//! See `migrations/20260807120000_local_adoption.sql` for the schema
//! rationale. The short version: Sonarr and Radarr forget an ignored
//! file the moment the dialog closes, which is fine for a one-off import
//! and wrong for a downloads folder that only grows — the same junk
//! comes back on every import and has to be skipped by hand again.
//!
//! Ignoring is not deleting and not a judgement about content. A row
//! here means "do not offer this again"; removing the row undoes it, and
//! that is the whole lifecycle.

use std::collections::HashSet;

use sqlx::Row;
use time::OffsetDateTime;

use crate::{AppError, db::Pool};

/// One path the operator set aside.
#[derive(Debug, Clone)]
pub struct IgnoredPath {
    /// Absolute path, exactly as the scan reported it.
    pub path: String,
    /// When it was set aside.
    pub ignored_at: OffsetDateTime,
}

/// Set paths aside, most recent wins nothing — an already-ignored path
/// keeps its original timestamp.
///
/// Bulk because the importer's bulk bar is where this is used: the
/// operator selects the rows that will never be catalogued and dismisses
/// them in one action.
///
/// # Errors
///
/// Returns [`AppError::Database`] on SQL failure.
pub async fn ignore(pool: &Pool, paths: &[String]) -> Result<u64, AppError> {
    let now = OffsetDateTime::now_utc().unix_timestamp();
    let mut written = 0;
    for path in paths {
        // `ON CONFLICT DO NOTHING` rather than `INSERT OR IGNORE`, the
        // same choice `grabs::reserve` documents: the latter would also
        // swallow a CHECK or FK violation.
        let res = sqlx::query(
            "INSERT INTO ignored_paths (path, ignored_at) VALUES (?, ?) ON CONFLICT DO NOTHING",
        )
        .bind(path)
        .bind(now)
        .execute(pool)
        .await?;
        written += res.rows_affected();
    }
    Ok(written)
}

/// Offer a path again. `false` when it was not being ignored.
///
/// # Errors
///
/// Returns [`AppError::Database`] on SQL failure.
pub async fn unignore(pool: &Pool, path: &str) -> Result<bool, AppError> {
    let res = sqlx::query("DELETE FROM ignored_paths WHERE path = ?")
        .bind(path)
        .execute(pool)
        .await?;
    Ok(res.rows_affected() > 0)
}

/// Every ignored path, newest first — what the `Ignorados` filter shows.
///
/// # Errors
///
/// Returns [`AppError::Database`] on SQL failure.
pub async fn list(pool: &Pool) -> Result<Vec<IgnoredPath>, AppError> {
    let rows = sqlx::query("SELECT path, ignored_at FROM ignored_paths ORDER BY ignored_at DESC")
        .fetch_all(pool)
        .await?;
    rows.iter()
        .map(|r| {
            let ts: i64 = r.try_get("ignored_at")?;
            Ok(IgnoredPath {
                path: r.try_get("path")?,
                ignored_at: OffsetDateTime::from_unix_timestamp(ts).map_err(|e| {
                    AppError::InvalidInput(format!("invalid ignored_paths.ignored_at: {e}"))
                })?,
            })
        })
        .collect()
}

/// The same set, shaped for the one question the scan asks per file.
///
/// A folder with thousands of videos would otherwise mean thousands of
/// round trips to answer "is this one ignored?".
///
/// # Errors
///
/// Returns [`AppError::Database`] on SQL failure.
pub async fn paths(pool: &Pool) -> Result<HashSet<String>, AppError> {
    let rows = sqlx::query("SELECT path FROM ignored_paths")
        .fetch_all(pool)
        .await?;
    rows.iter().map(|r| Ok(r.try_get("path")?)).collect()
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "tests assert on happy paths"
)]
mod tests {
    use super::*;
    use crate::db::open_memory;

    #[tokio::test]
    async fn ignoring_is_idempotent_and_reversible() {
        let pool = open_memory().await.unwrap();
        let one = "/data/torrents/sample-extra.mkv".to_owned();
        let two = "/data/torrents/trailer.mkv".to_owned();

        assert_eq!(ignore(&pool, &[one.clone(), two.clone()]).await.unwrap(), 2);
        assert_eq!(
            ignore(&pool, std::slice::from_ref(&one)).await.unwrap(),
            0,
            "ignoring twice is not an error and writes nothing new"
        );
        assert_eq!(paths(&pool).await.unwrap().len(), 2);
        assert!(paths(&pool).await.unwrap().contains(&one));

        assert!(unignore(&pool, &one).await.unwrap());
        assert!(
            !unignore(&pool, &one).await.unwrap(),
            "un-ignoring what is not ignored changes nothing"
        );
        let left = list(&pool).await.unwrap();
        assert_eq!(left.len(), 1);
        assert_eq!(left[0].path, two);
    }
}
