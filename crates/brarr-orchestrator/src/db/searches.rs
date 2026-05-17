//! Search rows. One row per user-initiated search; `result_count` is
//! updated when the orchestrator finishes persisting the per-tracker
//! decision rows.

use serde::{Deserialize, Serialize};
use sqlx::{Row, sqlite::SqliteRow};
use time::OffsetDateTime;
use uuid::Uuid;

use crate::{AppError, db::Pool};

/// Persisted search row.
#[derive(Debug, Clone)]
pub struct SearchRow {
    /// Stable UUID v4.
    pub id: Uuid,
    /// TMDb id used in the request (if any).
    pub tmdb_id: Option<u32>,
    /// IMDb id used in the request (if any). Reserved for future use.
    pub imdb_id: Option<String>,
    /// TVDB id used in the request (if any). Populated on the TV-axis
    /// path so an operator can query "did brarr run a search for this
    /// series episode?".
    pub tvdb_id: Option<u32>,
    /// TV season filter (when tvdb is set). `None` = series-wide.
    pub season: Option<u16>,
    /// TV episode filter (when tvdb + season are set). `None` =
    /// season-wide (also catches season packs).
    pub episode: Option<u16>,
    /// Submission timestamp (UTC).
    pub submitted_at: OffsetDateTime,
    /// How many decision rows resulted (after filtering rejects). Updated
    /// once the search pipeline finishes.
    pub result_count: u32,
    /// Free-form serialization of the original request.
    pub request_json: SearchRequestJson,
}

/// Serialized form of the original request, kept in the DB for replay
/// and audit. Matches the gRPC `SearchRequest` semantically but stays
/// independent of the proto-generated types.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SearchRequestJson {
    /// External media id (TMDb) if provided.
    #[serde(default)]
    pub tmdb_id: Option<u32>,
    /// IMDb id if provided.
    #[serde(default)]
    pub imdb_id: Option<String>,
    /// TVDB id if provided (TV axis).
    #[serde(default)]
    pub tvdb_id: Option<u32>,
    /// TV season filter.
    #[serde(default)]
    pub season: Option<u16>,
    /// TV episode filter.
    #[serde(default)]
    pub episode: Option<u16>,
}

/// Create a new search row in the `submitted` state (result_count = 0).
///
/// # Errors
///
/// Surfaces a [`sqlx::Error`] for SQL failures or a JSON error if the
/// request payload cannot be serialized (should never happen for our
/// types).
pub async fn create(pool: &Pool, request: SearchRequestJson) -> Result<SearchRow, AppError> {
    let id = Uuid::new_v4();
    let now = OffsetDateTime::now_utc();
    let request_json_text = serde_json::to_string(&request)?;

    sqlx::query(
        "INSERT INTO searches (id, tmdb_id, imdb_id, tvdb_id, season, episode, submitted_at, result_count, request_json) \
         VALUES (?, ?, ?, ?, ?, ?, ?, 0, ?)",
    )
    .bind(id.to_string())
    .bind(request.tmdb_id.map(i64::from))
    .bind(request.imdb_id.as_deref())
    .bind(request.tvdb_id.map(i64::from))
    .bind(request.season.map(i64::from))
    .bind(request.episode.map(i64::from))
    .bind(now.unix_timestamp())
    .bind(&request_json_text)
    .execute(pool)
    .await?;

    Ok(SearchRow {
        id,
        tmdb_id: request.tmdb_id,
        imdb_id: request.imdb_id.clone(),
        tvdb_id: request.tvdb_id,
        season: request.season,
        episode: request.episode,
        submitted_at: now,
        result_count: 0,
        request_json: request,
    })
}

/// Update `result_count` after the search pipeline finished persisting
/// its decision rows.
///
/// # Errors
///
/// Surfaces a [`sqlx::Error`] on SQL failure or [`AppError::NotFound`]
/// if no row with `id` exists.
pub async fn set_result_count(pool: &Pool, id: Uuid, count: u32) -> Result<(), AppError> {
    let res = sqlx::query("UPDATE searches SET result_count = ? WHERE id = ?")
        .bind(i64::from(count))
        .bind(id.to_string())
        .execute(pool)
        .await?;
    if res.rows_affected() == 0 {
        return Err(AppError::NotFound(format!("search {id}")));
    }
    Ok(())
}

/// Return the most recent `limit` searches (default 20, capped at 200).
///
/// # Errors
///
/// Surfaces a [`sqlx::Error`] on SQL failure.
pub async fn recent(pool: &Pool, limit: u32) -> Result<Vec<SearchRow>, AppError> {
    let limit = clamp_limit(limit);
    let rows = sqlx::query(
        "SELECT id, tmdb_id, imdb_id, tvdb_id, season, episode, submitted_at, result_count, request_json \
         FROM searches ORDER BY submitted_at DESC LIMIT ?",
    )
    .bind(i64::from(limit))
    .fetch_all(pool)
    .await?;
    rows.iter().map(row_to_search).collect()
}

