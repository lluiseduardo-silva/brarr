//! `webhook_events` table — audit log of every inbound *arr webhook.
//!
//! See `migrations/20260524120000_webhook_events.sql` for schema notes.
//! Rows are write-mostly: the webhook handler inserts one row per
//! incoming POST (whether or not it triggered a search), and the
//! handler back-fills `triggered_search_id` once the spawned search
//! produces a row.

use brarr_arr::ArrKind;
use sqlx::{Row, sqlite::SqliteRow};
use time::OffsetDateTime;
use uuid::Uuid;

use crate::{AppError, db::Pool};

/// One persisted inbound webhook event.
#[derive(Debug, Clone)]
pub struct WebhookEventRow {
    /// Stable UUID v4.
    pub id: Uuid,
    /// FK → `arr_instances.id`.
    pub arr_instance_id: Uuid,
    /// Sonarr / Radarr.
    pub kind: ArrKind,
    /// *arr's `eventType` field, verbatim (`Test`, `MovieAdded`, etc.).
    pub event_type: String,
    /// Raw JSON body. Bounded only by *arr's payload size in practice.
    pub payload_json: String,
    /// When the orchestrator first saw the request.
    pub received_at: OffsetDateTime,
    /// FK → `searches.id`. Back-filled once the spawned search
    /// completes. `None` for events that don't trigger a search
    /// (`Test`, unknown types, payloads with no usable id).
    pub triggered_search_id: Option<Uuid>,
}

/// Bundle of values used to insert one webhook event row.
#[derive(Debug, Clone)]
pub struct NewWebhookEvent<'a> {
    /// Source *arr instance.
    pub arr_instance_id: Uuid,
    /// Sonarr / Radarr.
    pub kind: ArrKind,
    /// Raw `eventType`.
    pub event_type: &'a str,
    /// Raw JSON payload (stored verbatim).
    pub payload_json: &'a str,
}

/// Insert one inbound webhook row. Returns the persisted shape so
/// callers can hand the `id` to [`link_search`] once the spawned
/// search completes.
///
/// # Errors
///
/// Returns [`AppError::Database`] on SQL failure.
pub async fn insert(pool: &Pool, new: NewWebhookEvent<'_>) -> Result<WebhookEventRow, AppError> {
    let id = Uuid::new_v4();
    let now = OffsetDateTime::now_utc();
    sqlx::query(
        "INSERT INTO webhook_events ( \
            id, arr_instance_id, kind, event_type, payload_json, received_at \
         ) VALUES (?, ?, ?, ?, ?, ?)",
    )
    .bind(id.to_string())
    .bind(new.arr_instance_id.to_string())
    .bind(new.kind.label())
    .bind(new.event_type)
    .bind(new.payload_json)
    .bind(now.unix_timestamp())
    .execute(pool)
    .await?;
    Ok(WebhookEventRow {
        id,
        arr_instance_id: new.arr_instance_id,
        kind: new.kind,
        event_type: new.event_type.to_string(),
        payload_json: new.payload_json.to_string(),
        received_at: now,
        triggered_search_id: None,
    })
}

/// Back-fill `triggered_search_id` on an existing row.
///
/// # Errors
///
/// Returns [`AppError::Database`] on SQL failure or [`AppError::NotFound`]
/// when no row with the given id exists.
pub async fn link_search(pool: &Pool, event_id: Uuid, search_id: Uuid) -> Result<(), AppError> {
    let res = sqlx::query("UPDATE webhook_events SET triggered_search_id = ? WHERE id = ?")
        .bind(search_id.to_string())
        .bind(event_id.to_string())
        .execute(pool)
        .await?;
    if res.rows_affected() == 0 {
        return Err(AppError::NotFound(format!("webhook_events {event_id}")));
    }
    Ok(())
}

/// Most recent `limit` rows across all *arr instances.
///
/// # Errors
///
/// Returns [`AppError::Database`] on SQL failure.
pub async fn recent(pool: &Pool, limit: u32) -> Result<Vec<WebhookEventRow>, AppError> {
    let limit = match limit {
        0 => 50,
        n if n > 500 => 500,
        n => n,
    };
    let rows = sqlx::query(
        "SELECT id, arr_instance_id, kind, event_type, payload_json, received_at, triggered_search_id \
         FROM webhook_events ORDER BY received_at DESC LIMIT ?",
    )
    .bind(i64::from(limit))
    .fetch_all(pool)
    .await?;
    rows.iter().map(row_to_event).collect()
}

