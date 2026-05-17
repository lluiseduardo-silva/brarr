//! `push_history` table — audit log of every push attempt brarr made
//! to an *arr instance.
//!
//! See `migrations/20260517140000_push_history.sql` for schema notes.
//! Rows are write-mostly: the search pipeline inserts one row per
//! push attempt; the admin UI reads them grouped by decision or
//! ordered by `pushed_at DESC` for the global feed.

use brarr_arr::ArrKind;
use sqlx::{Row, sqlite::SqliteRow};
use time::OffsetDateTime;
use uuid::Uuid;

use crate::{AppError, db::Pool};

/// Status discriminator persisted in [`PushHistoryRow::status`].
///
/// Kept as a typed enum on the Rust side for ergonomic matching, but
/// serialised to/from the DB as a free-form short string so adding a
/// new variant doesn't require a migration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PushStatus {
    /// *arr accepted the push (HTTP 2xx) and reported no immediate
    /// rejection. The release may still be rejected later by *arr's
    /// internal pipeline (e.g. quality profile mismatch); those
    /// downstream rejections aren't visible to brarr.
    Ok,
    /// *arr returned a non-2xx HTTP status. `http_status` carries the
    /// code (400 for malformed payload, 401 for bad apikey, etc.) and
    /// `response_body` captures the *arr-side error message.
    HttpError,
    /// `reqwest` couldn't reach the *arr instance at all (DNS failure,
    /// timeout, TLS handshake error). No `http_status`.
    TransportError,
}

impl PushStatus {
    /// Short tag for the `status` column.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::HttpError => "http_error",
            Self::TransportError => "transport_error",
        }
    }

    fn from_label(s: &str) -> Self {
        match s {
            "ok" => Self::Ok,
            "transport_error" => Self::TransportError,
            // Default to HttpError for unknown labels — the row is
            // still useful and the operator can spot the typo by
            // looking at the raw value.
            _ => Self::HttpError,
        }
    }
}

/// One persisted push attempt.
#[derive(Debug, Clone)]
pub struct PushHistoryRow {
    /// Stable UUID v4.
    pub id: Uuid,
    /// FK → `decisions.id`. The release that was pushed.
    pub decision_id: Uuid,
    /// FK → `arr_instances.id`. `None` if the *arr instance has since
    /// been deleted.
    pub arr_instance_id: Option<Uuid>,
    /// *arr instance display name snapshot (preserved across deletes).
    pub arr_instance_name: String,
    /// Sonarr / Radarr.
    pub arr_kind: ArrKind,
    /// When the push was attempted.
    pub pushed_at: OffsetDateTime,
    /// Status discriminator.
    pub status: PushStatus,
    /// HTTP status when applicable. `None` for transport errors.
    pub http_status: Option<u16>,
    /// Body slice from *arr (truncated to 8 KiB upstream).
    pub response_body: Option<String>,
    /// Parsed `rejections` array extracted from `response_body` (one
    /// entry per *arr-side rejection reason — quality profile, custom
    /// format, queue dedup, etc.). Empty `Vec` means *arr accepted the
    /// release cleanly; non-empty means HTTP 200 but no grab. `None`
    /// means brarr couldn't parse the body (legacy rows, transport
    /// errors, or *arr-side error pages).
    pub rejections: Option<Vec<String>>,
    /// Release title pulled from the parent `decisions` row via JOIN.
    /// Used by the `/pushes` UI to group multiple attempts of the same
    /// release under a single header — same release pushed three times
    /// = one collapsible row, not three siblings cluttering the table.
    /// Empty string only when the decision was deleted before the row
    /// was read (shouldn't happen in practice — `ON DELETE CASCADE`
    /// drops push_history first).
    pub release_name: String,
    /// Provider name snapshot from the parent `decisions` row.
    pub provider_name: String,
}

/// Bundle of values used to insert one push history row.
#[derive(Debug, Clone)]
pub struct NewPushHistory<'a> {
    /// Decision being pushed.
    pub decision_id: Uuid,
    /// Target *arr.
    pub arr_instance_id: Uuid,
    /// Snapshot name (read once when the push fires, before *arr
    /// possibly gets deleted).
    pub arr_instance_name: &'a str,
    /// Snapshot flavour.
    pub arr_kind: ArrKind,
    /// Outcome.
    pub status: PushStatus,
    /// HTTP status when applicable.
    pub http_status: Option<u16>,
    /// *arr-side response body (only useful on failure).
    pub response_body: Option<&'a str>,
    /// Parsed `rejections` from `response_body` — see
    /// [`PushHistoryRow::rejections`].
    pub rejections: Option<Vec<String>>,
}

