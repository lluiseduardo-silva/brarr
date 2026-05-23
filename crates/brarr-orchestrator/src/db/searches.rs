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

/// Filter expression for [`filter`] / [`count_filtered`]. Every field
/// is optional — absent fields don't constrain the result set.
///
/// Note: `limit` / `offset` only apply to [`filter`]; [`count_filtered`]
/// ignores them so the page counter reflects the unbounded total.
#[derive(Debug, Clone, Default)]
pub struct FilterParams {
    /// Match `searches.tmdb_id` exactly.
    pub tmdb_id: Option<u32>,
    /// Match `searches.imdb_id` exactly (string comparison — the
    /// stored form already includes the `tt` prefix or not as the
    /// caller submitted it).
    pub imdb_id: Option<String>,
    /// Match `searches.tvdb_id` exactly.
    pub tvdb_id: Option<u32>,
    /// Match `searches.season` exactly. Only meaningful when combined
    /// with `tvdb_id`.
    pub season: Option<u16>,
    /// Match `searches.episode` exactly. Only meaningful when combined
    /// with `tvdb_id` + `season`.
    pub episode: Option<u16>,
    /// Lower bound on `submitted_at` (Unix seconds, inclusive).
    pub from_unix: Option<i64>,
    /// Upper bound on `submitted_at` (Unix seconds, inclusive).
    pub to_unix: Option<i64>,
    /// `Some(true)` keeps only searches that produced at least one
    /// non-rejected decision. `Some(false)` keeps only searches that
    /// produced no kept decisions (zero results or every release
    /// rejected). `None` matches both.
    pub has_kept_decision: Option<bool>,
    /// Maximum rows to return (clamped 1..=200; 0 → 50 default).
    pub limit: u32,
    /// Row offset for pagination. Unbounded by design — SQLite handles
    /// large offsets fine and the UI gates page size.
    pub offset: u32,
}

/// Return the filtered + paginated subset of searches matching `p`.
///
/// Sort order is always `submitted_at DESC` (newest first); a future
/// "sort by result count" would require an additional field.
///
/// # Performance note
///
/// No indexes exist on `tmdb_id` / `imdb_id` / `tvdb_id` today. The
/// orchestrator's working set is small (thousands of searches at
/// most), so SQLite scans the table in microseconds. Add partial
/// indexes if EXPLAIN QUERY PLAN ever shows a hot spot.
///
/// # Errors
///
/// Surfaces a [`sqlx::Error`] on SQL failure.
pub async fn filter(pool: &Pool, p: FilterParams) -> Result<Vec<SearchRow>, AppError> {
    let limit = clamp_limit(p.limit);
    let offset = p.offset;
    let mut qb = sqlx::QueryBuilder::<sqlx::Sqlite>::new(
        "SELECT id, tmdb_id, imdb_id, tvdb_id, season, episode, submitted_at, result_count, request_json \
         FROM searches",
    );
    push_filters(&mut qb, &p);
    qb.push(" ORDER BY submitted_at DESC LIMIT ")
        .push_bind(i64::from(limit))
        .push(" OFFSET ")
        .push_bind(i64::from(offset));
    let rows = qb.build().fetch_all(pool).await?;
    rows.iter().map(row_to_search).collect()
}

/// Total number of rows matching `p`, ignoring `limit` / `offset`.
/// Used by the UI to compute total pages without a second roundtrip
/// per page.
///
/// # Errors
///
/// Surfaces a [`sqlx::Error`] on SQL failure.
pub async fn count_filtered(pool: &Pool, p: &FilterParams) -> Result<u64, AppError> {
    let mut qb = sqlx::QueryBuilder::<sqlx::Sqlite>::new("SELECT COUNT(*) AS n FROM searches");
    push_filters(&mut qb, p);
    let row = qb.build().fetch_one(pool).await?;
    let n: i64 = row.try_get("n")?;
    Ok(u64::try_from(n).unwrap_or(0))
}

