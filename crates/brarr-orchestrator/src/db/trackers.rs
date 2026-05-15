//! Tracker rows in SQLite.
//!
//! The orchestrator owns the canonical tracker list at runtime — the
//! admin UI writes through this module. `brarr-cli` keeps its TOML-based
//! flow unchanged for now; a future phase can teach the CLI to read from
//! the orchestrator via gRPC.

use std::path::PathBuf;

use sqlx::{Row, sqlite::SqliteRow};
use time::OffsetDateTime;
use url::Url;
use uuid::Uuid;

use crate::{AppError, db::Pool};

/// A configured tracker.
#[derive(Debug, Clone)]
pub struct TrackerRow {
    /// Stable UUID v4 used in URLs and gRPC payloads.
    pub id: Uuid,
    /// Human-friendly tracker name (e.g. `capybarabr`). Must be unique.
    pub name: String,
    /// Base URL of the tracker.
    pub base_url: Url,
    /// API token. Stored as plaintext for now; encryption-at-rest is a
    /// future hardening (the DB itself sits on local disk owned by the
    /// service user, not exposed externally).
    pub api_token: String,
    /// Tracker family. Today either `unit3d` or `plugin`.
    pub kind: String,
    /// Filesystem path to a `.wasm`/`.wat` plugin module. `None` means
    /// the tracker is served by the built-in UNIT3D HTTP client.
    pub plugin_path: Option<PathBuf>,
    /// Row creation timestamp.
    pub created_at: OffsetDateTime,
}

impl TrackerRow {
    /// `true` when this row drives a WASM plugin (`plugin_path` set).
    #[must_use]
    pub fn is_plugin(&self) -> bool {
        self.plugin_path.is_some()
    }
}

/// Bundle of values used to create a new tracker row.
#[derive(Debug, Clone)]
pub struct NewTracker<'a> {
    /// Display name (must be unique).
    pub name: &'a str,
    /// Tracker base URL.
    pub base_url: &'a Url,
    /// API token (UNIT3D bearer); free-form for plugins.
    pub api_token: &'a str,
    /// `"unit3d"` or `"plugin"`.
    pub kind: &'a str,
    /// Optional plugin filesystem path.
    pub plugin_path: Option<&'a std::path::Path>,
}