/// Insert one push history row.
///
/// # Errors
///
/// Returns [`AppError::Database`] on SQL failure.
pub async fn insert(pool: &Pool, new: NewPushHistory<'_>) -> Result<PushHistoryRow, AppError> {
    let id = Uuid::new_v4();
    let now = OffsetDateTime::now_utc();
    let rejections_json = match new.rejections.as_ref() {
        Some(v) => Some(serde_json::to_string(v)?),
        None => None,
    };
    sqlx::query(
        "INSERT INTO push_history ( \
            id, decision_id, arr_instance_id, arr_instance_name, arr_kind, \
            pushed_at, status, http_status, response_body, rejections_json \
         ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(id.to_string())
    .bind(new.decision_id.to_string())
    .bind(new.arr_instance_id.to_string())
    .bind(new.arr_instance_name)
    .bind(new.arr_kind.label())
    .bind(now.unix_timestamp())
    .bind(new.status.label())
    .bind(new.http_status.map(i64::from))
    .bind(new.response_body)
    .bind(rejections_json.as_deref())
    .execute(pool)
    .await?;
    // Re-read the row through `get_by_id` so the JOIN-derived
    // release_name / provider_name fields are populated. Cheap (PK
    // lookup) and keeps the in-memory row identical to what a later
    // /pushes page query would surface.
    get_by_id(pool, id).await
}

/// Fetch one row by id. Used internally by `insert` to refresh the
/// row through the JOIN that populates `release_name` /
/// `provider_name`.
///
/// # Errors
///
/// Returns [`AppError::NotFound`] when the id is missing,
/// [`AppError::Database`] on SQL failure.
pub async fn get_by_id(pool: &Pool, id: Uuid) -> Result<PushHistoryRow, AppError> {
    let row_opt = sqlx::query(
        "SELECT id, decision_id, arr_instance_id, arr_instance_name, arr_kind, \
                pushed_at, status, http_status, response_body, rejections_json, \
                (SELECT release_name FROM decisions WHERE id = decision_id) AS release_name, \
                (SELECT provider_name FROM decisions WHERE id = decision_id) AS provider_name \
         FROM push_history WHERE id = ?",
    )
    .bind(id.to_string())
    .fetch_optional(pool)
    .await?;
    match row_opt {
        Some(row) => row_to_push(&row),
        None => Err(AppError::NotFound(format!("push_history {id}"))),
    }
}

/// Most recent `limit` push rows across all decisions. Used by the
/// admin UI's "Push activity" page.
///
/// # Errors
///
/// Returns [`AppError::Database`] on SQL failure.
pub async fn recent(pool: &Pool, limit: u32) -> Result<Vec<PushHistoryRow>, AppError> {
    let limit = match limit {
        0 => 50,
        n if n > 500 => 500,
        n => n,
    };
    let rows = sqlx::query(
        "SELECT id, decision_id, arr_instance_id, arr_instance_name, arr_kind, \
                pushed_at, status, http_status, response_body, rejections_json, \
                (SELECT release_name FROM decisions WHERE id = decision_id) AS release_name, \
                (SELECT provider_name FROM decisions WHERE id = decision_id) AS provider_name \
         FROM push_history ORDER BY pushed_at DESC LIMIT ?",
    )
    .bind(i64::from(limit))
    .fetch_all(pool)
    .await?;
    rows.iter().map(row_to_push).collect()
}

/// Aggregate success counters for the dashboard. Returns
/// `(total_attempts, successful_attempts)` across the full
/// `push_history` table. Dashboard uses the ratio to render the
/// "PUSHES OK" stat card.
///
/// # Errors
///
/// Returns [`AppError::Database`] on SQL failure.
pub async fn success_rate(pool: &Pool) -> Result<(u64, u64), AppError> {
    let row = sqlx::query(
        "SELECT \
            COUNT(*) AS total, \
            SUM(CASE WHEN status = 'ok' THEN 1 ELSE 0 END) AS ok \
         FROM push_history",
    )
    .fetch_one(pool)
    .await?;
    let total: i64 = row.try_get("total").unwrap_or(0);
    let ok: i64 = row.try_get("ok").unwrap_or(0);
    Ok((
        u64::try_from(total).unwrap_or(0),
        u64::try_from(ok).unwrap_or(0),
    ))
}

/// All push attempts for a given decision, oldest first.
///
/// # Errors
///
/// Returns [`AppError::Database`] on SQL failure.
pub async fn list_for_decision(
    pool: &Pool,
    decision_id: Uuid,
) -> Result<Vec<PushHistoryRow>, AppError> {
    let rows = sqlx::query(
        "SELECT id, decision_id, arr_instance_id, arr_instance_name, arr_kind, \
                pushed_at, status, http_status, response_body, rejections_json, \
                (SELECT release_name FROM decisions WHERE id = decision_id) AS release_name, \
                (SELECT provider_name FROM decisions WHERE id = decision_id) AS provider_name \
         FROM push_history WHERE decision_id = ? ORDER BY pushed_at ASC",
    )
    .bind(decision_id.to_string())
    .fetch_all(pool)
    .await?;
    rows.iter().map(row_to_push).collect()
}

/// Has any release with the same `(provider_id, release_id_remote)`
/// already been pushed to this `arr_instance_id`?
///
/// Counts **all** attempts (success, *arr rejection, transport
/// failure) — once brarr handed a specific upstream release to a
/// specific *arr, the same release won't be re-pushed regardless of
/// outcome. The next poll cycle will pick the next-best decision
/// instead. Use this when the downstream download outcome matters:
/// a "successful push" that resulted in a stuck/dead/missing-articles
/// grab should not be retried with the same release.
///
/// Joins through `decisions` because brarr's `push_history` snapshots
/// `decision_id` (a UUID per poll), not the upstream release id.
///
/// # Errors
///
/// Returns [`AppError::Database`] on SQL failure.
pub async fn already_tried_release(
    pool: &Pool,
    provider_id: Uuid,
    release_id_remote: u64,
    arr_instance_id: Uuid,
) -> Result<bool, AppError> {
    let release_id_signed = i64_from_u64(release_id_remote);
    let row = sqlx::query(
        "SELECT COUNT(*) AS n \
         FROM push_history ph \
         JOIN decisions d ON d.id = ph.decision_id \
         WHERE d.provider_id = ? AND d.release_id_remote = ? AND ph.arr_instance_id = ?",
    )
    .bind(provider_id.to_string())
    .bind(release_id_signed)
    .bind(arr_instance_id.to_string())
    .fetch_one(pool)
    .await?;
    let n: i64 = row.try_get("n")?;
    Ok(n > 0)
}

/// SQLite has no native u64; we store u64 reinterpreted as i64.
#[allow(
    clippy::cast_possible_wrap,
    reason = "release ids comfortably fit in i64 positive range"
)]
const fn i64_from_u64(v: u64) -> i64 {
    v as i64
}

