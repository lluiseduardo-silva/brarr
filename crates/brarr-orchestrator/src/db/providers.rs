//! Provider rows in SQLite.
//!
//! The orchestrator owns the canonical provider list at runtime — the
//! admin UI writes through this module. A "provider" is any source of
//! release data: a UNIT3D torrent tracker, a Newznab Usenet indexer, a
//! Torznab gateway (Jackett/Prowlarr), or a WASM plugin. The previous
//! revision of this schema called the table `trackers`; the
//! `20260516120000_rename_to_providers.sql` migration renames it
//! in-place. `brarr-cli` keeps its TOML-based flow unchanged for now; a
//! future phase can teach the CLI to read from the orchestrator via
//! gRPC.

use std::path::PathBuf;

use sqlx::{Row, sqlite::SqliteRow};
use time::OffsetDateTime;
use url::Url;
use uuid::Uuid;

use crate::{AppError, db::Pool};

/// A configured provider.
#[derive(Debug, Clone)]
pub struct ProviderRow {
    /// Stable UUID v4 used in URLs and gRPC payloads.
    pub id: Uuid,
    /// Human-friendly provider name (e.g. `capybarabr`). Must be unique.
    pub name: String,
    /// Base URL of the provider.
    pub base_url: Url,
    /// API token. Stored as plaintext for now; encryption-at-rest is a
    /// future hardening (the DB itself sits on local disk owned by the
    /// service user, not exposed externally).
    pub api_token: String,
    /// Provider family: `unit3d`, `newznab`, `torznab`, or `plugin`.
    pub kind: String,
    /// Filesystem path to a `.wasm`/`.wat` plugin module. `None` means
    /// the provider is served by one of the built-in HTTP clients
    /// (selected by `kind`).
    pub plugin_path: Option<PathBuf>,
    /// `false` removes the provider from the search fan-out without
    /// deleting its config (drain mode + targeted-testing toggle).
    pub enabled: bool,
    /// Row creation timestamp.
    pub created_at: OffsetDateTime,
}

impl ProviderRow {
    /// `true` when this row drives a WASM plugin (`plugin_path` set).
    #[must_use]
    pub fn is_plugin(&self) -> bool {
        self.plugin_path.is_some()
    }
}

/// Bundle of values used to create a new provider row.
#[derive(Debug, Clone)]
pub struct NewProvider<'a> {
    /// Display name (must be unique).
    pub name: &'a str,
    /// Provider base URL.
    pub base_url: &'a Url,
    /// API token (bearer for UNIT3D, apikey for Newznab/Torznab; free-form for plugins).
    pub api_token: &'a str,
    /// `"unit3d"`, `"newznab"`, `"torznab"`, or `"plugin"`.
    pub kind: &'a str,
    /// Optional plugin filesystem path.
    pub plugin_path: Option<&'a std::path::Path>,
}