/// Insert a new tracker, returning the persisted row (with id + timestamp).
///
/// # Errors
///
/// Returns [`AppError::Database`] on `UNIQUE` violations of `name`, or
/// any other SQL error.
pub async fn insert(pool: &Pool, new: NewTracker<'_>) -> Result<TrackerRow, AppError> {
    if new.name.trim().is_empty() {
        return Err(AppError::InvalidInput(
            "tracker name cannot be empty".into(),
        ));
    }
    let id = Uuid::new_v4();
    let now = OffsetDateTime::now_utc();
    let base = new.base_url.as_str();
    let id_str = id.to_string();
    let created_unix = now.unix_timestamp();
    let plugin_path_str = new.plugin_path.map(|p| p.to_string_lossy().into_owned());
    sqlx::query(
        "INSERT INTO trackers (id, name, base_url, api_token, kind, plugin_path, created_at) \
         VALUES (?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(&id_str)
    .bind(new.name)
    .bind(base)
    .bind(new.api_token)
    .bind(new.kind)
    .bind(&plugin_path_str)
    .bind(created_unix)
    .execute(pool)
    .await?;

    Ok(TrackerRow {
        id,
        name: new.name.to_string(),
        base_url: new.base_url.clone(),
        api_token: new.api_token.to_string(),
        kind: new.kind.to_string(),
        plugin_path: new.plugin_path.map(PathBuf::from),
        created_at: now,
    })
}

/// List all trackers, ordered by `name` ascending.
///
/// # Errors
///
/// Returns [`AppError::Database`] on SQL failure.
pub async fn list_all(pool: &Pool) -> Result<Vec<TrackerRow>, AppError> {
    let rows = sqlx::query(
        "SELECT id, name, base_url, api_token, kind, plugin_path, created_at \
         FROM trackers ORDER BY name ASC",
    )
    .fetch_all(pool)
    .await?;
    rows.iter().map(row_to_tracker).collect()
}

/// Fetch a tracker by UUID.
///
/// # Errors
///
/// Returns [`AppError::NotFound`] if no row matches.
pub async fn get_by_id(pool: &Pool, id: Uuid) -> Result<TrackerRow, AppError> {
    let row_opt = sqlx::query(
        "SELECT id, name, base_url, api_token, kind, plugin_path, created_at \
         FROM trackers WHERE id = ?",
    )
    .bind(id.to_string())
    .fetch_optional(pool)
    .await?;
    match row_opt {
        Some(row) => row_to_tracker(&row),
        None => Err(AppError::NotFound(format!("tracker {id}"))),
    }
}

/// Delete a tracker by UUID. Returns `true` if a row was removed,
/// `false` if no row matched.
///
/// # Errors
///
/// Returns [`AppError::Database`] on SQL failure.
pub async fn delete_by_id(pool: &Pool, id: Uuid) -> Result<bool, AppError> {
    let res = sqlx::query("DELETE FROM trackers WHERE id = ?")
        .bind(id.to_string())
        .execute(pool)
        .await?;
    Ok(res.rows_affected() > 0)
}

fn row_to_tracker(row: &SqliteRow) -> Result<TrackerRow, AppError> {
    let id_str: String = row.try_get("id")?;
    let id = Uuid::parse_str(&id_str)
        .map_err(|e| AppError::InvalidInput(format!("invalid uuid in trackers.id: {e}")))?;
    let base_url_str: String = row.try_get("base_url")?;
    let base_url = Url::parse(&base_url_str)
        .map_err(|e| AppError::InvalidInput(format!("invalid url in trackers.base_url: {e}")))?;
    let created_unix: i64 = row.try_get("created_at")?;
    let created_at = OffsetDateTime::from_unix_timestamp(created_unix)
        .map_err(|e| AppError::InvalidInput(format!("invalid timestamp: {e}")))?;
    let plugin_path_str: Option<String> = row.try_get("plugin_path")?;
    Ok(TrackerRow {
        id,
        name: row.try_get("name")?,
        base_url,
        api_token: row.try_get("api_token")?,
        kind: row.try_get("kind")?,
        plugin_path: plugin_path_str.map(PathBuf::from),
        created_at,
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use crate::db::open_memory;

    fn nt<'a>(
        name: &'a str,
        base_url: &'a Url,
        api_token: &'a str,
        kind: &'a str,
    ) -> NewTracker<'a> {
        NewTracker {
            name,
            base_url,
            api_token,
            kind,
            plugin_path: None,
        }
    }

    #[tokio::test]
    async fn insert_and_list_roundtrips() {
        let pool = open_memory().await.unwrap();
        let url = Url::parse("https://capybarabr.com/").unwrap();
        let row = insert(&pool, nt("capybara", &url, "tok", "unit3d"))
            .await
            .unwrap();
        assert_eq!(row.name, "capybara");
        assert_eq!(row.base_url, url);
        assert!(row.plugin_path.is_none());
        assert!(!row.is_plugin());

        let all = list_all(&pool).await.unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].name, "capybara");
        assert_eq!(all[0].id, row.id);
    }

    #[tokio::test]
    async fn duplicate_name_violates_unique() {
        let pool = open_memory().await.unwrap();
        let url = Url::parse("https://x.com/").unwrap();
        insert(&pool, nt("dupe", &url, "t1", "unit3d"))
            .await
            .unwrap();
        let err = insert(&pool, nt("dupe", &url, "t2", "unit3d"))
            .await
            .unwrap_err();
        assert!(matches!(err, AppError::Database(_)));
    }

    #[tokio::test]
    async fn empty_name_rejected() {
        let pool = open_memory().await.unwrap();
        let url = Url::parse("https://x.com/").unwrap();
        let err = insert(&pool, nt("  ", &url, "t", "unit3d"))
            .await
            .unwrap_err();
        assert!(matches!(err, AppError::InvalidInput(_)));
    }

    #[tokio::test]
    async fn delete_returns_true_only_when_row_existed() {
        let pool = open_memory().await.unwrap();
        let url = Url::parse("https://x.com/").unwrap();
        let row = insert(&pool, nt("t", &url, "tok", "unit3d")).await.unwrap();
        assert!(delete_by_id(&pool, row.id).await.unwrap());
        assert!(!delete_by_id(&pool, row.id).await.unwrap());
        assert!(list_all(&pool).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn plugin_path_roundtrips_through_db() {
        let pool = open_memory().await.unwrap();
        let url = Url::parse("https://plugin.example/").unwrap();
        let p = std::path::Path::new("/tmp/test.wasm");
        let row = insert(
            &pool,
            NewTracker {
                name: "scrap-plugin",
                base_url: &url,
                api_token: "",
                kind: "plugin",
                plugin_path: Some(p),
            },
        )
        .await
        .unwrap();
        assert_eq!(row.plugin_path.as_deref(), Some(p));
        assert!(row.is_plugin());

        let refreshed = get_by_id(&pool, row.id).await.unwrap();
        assert_eq!(refreshed.plugin_path.as_deref(), Some(p));
    }

    #[tokio::test]
    async fn get_by_id_404s_when_missing() {
        let pool = open_memory().await.unwrap();
        let err = get_by_id(&pool, Uuid::new_v4()).await.unwrap_err();
        assert!(matches!(err, AppError::NotFound(_)));
    }
}