/// Has this `(decision_id, arr_instance_id)` pair already been pushed
/// successfully? Used by the auto-push path to avoid double-grabbing
/// the same release when the search reruns.
///
/// # Errors
///
/// Returns [`AppError::Database`] on SQL failure.
pub async fn already_pushed(
    pool: &Pool,
    decision_id: Uuid,
    arr_instance_id: Uuid,
) -> Result<bool, AppError> {
    let row = sqlx::query(
        "SELECT COUNT(*) AS n FROM push_history \
         WHERE decision_id = ? AND arr_instance_id = ? AND status = 'ok'",
    )
    .bind(decision_id.to_string())
    .bind(arr_instance_id.to_string())
    .fetch_one(pool)
    .await?;
    let n: i64 = row.try_get("n")?;
    Ok(n > 0)
}

fn row_to_push(row: &SqliteRow) -> Result<PushHistoryRow, AppError> {
    let id_str: String = row.try_get("id")?;
    let id = Uuid::parse_str(&id_str)
        .map_err(|e| AppError::InvalidInput(format!("invalid uuid in push_history.id: {e}")))?;
    let decision_id_str: String = row.try_get("decision_id")?;
    let decision_id = Uuid::parse_str(&decision_id_str)
        .map_err(|e| AppError::InvalidInput(format!("invalid uuid in decision_id: {e}")))?;
    let arr_instance_id_opt: Option<String> = row.try_get("arr_instance_id")?;
    let arr_instance_id = match arr_instance_id_opt {
        Some(s) => Some(Uuid::parse_str(&s).map_err(|e| {
            AppError::InvalidInput(format!("invalid uuid in arr_instance_id: {e}"))
        })?),
        None => None,
    };
    let arr_kind_str: String = row.try_get("arr_kind")?;
    let arr_kind = match arr_kind_str.as_str() {
        "sonarr" => ArrKind::Sonarr,
        "radarr" => ArrKind::Radarr,
        other => {
            return Err(AppError::InvalidInput(format!(
                "unknown push_history.arr_kind: {other}"
            )));
        }
    };
    let pushed_unix: i64 = row.try_get("pushed_at")?;
    let pushed_at = OffsetDateTime::from_unix_timestamp(pushed_unix)
        .map_err(|e| AppError::InvalidInput(format!("invalid timestamp: {e}")))?;
    let status_str: String = row.try_get("status")?;
    let http_status_i64: Option<i64> = row.try_get("http_status")?;
    let http_status = http_status_i64.and_then(|s| u16::try_from(s).ok());
    // `rejections_json` is NULL on legacy rows + transport-error pushes.
    // Bad JSON silently degrades to None rather than failing the whole
    // row — the operator can still read the raw `response_body`.
    let rejections_json: Option<String> = row.try_get("rejections_json").ok().flatten();
    let rejections = rejections_json.and_then(|s| serde_json::from_str::<Vec<String>>(&s).ok());
    Ok(PushHistoryRow {
        id,
        decision_id,
        arr_instance_id,
        arr_instance_name: row.try_get("arr_instance_name")?,
        arr_kind,
        pushed_at,
        status: PushStatus::from_label(&status_str),
        http_status,
        response_body: row.try_get("response_body")?,
        rejections,
        release_name: row.try_get("release_name").unwrap_or_default(),
        provider_name: row.try_get("provider_name").unwrap_or_default(),
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use crate::db::arr_instances::{self, NewArrInstance};
    use crate::db::decisions::{self, DecisionInsert};
    use crate::db::open_memory;
    use crate::db::searches::{self, SearchRequestJson};
    use brarr_core::{ReleaseKind, Resolution};
    use url::Url;

    async fn make_decision(pool: &Pool) -> Uuid {
        let search = searches::create(
            pool,
            SearchRequestJson {
                tmdb_id: Some(603),
                ..SearchRequestJson::default()
            },
        )
        .await
        .unwrap();
        let row = decisions::insert(
            pool,
            DecisionInsert {
                search_id: search.id,
                provider_id: None,
                provider_name: "p".into(),
                release_name: "r".into(),
                release_id_remote: 1,
                score: 800,
                rejected: false,
                tags: vec![],
                matched_rules: vec![],
                seeders: 1,
                leechers: 0,
                size_bytes: 1,
                resolution: Resolution::P1080,
                kind: ReleaseKind::WebDl,
                download_url: None,
                details_url: None,
                provider_kind: Some("unit3d".into()),
                published_at: None,
                audio_languages: Vec::new(),
                subtitle_languages: Vec::new(),
                profile_scores: std::collections::HashMap::new(),
            },
        )
        .await
        .unwrap();
        row.id
    }

    async fn make_arr_instance(pool: &Pool) -> Uuid {
        let url = Url::parse("https://r.example/").unwrap();
        arr_instances::insert(
            pool,
            NewArrInstance {
                name: "radarr-main",
                kind: ArrKind::Radarr,
                base_url: &url,
                api_key: "k",
                push_threshold: None,
                profile_id: None,
                enabled: None,
            },
        )
        .await
        .unwrap()
        .id
    }

    #[tokio::test]
    async fn insert_and_list_roundtrips() {
        let pool = open_memory().await.unwrap();
        let did = make_decision(&pool).await;
        let aid = make_arr_instance(&pool).await;
        let row = insert(
            &pool,
            NewPushHistory {
                decision_id: did,
                arr_instance_id: aid,
                arr_instance_name: "radarr-main",
                arr_kind: ArrKind::Radarr,
                status: PushStatus::Ok,
                http_status: Some(200),
                response_body: None,
                rejections: None,
            },
        )
        .await
        .unwrap();
        assert_eq!(row.status, PushStatus::Ok);
        let list = list_for_decision(&pool, did).await.unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].id, row.id);
    }

    #[tokio::test]
    async fn already_pushed_only_counts_ok_rows() {
        let pool = open_memory().await.unwrap();
        let did = make_decision(&pool).await;
        let aid = make_arr_instance(&pool).await;
        // Failure first — should NOT count as "already pushed".
        insert(
            &pool,
            NewPushHistory {
                decision_id: did,
                arr_instance_id: aid,
                arr_instance_name: "x",
                arr_kind: ArrKind::Radarr,
                status: PushStatus::HttpError,
                http_status: Some(400),
                response_body: Some("Unknown movie"),
                rejections: Some(vec!["Unknown movie".to_string()]),
            },
        )
        .await
        .unwrap();
        assert!(!already_pushed(&pool, did, aid).await.unwrap());

        // Successful push — should now count.
        insert(
            &pool,
            NewPushHistory {
                decision_id: did,
                arr_instance_id: aid,
                arr_instance_name: "x",
                arr_kind: ArrKind::Radarr,
                status: PushStatus::Ok,
                http_status: Some(200),
                response_body: None,
                rejections: None,
            },
        )
        .await
        .unwrap();
        assert!(already_pushed(&pool, did, aid).await.unwrap());
    }

    #[tokio::test]
    async fn recent_orders_by_pushed_at_desc() {
        let pool = open_memory().await.unwrap();
        let did = make_decision(&pool).await;
        let aid = make_arr_instance(&pool).await;
        for status in [PushStatus::Ok, PushStatus::HttpError, PushStatus::Ok] {
            insert(
                &pool,
                NewPushHistory {
                    decision_id: did,
                    arr_instance_id: aid,
                    arr_instance_name: "x",
                    arr_kind: ArrKind::Radarr,
                    status,
                    http_status: Some(200),
                    response_body: None,
                    rejections: None,
                },
            )
            .await
            .unwrap();
            // Insert order matters — sqlite stores in arrival order
            // and same-second timestamps are kept stable by rowid.
        }
        let rows = recent(&pool, 10).await.unwrap();
        assert_eq!(rows.len(), 3);
    }

    #[tokio::test]
    async fn deleting_decision_cascades_to_push_history() {
        let pool = open_memory().await.unwrap();
        let did = make_decision(&pool).await;
        let aid = make_arr_instance(&pool).await;
        insert(
            &pool,
            NewPushHistory {
                decision_id: did,
                arr_instance_id: aid,
                arr_instance_name: "x",
                arr_kind: ArrKind::Radarr,
                status: PushStatus::Ok,
                http_status: Some(200),
                response_body: None,
                rejections: None,
            },
        )
        .await
        .unwrap();
        // Pull the search id from the decisions row and delete the
        // search to cascade through decisions → push_history.
        let row = sqlx::query("SELECT search_id FROM decisions WHERE id = ?")
            .bind(did.to_string())
            .fetch_one(&pool)
            .await
            .unwrap();
        let search_id_str: String = row.try_get("search_id").unwrap();
        sqlx::query("DELETE FROM searches WHERE id = ?")
            .bind(search_id_str)
            .execute(&pool)
            .await
            .unwrap();
        assert!(list_for_decision(&pool, did).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn deleting_arr_instance_nulls_fk_but_keeps_audit() {
        let pool = open_memory().await.unwrap();
        let did = make_decision(&pool).await;
        let aid = make_arr_instance(&pool).await;
        insert(
            &pool,
            NewPushHistory {
                decision_id: did,
                arr_instance_id: aid,
                arr_instance_name: "radarr-main",
                arr_kind: ArrKind::Radarr,
                status: PushStatus::Ok,
                http_status: Some(200),
                response_body: None,
                rejections: None,
            },
        )
        .await
        .unwrap();
        arr_instances::delete_by_id(&pool, aid).await.unwrap();
        let rows = list_for_decision(&pool, did).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert!(rows[0].arr_instance_id.is_none());
        assert_eq!(rows[0].arr_instance_name, "radarr-main");
    }
}
