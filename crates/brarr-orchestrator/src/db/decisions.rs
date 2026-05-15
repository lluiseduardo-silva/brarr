//! Decision rows. One row per release that survived the rules engine
//! (rejected releases are still persisted so the UI can show *why*
//! something was filtered out).

use brarr_core::{ReleaseKind, Resolution};
use sqlx::{Row, sqlite::SqliteRow};
use time::OffsetDateTime;
use uuid::Uuid;

use crate::{AppError, db::Pool};

/// Persisted decision row.
#[derive(Debug, Clone)]
pub struct DecisionRow {
    /// Stable UUID v4.
    pub id: Uuid,
    /// FK → `searches.id`.
    pub search_id: Uuid,
    /// FK → `trackers.id` (or `None` if the tracker has since been deleted).
    pub tracker_id: Option<Uuid>,
    /// Tracker name snapshot (denormalized so the UI doesn't `JOIN` and
    /// historical rows survive tracker deletions).
    pub tracker_name: String,
    /// Release title.
    pub release_name: String,
    /// Tracker-side numeric id (so the UI can deep-link).
    pub release_id_remote: u64,
    /// Score assigned by the rules engine.
    pub score: u32,
    /// Whether the engine rejected this release.
    pub rejected: bool,
    /// Tags attached by matched rules.
    pub tags: Vec<String>,
    /// Names of the rules that matched.
    pub matched_rules: Vec<String>,
    /// Seeder count snapshot.
    pub seeders: u32,
    /// Leecher count snapshot.
    pub leechers: u32,
    /// Total bytes.
    pub size_bytes: u64,
    /// Resolution label.
    pub resolution: String,
    /// Kind label.
    pub kind: String,
    /// When the engine produced the outcome.
    pub decided_at: OffsetDateTime,
}

/// Input bundle used to insert a single decision row.
#[derive(Debug, Clone)]
pub struct DecisionInsert {
    /// FK → `searches.id`.
    pub search_id: Uuid,
    /// FK → `trackers.id`.
    pub tracker_id: Option<Uuid>,
    /// Tracker name snapshot.
    pub tracker_name: String,
    /// Release title.
    pub release_name: String,
    /// Tracker-side numeric id.
    pub release_id_remote: u64,
    /// Engine score.
    pub score: u32,
    /// Rejected flag.
    pub rejected: bool,
    /// Tags collected from matched rules.
    pub tags: Vec<String>,
    /// Names of matched rules.
    pub matched_rules: Vec<String>,
    /// Seeders.
    pub seeders: u32,
    /// Leechers.
    pub leechers: u32,
    /// Size in bytes.
    pub size_bytes: u64,
    /// Resolution from `brarr_core`.
    pub resolution: Resolution,
    /// Release kind from `brarr_core`.
    pub kind: ReleaseKind,
}