fn row_to_event(row: &SqliteRow) -> Result<WebhookEventRow, AppError> {
    let id_str: String = row.try_get("id")?;
    let id = Uuid::parse_str(&id_str)
        .map_err(|e| AppError::InvalidInput(format!("invalid uuid in webhook_events.id: {e}")))?;
    let arr_id_str: String = row.try_get("arr_instance_id")?;
    let arr_instance_id = Uuid::parse_str(&arr_id_str)
        .map_err(|e| AppError::InvalidInput(format!("invalid uuid in arr_instance_id: {e}")))?;
    let kind_str: String = row.try_get("kind")?;
    let kind = match kind_str.as_str() {
        "sonarr" => ArrKind::Sonarr,
        "radarr" => ArrKind::Radarr,
        other => {
            return Err(AppError::InvalidInput(format!(
                "unknown webhook_events.kind: {other}"
            )));
        }
    };
    let received_unix: i64 = row.try_get("received_at")?;
    let received_at = OffsetDateTime::from_unix_timestamp(received_unix)
        .map_err(|e| AppError::InvalidInput(format!("invalid timestamp: {e}")))?;
    let triggered_str: Option<String> = row.try_get("triggered_search_id")?;
    let triggered_search_id = match triggered_str {
        Some(s) => Some(Uuid::parse_str(&s).map_err(|e| {
            AppError::InvalidInput(format!("invalid uuid in triggered_search_id: {e}"))
        })?),
        None => None,
    };
    Ok(WebhookEventRow {
        id,
        arr_instance_id,
        kind,
        event_type: row.try_get("event_type")?,
        payload_json: row.try_get("payload_json")?,
        received_at,
        triggered_search_id,
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use crate::db::arr_instances::{self, NewArrInstance};
    use crate::db::open_memory;
    use crate::db::searches::{self, SearchRequestJson};
    use url::Url;

    async fn make_arr(pool: &Pool, name: &str, kind: ArrKind) -> Uuid {
        let url = Url::parse("https://arr.example/").unwrap();
        arr_instances::insert(
            pool,
            NewArrInstance {
                name,
                kind,
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
    async fn insert_roundtrips_and_carries_kind() {
        let pool = open_memory().await.unwrap();
        let arr_id = make_arr(&pool, "radarr-main", ArrKind::Radarr).await;
        let row = insert(
            &pool,
            NewWebhookEvent {
                arr_instance_id: arr_id,
                kind: ArrKind::Radarr,
                event_type: "MovieAdded",
                payload_json: r#"{"eventType":"MovieAdded"}"#,
            },
        )
        .await
        .unwrap();
        assert_eq!(row.kind, ArrKind::Radarr);
        assert_eq!(row.event_type, "MovieAdded");
        assert!(row.triggered_search_id.is_none());

        let recent_rows = recent(&pool, 10).await.unwrap();
        assert_eq!(recent_rows.len(), 1);
        assert_eq!(recent_rows[0].id, row.id);
    }

    #[tokio::test]
    async fn link_search_backfills_search_id() {
        let pool = open_memory().await.unwrap();
        let arr_id = make_arr(&pool, "sonarr-main", ArrKind::Sonarr).await;
        let event = insert(
            &pool,
            NewWebhookEvent {
                arr_instance_id: arr_id,
                kind: ArrKind::Sonarr,
                event_type: "EpisodeAdded",
                payload_json: "{}",
            },
        )
        .await
        .unwrap();
        let search = searches::create(
            &pool,
            SearchRequestJson {
                tvdb_id: Some(81189),
                season: Some(1),
                episode: Some(1),
                ..SearchRequestJson::default()
            },
        )
        .await
        .unwrap();

        link_search(&pool, event.id, search.id).await.unwrap();
        let rows = recent(&pool, 10).await.unwrap();
        assert_eq!(rows[0].triggered_search_id, Some(search.id));
    }

    #[tokio::test]
    async fn link_search_unknown_event_id_is_not_found() {
        let pool = open_memory().await.unwrap();
        let arr_id = make_arr(&pool, "x", ArrKind::Radarr).await;
        let search = searches::create(
            &pool,
            SearchRequestJson {
                tmdb_id: Some(603),
                ..SearchRequestJson::default()
            },
        )
        .await
        .unwrap();
        let _ = arr_id;
        let err = link_search(&pool, Uuid::new_v4(), search.id)
            .await
            .expect_err("unknown event id");
        assert!(matches!(err, AppError::NotFound(_)));
    }

    #[tokio::test]
    async fn deleting_arr_instance_cascades_to_webhook_events() {
        let pool = open_memory().await.unwrap();
        let arr_id = make_arr(&pool, "radarr-x", ArrKind::Radarr).await;
        insert(
            &pool,
            NewWebhookEvent {
                arr_instance_id: arr_id,
                kind: ArrKind::Radarr,
                event_type: "Test",
                payload_json: "{}",
            },
        )
        .await
        .unwrap();
        arr_instances::delete_by_id(&pool, arr_id).await.unwrap();
        assert!(recent(&pool, 10).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn deleting_search_nulls_link_but_keeps_event() {
        let pool = open_memory().await.unwrap();
        let arr_id = make_arr(&pool, "sonarr-x", ArrKind::Sonarr).await;
        let event = insert(
            &pool,
            NewWebhookEvent {
                arr_instance_id: arr_id,
                kind: ArrKind::Sonarr,
                event_type: "SeriesAdded",
                payload_json: "{}",
            },
        )
        .await
        .unwrap();
        let search = searches::create(
            &pool,
            SearchRequestJson {
                tvdb_id: Some(81189),
                ..SearchRequestJson::default()
            },
        )
        .await
        .unwrap();
        link_search(&pool, event.id, search.id).await.unwrap();

        sqlx::query("DELETE FROM searches WHERE id = ?")
            .bind(search.id.to_string())
            .execute(&pool)
            .await
            .unwrap();
        let rows = recent(&pool, 10).await.unwrap();
        assert_eq!(rows.len(), 1, "event row should still exist");
        assert!(
            rows[0].triggered_search_id.is_none(),
            "ON DELETE SET NULL should clear the link"
        );
    }
}