/// Fetch a single search by id.
///
/// # Errors
///
/// Returns [`AppError::NotFound`] if no row matches.
pub async fn get_by_id(pool: &Pool, id: Uuid) -> Result<SearchRow, AppError> {
    let row_opt = sqlx::query(
        "SELECT id, tmdb_id, imdb_id, tvdb_id, season, episode, submitted_at, result_count, request_json \
         FROM searches WHERE id = ?",
    )
    .bind(id.to_string())
    .fetch_optional(pool)
    .await?;
    match row_opt {
        Some(row) => row_to_search(&row),
        None => Err(AppError::NotFound(format!("search {id}"))),
    }
}

fn clamp_limit(requested: u32) -> u32 {
    match requested {
        0 => 20,
        n if n > 200 => 200,
        n => n,
    }
}

fn row_to_search(row: &SqliteRow) -> Result<SearchRow, AppError> {
    let id_str: String = row.try_get("id")?;
    let id = Uuid::parse_str(&id_str)
        .map_err(|e| AppError::InvalidInput(format!("invalid uuid in searches.id: {e}")))?;
    let tmdb_db: Option<i64> = row.try_get("tmdb_id")?;
    let imdb: Option<String> = row.try_get("imdb_id")?;
    // TV axis columns are NULL on legacy rows; `try_get` returns Ok(None).
    let tvdb_raw: Option<i64> = row.try_get("tvdb_id").ok().flatten();
    let season_raw: Option<i64> = row.try_get("season").ok().flatten();
    let episode_raw: Option<i64> = row.try_get("episode").ok().flatten();
    let submitted_unix: i64 = row.try_get("submitted_at")?;
    let submitted_at = OffsetDateTime::from_unix_timestamp(submitted_unix)
        .map_err(|e| AppError::InvalidInput(format!("invalid timestamp: {e}")))?;
    let count_db: i64 = row.try_get("result_count")?;
    let request_json_text: String = row.try_get("request_json")?;
    let request_json: SearchRequestJson = serde_json::from_str(&request_json_text)?;
    Ok(SearchRow {
        id,
        tmdb_id: tmdb_db.and_then(|v| u32::try_from(v).ok()),
        imdb_id: imdb,
        tvdb_id: tvdb_raw.and_then(|v| u32::try_from(v).ok()),
        season: season_raw.and_then(|v| u16::try_from(v).ok()),
        episode: episode_raw.and_then(|v| u16::try_from(v).ok()),
        submitted_at,
        result_count: u32::try_from(count_db).unwrap_or(0),
        request_json,
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use crate::db::open_memory;

    #[tokio::test]
    async fn create_and_recent_roundtrips() {
        let pool = open_memory().await.unwrap();
        let req = SearchRequestJson {
            tmdb_id: Some(603),
            ..SearchRequestJson::default()
        };
        let row = create(&pool, req.clone()).await.unwrap();
        assert_eq!(row.tmdb_id, Some(603));
        assert_eq!(row.result_count, 0);

        let recent_rows = recent(&pool, 10).await.unwrap();
        assert_eq!(recent_rows.len(), 1);
        assert_eq!(recent_rows[0].id, row.id);
        assert_eq!(recent_rows[0].tmdb_id, Some(603));
    }

    #[tokio::test]
    async fn set_result_count_updates_row() {
        let pool = open_memory().await.unwrap();
        let row = create(
            &pool,
            SearchRequestJson {
                tmdb_id: Some(1),
                ..SearchRequestJson::default()
            },
        )
        .await
        .unwrap();
        set_result_count(&pool, row.id, 7).await.unwrap();
        let refreshed = get_by_id(&pool, row.id).await.unwrap();
        assert_eq!(refreshed.result_count, 7);
    }

    #[tokio::test]
    async fn set_result_count_missing_row_returns_not_found() {
        let pool = open_memory().await.unwrap();
        let err = set_result_count(&pool, Uuid::new_v4(), 1)
            .await
            .unwrap_err();
        assert!(matches!(err, AppError::NotFound(_)));
    }

    #[tokio::test]
    async fn recent_clamps_limit() {
        let pool = open_memory().await.unwrap();
        for i in 0..5_u32 {
            create(
                &pool,
                SearchRequestJson {
                    tmdb_id: Some(i),
                    ..SearchRequestJson::default()
                },
            )
            .await
            .unwrap();
        }
        // 0 → default 20 (we only have 5)
        assert_eq!(recent(&pool, 0).await.unwrap().len(), 5);
        // > 200 → 200 (we have 5)
        assert_eq!(recent(&pool, 9999).await.unwrap().len(), 5);
        // explicit smaller limit honored
        assert_eq!(recent(&pool, 2).await.unwrap().len(), 2);
    }

    #[tokio::test]
    async fn recent_orders_newest_first() {
        let pool = open_memory().await.unwrap();
        let first = create(
            &pool,
            SearchRequestJson {
                tmdb_id: Some(1),
                ..SearchRequestJson::default()
            },
        )
        .await
        .unwrap();
        // Ensure timestamps differ by at least one second.
        tokio::time::sleep(std::time::Duration::from_millis(1100)).await;
        let second = create(
            &pool,
            SearchRequestJson {
                tmdb_id: Some(2),
                ..SearchRequestJson::default()
            },
        )
        .await
        .unwrap();
        let rows = recent(&pool, 10).await.unwrap();
        assert_eq!(rows[0].id, second.id);
        assert_eq!(rows[1].id, first.id);
    }
}