/// Insert a new provider, returning the persisted row (with id + timestamp).
///
/// # Errors
///
/// Returns [`AppError::Database`] on `UNIQUE` violations of `name`, or
/// any other SQL error.
pub async fn insert(pool: &Pool, new: NewProvider<'_>) -> Result<ProviderRow, AppError> {
    if new.name.trim().is_empty() {
        return Err(AppError::InvalidInput(
            "provider name cannot be empty".into(),
        ));
    }
    let id = Uuid::new_v4();
    let now = OffsetDateTime::now_utc();
    let base = new.base_url.as_str();
    let id_str = id.to_string();
    let created_unix = now.unix_timestamp();
    let plugin_path_str = new.plugin_path.map(|p| p.to_string_lossy().into_owned());
    sqlx::query(
        "INSERT INTO providers (id, name, base_url, api_token, kind, plugin_path, enabled, created_at) \
         VALUES (?, ?, ?, ?, ?, ?, 1, ?)",
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

    Ok(ProviderRow {
        id,
        name: new.name.to_string(),
        base_url: new.base_url.clone(),
        api_token: new.api_token.to_string(),
        kind: new.kind.to_string(),
        plugin_path: new.plugin_path.map(PathBuf::from),
        enabled: true,
        created_at: now,
    })
}

/// Flip the `enabled` flag for one row in place. Returns the refreshed
/// row.
///
/// # Errors
///
/// - [`AppError::NotFound`] when no row matches.
/// - [`AppError::Database`] on SQL failure.
pub async fn set_enabled(pool: &Pool, id: Uuid, enabled: bool) -> Result<ProviderRow, AppError> {
    let res = sqlx::query("UPDATE providers SET enabled = ? WHERE id = ?")
        .bind(i64::from(u8::from(enabled)))
        .bind(id.to_string())
        .execute(pool)
        .await?;
    if res.rows_affected() == 0 {
        return Err(AppError::NotFound(format!("provider {id}")));
    }
    get_by_id(pool, id).await
}

/// List all providers, ordered by `name` ascending.
///
/// # Errors
///
/// Returns [`AppError::Database`] on SQL failure.
pub async fn list_all(pool: &Pool) -> Result<Vec<ProviderRow>, AppError> {
    let rows = sqlx::query(
        "SELECT id, name, base_url, api_token, kind, plugin_path, enabled, created_at \
         FROM providers ORDER BY name ASC",
    )
    .fetch_all(pool)
    .await?;
    rows.iter().map(row_to_provider).collect()
}

/// Fetch a provider by UUID.
///
/// # Errors
///
/// Returns [`AppError::NotFound`] if no row matches.
pub async fn get_by_id(pool: &Pool, id: Uuid) -> Result<ProviderRow, AppError> {
    let row_opt = sqlx::query(
        "SELECT id, name, base_url, api_token, kind, plugin_path, enabled, created_at \
         FROM providers WHERE id = ?",
    )
    .bind(id.to_string())
    .fetch_optional(pool)
    .await?;
    match row_opt {
        Some(row) => row_to_provider(&row),
        None => Err(AppError::NotFound(format!("provider {id}"))),
    }
}

/// Delete a provider by UUID. Returns `true` if a row was removed,
/// `false` if no row matched.
///
/// # Errors
///
/// Returns [`AppError::Database`] on SQL failure.
pub async fn delete_by_id(pool: &Pool, id: Uuid) -> Result<bool, AppError> {
    let res = sqlx::query("DELETE FROM providers WHERE id = ?")
        .bind(id.to_string())
        .execute(pool)
        .await?;
    Ok(res.rows_affected() > 0)
}

/// Bundle of fields the edit form may overwrite.
#[derive(Debug, Clone)]
pub struct ProviderUpdate<'a> {
    /// New display name (must stay unique).
    pub name: &'a str,
    /// New base URL.
    pub base_url: &'a Url,
    /// New api token.
    pub api_token: &'a str,
    /// New kind label (`unit3d` / `newznab` / `torznab` / `plugin`).
    pub kind: &'a str,
    /// New plugin filesystem path. `None` clears the column.
    pub plugin_path: Option<&'a std::path::Path>,
}

/// Rewrite the editable fields of one row in place. The `enabled`
/// flag and `created_at` timestamp are left untouched — toggling
/// enabled goes through [`set_enabled`].
///
/// # Errors
///
/// - [`AppError::InvalidInput`] when `name` is empty.
/// - [`AppError::NotFound`] when no row matches `id`.
/// - [`AppError::Database`] on `UNIQUE(name)` violation or other SQL
///   error.
pub async fn update(
    pool: &Pool,
    id: Uuid,
    upd: ProviderUpdate<'_>,
) -> Result<ProviderRow, AppError> {
    if upd.name.trim().is_empty() {
        return Err(AppError::InvalidInput(
            "provider name cannot be empty".into(),
        ));
    }
    let plugin_path_str = upd.plugin_path.map(|p| p.to_string_lossy().into_owned());
    let res = sqlx::query(
        "UPDATE providers SET name = ?, base_url = ?, api_token = ?, kind = ?, plugin_path = ? \
         WHERE id = ?",
    )
    .bind(upd.name)
    .bind(upd.base_url.as_str())
    .bind(upd.api_token)
    .bind(upd.kind)
    .bind(&plugin_path_str)
    .bind(id.to_string())
    .execute(pool)
    .await?;
    if res.rows_affected() == 0 {
        return Err(AppError::NotFound(format!("provider {id}")));
    }
    get_by_id(pool, id).await
}