/// Insert one decision row, returning the persisted form.
///
/// # Errors
///
/// Surfaces a [`sqlx::Error`] on SQL failure or a JSON error if `tags`/
/// `matched_rules` cannot be serialized.
pub async fn insert(pool: &Pool, ins: DecisionInsert) -> Result<DecisionRow, AppError> {
    let id = Uuid::new_v4();
    let now = OffsetDateTime::now_utc();
    let tags_json = serde_json::to_string(&ins.tags)?;
    let matched_json = serde_json::to_string(&ins.matched_rules)?;
    let resolution = resolution_label(&ins.resolution);
    let kind = kind_label(&ins.kind);

    sqlx::query(
        "INSERT INTO decisions ( \
            id, search_id, tracker_id, tracker_name, release_name, release_id_remote, \
            score, rejected, tags_json, matched_json, seeders, leechers, size_bytes, \
            resolution, kind, decided_at \
         ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(id.to_string())
    .bind(ins.search_id.to_string())
    .bind(ins.tracker_id.map(|t| t.to_string()))
    .bind(&ins.tracker_name)
    .bind(&ins.release_name)
    .bind(i64_from_u64(ins.release_id_remote))
    .bind(i64::from(ins.score))
    .bind(i64::from(u8::from(ins.rejected)))
    .bind(&tags_json)
    .bind(&matched_json)
    .bind(i64::from(ins.seeders))
    .bind(i64::from(ins.leechers))
    .bind(i64_from_u64(ins.size_bytes))
    .bind(&resolution)
    .bind(&kind)
    .bind(now.unix_timestamp())
    .execute(pool)
    .await?;

    Ok(DecisionRow {
        id,
        search_id: ins.search_id,
        tracker_id: ins.tracker_id,
        tracker_name: ins.tracker_name,
        release_name: ins.release_name,
        release_id_remote: ins.release_id_remote,
        score: ins.score,
        rejected: ins.rejected,
        tags: ins.tags,
        matched_rules: ins.matched_rules,
        seeders: ins.seeders,
        leechers: ins.leechers,
        size_bytes: ins.size_bytes,
        resolution,
        kind,
        decided_at: now,
    })
}

/// Fetch all decisions for a given search, ordered by score DESC.
///
/// # Errors
///
/// Surfaces a [`sqlx::Error`] on SQL failure.
pub async fn list_for_search(pool: &Pool, search_id: Uuid) -> Result<Vec<DecisionRow>, AppError> {
    let rows = sqlx::query(
        "SELECT id, search_id, tracker_id, tracker_name, release_name, release_id_remote, \
                score, rejected, tags_json, matched_json, seeders, leechers, size_bytes, \
                resolution, kind, decided_at \
         FROM decisions WHERE search_id = ? ORDER BY score DESC, seeders DESC",
    )
    .bind(search_id.to_string())
    .fetch_all(pool)
    .await?;
    rows.iter().map(row_to_decision).collect()
}

/// Return the most recent `limit` decision rows across all searches.
///
/// # Errors
///
/// Surfaces a [`sqlx::Error`] on SQL failure.
pub async fn recent(pool: &Pool, limit: u32) -> Result<Vec<DecisionRow>, AppError> {
    let limit = match limit {
        0 => 50,
        n if n > 500 => 500,
        n => n,
    };
    let rows = sqlx::query(
        "SELECT id, search_id, tracker_id, tracker_name, release_name, release_id_remote, \
                score, rejected, tags_json, matched_json, seeders, leechers, size_bytes, \
                resolution, kind, decided_at \
         FROM decisions ORDER BY decided_at DESC LIMIT ?",
    )
    .bind(i64::from(limit))
    .fetch_all(pool)
    .await?;
    rows.iter().map(row_to_decision).collect()
}

fn row_to_decision(row: &SqliteRow) -> Result<DecisionRow, AppError> {
    let id_str: String = row.try_get("id")?;
    let id = Uuid::parse_str(&id_str)
        .map_err(|e| AppError::InvalidInput(format!("invalid uuid in decisions.id: {e}")))?;
    let search_id_str: String = row.try_get("search_id")?;
    let search_id = Uuid::parse_str(&search_id_str)
        .map_err(|e| AppError::InvalidInput(format!("invalid uuid in search_id: {e}")))?;
    let tracker_id_opt: Option<String> = row.try_get("tracker_id")?;
    let tracker_id = match tracker_id_opt {
        Some(s) => Some(
            Uuid::parse_str(&s)
                .map_err(|e| AppError::InvalidInput(format!("invalid uuid in tracker_id: {e}")))?,
        ),
        None => None,
    };
    let release_id_remote_i64: i64 = row.try_get("release_id_remote")?;
    let release_id_remote = u64_from_i64(release_id_remote_i64);
    let score_i64: i64 = row.try_get("score")?;
    let rejected_i64: i64 = row.try_get("rejected")?;
    let tags_json: String = row.try_get("tags_json")?;
    let matched_json: String = row.try_get("matched_json")?;
    let seeders_i64: i64 = row.try_get("seeders")?;
    let leechers_i64: i64 = row.try_get("leechers")?;
    let size_bytes_i64: i64 = row.try_get("size_bytes")?;
    let decided_unix: i64 = row.try_get("decided_at")?;
    let decided_at = OffsetDateTime::from_unix_timestamp(decided_unix)
        .map_err(|e| AppError::InvalidInput(format!("invalid timestamp: {e}")))?;

    Ok(DecisionRow {
        id,
        search_id,
        tracker_id,
        tracker_name: row.try_get("tracker_name")?,
        release_name: row.try_get("release_name")?,
        release_id_remote,
        score: u32::try_from(score_i64).unwrap_or(0),
        rejected: rejected_i64 != 0,
        tags: serde_json::from_str(&tags_json)?,
        matched_rules: serde_json::from_str(&matched_json)?,
        seeders: u32::try_from(seeders_i64).unwrap_or(0),
        leechers: u32::try_from(leechers_i64).unwrap_or(0),
        size_bytes: u64_from_i64(size_bytes_i64),
        resolution: row.try_get("resolution")?,
        kind: row.try_get("kind")?,
        decided_at,
    })
}

/// SQLite has no native u64; we store u64 reinterpreted as i64. Negative
/// values are impossible in practice for our domain (sizes, ids).
#[allow(
    clippy::cast_possible_wrap,
    reason = "release ids and byte counts comfortably fit in i64 positive range for this domain"
)]
const fn i64_from_u64(v: u64) -> i64 {
    v as i64
}