/// Append the `WHERE ...` clauses for `p`. Lives between `filter` and
/// `count_filtered` so the two stay in sync — a filter that's
/// honored by one but not the other is an immediate UI bug.
fn push_filters(qb: &mut sqlx::QueryBuilder<'_, sqlx::Sqlite>, p: &FilterParams) {
    let mut where_pushed = false;
    let mut separator = |qb: &mut sqlx::QueryBuilder<'_, sqlx::Sqlite>| {
        if where_pushed {
            qb.push(" AND ");
        } else {
            qb.push(" WHERE ");
            where_pushed = true;
        }
    };
    if let Some(v) = p.tmdb_id {
        separator(qb);
        qb.push("tmdb_id = ").push_bind(i64::from(v));
    }
    if let Some(v) = &p.imdb_id {
        separator(qb);
        qb.push("imdb_id = ").push_bind(v.clone());
    }
    if let Some(v) = p.tvdb_id {
        separator(qb);
        qb.push("tvdb_id = ").push_bind(i64::from(v));
    }
    if let Some(v) = p.season {
        separator(qb);
        qb.push("season = ").push_bind(i64::from(v));
    }
    if let Some(v) = p.episode {
        separator(qb);
        qb.push("episode = ").push_bind(i64::from(v));
    }
    if let Some(v) = p.from_unix {
        separator(qb);
        qb.push("submitted_at >= ").push_bind(v);
    }
    if let Some(v) = p.to_unix {
        separator(qb);
        qb.push("submitted_at <= ").push_bind(v);
    }
    if let Some(v) = p.has_kept_decision {
        separator(qb);
        if v {
            qb.push(
                "EXISTS (SELECT 1 FROM decisions WHERE decisions.search_id = searches.id AND decisions.rejected = 0)",
            );
        } else {
            qb.push(
                "NOT EXISTS (SELECT 1 FROM decisions WHERE decisions.search_id = searches.id AND decisions.rejected = 0)",
            );
        }
    }
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

    // ---- filter() tests ------------------------------------------

    async fn seed_filter_fixture(pool: &Pool) -> Vec<Uuid> {
        // Three searches: one TMDb movie, one IMDb movie, one TV tuple.
        let mut ids = Vec::new();
        for req in [
            SearchRequestJson {
                tmdb_id: Some(603),
                ..SearchRequestJson::default()
            },
            SearchRequestJson {
                imdb_id: Some("tt0133093".to_string()),
                ..SearchRequestJson::default()
            },
            SearchRequestJson {
                tvdb_id: Some(81189),
                season: Some(1),
                episode: Some(1),
                ..SearchRequestJson::default()
            },
        ] {
            let row = create(pool, req).await.unwrap();
            ids.push(row.id);
            // Stagger timestamps so ORDER BY submitted_at DESC is
            // deterministic (matters for the pagination tests).
            tokio::time::sleep(std::time::Duration::from_millis(1100)).await;
        }
        ids
    }

    #[tokio::test]
    async fn filter_with_no_constraints_returns_all() {
        let pool = open_memory().await.unwrap();
        seed_filter_fixture(&pool).await;
        let rows = filter(
            &pool,
            FilterParams {
                limit: 50,
                ..FilterParams::default()
            },
        )
        .await
        .unwrap();
        assert_eq!(rows.len(), 3);
    }

    #[tokio::test]
    async fn filter_by_tmdb_isolates_movie_search() {
        let pool = open_memory().await.unwrap();
        seed_filter_fixture(&pool).await;
        let rows = filter(
            &pool,
            FilterParams {
                tmdb_id: Some(603),
                limit: 50,
                ..FilterParams::default()
            },
        )
        .await
        .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].tmdb_id, Some(603));
    }

    #[tokio::test]
    async fn filter_by_imdb_isolates_imdb_search() {
        let pool = open_memory().await.unwrap();
        seed_filter_fixture(&pool).await;
        let rows = filter(
            &pool,
            FilterParams {
                imdb_id: Some("tt0133093".to_string()),
                limit: 50,
                ..FilterParams::default()
            },
        )
        .await
        .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].imdb_id.as_deref(), Some("tt0133093"));
    }

    #[tokio::test]
    async fn filter_by_tvdb_season_episode_combines() {
        let pool = open_memory().await.unwrap();
        seed_filter_fixture(&pool).await;
        let rows = filter(
            &pool,
            FilterParams {
                tvdb_id: Some(81189),
                season: Some(1),
                episode: Some(1),
                limit: 50,
                ..FilterParams::default()
            },
        )
        .await
        .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].tvdb_id, Some(81189));
        assert_eq!(rows[0].season, Some(1));
        assert_eq!(rows[0].episode, Some(1));
    }

    #[tokio::test]
    async fn filter_date_window_excludes_outside() {
        let pool = open_memory().await.unwrap();
        seed_filter_fixture(&pool).await;
        // Far-future bounds should match nothing.
        let rows = filter(
            &pool,
            FilterParams {
                from_unix: Some(99_999_999_999),
                limit: 50,
                ..FilterParams::default()
            },
        )
        .await
        .unwrap();
        assert!(rows.is_empty());
        // Far-past upper bound also matches nothing.
        let rows = filter(
            &pool,
            FilterParams {
                to_unix: Some(0),
                limit: 50,
                ..FilterParams::default()
            },
        )
        .await
        .unwrap();
        assert!(rows.is_empty());
    }

    #[tokio::test]
    async fn filter_pagination_limit_and_offset() {
        let pool = open_memory().await.unwrap();
        seed_filter_fixture(&pool).await;
        let page1 = filter(
            &pool,
            FilterParams {
                limit: 2,
                offset: 0,
                ..FilterParams::default()
            },
        )
        .await
        .unwrap();
        assert_eq!(page1.len(), 2);
        let page2 = filter(
            &pool,
            FilterParams {
                limit: 2,
                offset: 2,
                ..FilterParams::default()
            },
        )
        .await
        .unwrap();
        assert_eq!(page2.len(), 1);
        // No overlap between pages.
        assert!(page1.iter().all(|r| r.id != page2[0].id));
    }

    #[tokio::test]
    async fn count_filtered_matches_filter_row_count_when_unpaginated() {
        let pool = open_memory().await.unwrap();
        seed_filter_fixture(&pool).await;
        let p = FilterParams {
            tvdb_id: Some(81189),
            ..FilterParams::default()
        };
        assert_eq!(count_filtered(&pool, &p).await.unwrap(), 1);
        let p_all = FilterParams::default();
        assert_eq!(count_filtered(&pool, &p_all).await.unwrap(), 3);
    }

    #[tokio::test]
    #[allow(
        clippy::too_many_lines,
        reason = "two parallel insertion blocks keep the kept-vs-rejected fixture readable inline"
    )]
    async fn filter_has_kept_decision_distinguishes_with_and_without() {
        use crate::db::decisions::{self, DecisionInsert};
        use brarr_core::{ReleaseKind, Resolution};

        let pool = open_memory().await.unwrap();
        let with_kept = create(
            &pool,
            SearchRequestJson {
                tmdb_id: Some(101),
                ..SearchRequestJson::default()
            },
        )
        .await
        .unwrap();
        let without = create(
            &pool,
            SearchRequestJson {
                tmdb_id: Some(102),
                ..SearchRequestJson::default()
            },
        )
        .await
        .unwrap();
        // First search gets a kept decision.
        decisions::insert(
            &pool,
            DecisionInsert {
                search_id: with_kept.id,
                provider_id: None,
                provider_name: "p".to_string(),
                release_name: "r".to_string(),
                release_id_remote: 1,
                score: 800,
                rejected: false,
                tags: Vec::new(),
                matched_rules: Vec::new(),
                seeders: 1,
                leechers: 0,
                size_bytes: 1,
                resolution: Resolution::P1080,
                kind: ReleaseKind::WebDl,
                download_url: None,
                details_url: None,
                provider_kind: Some("unit3d".to_string()),
                published_at: None,
                audio_languages: Vec::new(),
                subtitle_languages: Vec::new(),
                profile_scores: std::collections::HashMap::new(),
            },
        )
        .await
        .unwrap();
        // Second gets a rejected decision (so the EXISTS branch
        // sees decisions exist but none are kept).
        decisions::insert(
            &pool,
            DecisionInsert {
                search_id: without.id,
                provider_id: None,
                provider_name: "p".to_string(),
                release_name: "r".to_string(),
                release_id_remote: 2,
                score: 0,
                rejected: true,
                tags: Vec::new(),
                matched_rules: Vec::new(),
                seeders: 0,
                leechers: 0,
                size_bytes: 1,
                resolution: Resolution::P720,
                kind: ReleaseKind::WebDl,
                download_url: None,
                details_url: None,
                provider_kind: Some("unit3d".to_string()),
                published_at: None,
                audio_languages: Vec::new(),
                subtitle_languages: Vec::new(),
                profile_scores: std::collections::HashMap::new(),
            },
        )
        .await
        .unwrap();

        let kept_only = filter(
            &pool,
            FilterParams {
                has_kept_decision: Some(true),
                limit: 50,
                ..FilterParams::default()
            },
        )
        .await
        .unwrap();
        assert_eq!(kept_only.len(), 1);
        assert_eq!(kept_only[0].id, with_kept.id);

        let no_kept = filter(
            &pool,
            FilterParams {
                has_kept_decision: Some(false),
                limit: 50,
                ..FilterParams::default()
            },
        )
        .await
        .unwrap();
        assert_eq!(no_kept.len(), 1);
        assert_eq!(no_kept[0].id, without.id);

        // None → both searches returned.
        let either = filter(
            &pool,
            FilterParams {
                limit: 50,
                ..FilterParams::default()
            },
        )
        .await
        .unwrap();
        assert_eq!(either.len(), 2);
    }
}