fn row_to_provider(row: &SqliteRow) -> Result<ProviderRow, AppError> {
    let id_str: String = row.try_get("id")?;
    let id = Uuid::parse_str(&id_str)
        .map_err(|e| AppError::InvalidInput(format!("invalid uuid in providers.id: {e}")))?;
    let base_url_str: String = row.try_get("base_url")?;
    let base_url = Url::parse(&base_url_str)
        .map_err(|e| AppError::InvalidInput(format!("invalid url in providers.base_url: {e}")))?;
    let created_unix: i64 = row.try_get("created_at")?;
    let created_at = OffsetDateTime::from_unix_timestamp(created_unix)
        .map_err(|e| AppError::InvalidInput(format!("invalid timestamp: {e}")))?;
    let plugin_path_str: Option<String> = row.try_get("plugin_path")?;
    // Legacy rows before the 20260517160000 migration won't have the
    // column. `try_get` returns the SQLite default (1) so this stays
    // backward-compatible.
    let enabled_i64: i64 = row.try_get::<i64, _>("enabled").unwrap_or(1);
    Ok(ProviderRow {
        id,
        name: row.try_get("name")?,
        base_url,
        api_token: row.try_get("api_token")?,
        kind: row.try_get("kind")?,
        plugin_path: plugin_path_str.map(PathBuf::from),
        enabled: enabled_i64 != 0,
        created_at,
    })
}

/// Return only the providers with `enabled = 1`. Search fan-out uses
/// this so disabled rows are skipped without losing their config.
///
/// # Errors
///
/// Returns [`AppError::Database`] on SQL failure.
pub async fn list_enabled(pool: &Pool) -> Result<Vec<ProviderRow>, AppError> {
    let rows = sqlx::query(
        "SELECT id, name, base_url, api_token, kind, plugin_path, enabled, created_at \
         FROM providers WHERE enabled = 1 ORDER BY name ASC",
    )
    .fetch_all(pool)
    .await?;
    rows.iter().map(row_to_provider).collect()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use crate::db::open_memory;

    fn np<'a>(
        name: &'a str,
        base_url: &'a Url,
        api_token: &'a str,
        kind: &'a str,
    ) -> NewProvider<'a> {
        NewProvider {
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
        let row = insert(&pool, np("capybara", &url, "tok", "unit3d"))
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
        insert(&pool, np("dupe", &url, "t1", "unit3d"))
            .await
            .unwrap();
        let err = insert(&pool, np("dupe", &url, "t2", "unit3d"))
            .await
            .unwrap_err();
        assert!(matches!(err, AppError::Database(_)));
    }

    #[tokio::test]
    async fn empty_name_rejected() {
        let pool = open_memory().await.unwrap();
        let url = Url::parse("https://x.com/").unwrap();
        let err = insert(&pool, np("  ", &url, "t", "unit3d"))
            .await
            .unwrap_err();
        assert!(matches!(err, AppError::InvalidInput(_)));
    }

    #[tokio::test]
    async fn delete_returns_true_only_when_row_existed() {
        let pool = open_memory().await.unwrap();
        let url = Url::parse("https://x.com/").unwrap();
        let row = insert(&pool, np("t", &url, "tok", "unit3d")).await.unwrap();
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
            NewProvider {
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