#[allow(
    clippy::cast_sign_loss,
    reason = "values were originally u64; sign is purely a storage artifact"
)]
const fn u64_from_i64(v: i64) -> u64 {
    v as u64
}

fn resolution_label(r: &Resolution) -> String {
    match r {
        Resolution::Sd => "SD".to_string(),
        Resolution::P720 => "720p".to_string(),
        Resolution::P1080 => "1080p".to_string(),
        Resolution::P2160 => "2160p".to_string(),
        Resolution::Other(s) => s.clone(),
    }
}

fn kind_label(k: &ReleaseKind) -> String {
    match k {
        ReleaseKind::WebDl => "WEB-DL".to_string(),
        ReleaseKind::BluRay => "BluRay".to_string(),
        ReleaseKind::Encode => "Encode".to_string(),
        ReleaseKind::HdTv => "HDTV".to_string(),
        ReleaseKind::Dvd => "DVD".to_string(),
        ReleaseKind::Other(s) => s.clone(),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use crate::db::searches::SearchRequestJson;
    use crate::db::{open_memory, searches};

    async fn make_search(pool: &Pool) -> Uuid {
        searches::create(
            pool,
            SearchRequestJson {
                tmdb_id: Some(603),
                imdb_id: None,
            },
        )
        .await
        .unwrap()
        .id
    }

    fn sample_insert(search_id: Uuid, score: u32) -> DecisionInsert {
        DecisionInsert {
            search_id,
            tracker_id: None,
            tracker_name: "capybara".into(),
            release_name: "The Matrix 1999 1080p BluRay x264".into(),
            release_id_remote: 12345,
            score,
            rejected: false,
            tags: vec!["PT-BR".into()],
            matched_rules: vec!["PT-BR audio".into(), "1080p".into()],
            seeders: 42,
            leechers: 1,
            size_bytes: 9_608_016_733,
            resolution: Resolution::P1080,
            kind: ReleaseKind::BluRay,
        }
    }

    #[tokio::test]
    async fn insert_and_list_roundtrips() {
        let pool = open_memory().await.unwrap();
        let search_id = make_search(&pool).await;
        let row = insert(&pool, sample_insert(search_id, 110)).await.unwrap();
        assert_eq!(row.score, 110);
        assert_eq!(row.tags, vec!["PT-BR".to_string()]);
        assert_eq!(row.resolution, "1080p");

        let list = list_for_search(&pool, search_id).await.unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].id, row.id);
    }

    #[tokio::test]
    async fn list_for_search_orders_by_score_desc() {
        let pool = open_memory().await.unwrap();
        let search_id = make_search(&pool).await;
        insert(&pool, sample_insert(search_id, 50)).await.unwrap();
        insert(&pool, sample_insert(search_id, 200)).await.unwrap();
        insert(&pool, sample_insert(search_id, 100)).await.unwrap();
        let list = list_for_search(&pool, search_id).await.unwrap();
        assert_eq!(list.len(), 3);
        assert_eq!(list[0].score, 200);
        assert_eq!(list[1].score, 100);
        assert_eq!(list[2].score, 50);
    }

    #[tokio::test]
    async fn cascade_delete_removes_decisions_when_search_dropped() {
        let pool = open_memory().await.unwrap();
        let search_id = make_search(&pool).await;
        insert(&pool, sample_insert(search_id, 10)).await.unwrap();
        sqlx::query("DELETE FROM searches WHERE id = ?")
            .bind(search_id.to_string())
            .execute(&pool)
            .await
            .unwrap();
        let list = list_for_search(&pool, search_id).await.unwrap();
        assert!(list.is_empty());
    }

    #[tokio::test]
    async fn rejected_flag_roundtrips() {
        let pool = open_memory().await.unwrap();
        let search_id = make_search(&pool).await;
        let mut ins = sample_insert(search_id, 0);
        ins.rejected = true;
        let row = insert(&pool, ins).await.unwrap();
        assert!(row.rejected);
        let list = list_for_search(&pool, search_id).await.unwrap();
        assert!(list[0].rejected);
    }
}
