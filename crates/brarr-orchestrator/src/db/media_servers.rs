//! `media_servers` — Plex, Jellyfin and Emby, and how the last
//! notification went.
//!
//! See `migrations/20260818120000_media_servers.sql` for the schema
//! rationale. Two conventions carried over from
//! [`crate::db::download_clients`], for the same reasons:
//!
//! - **The dialect is derived, never stored.**
//!   [`MediaServerRow::api`] asks the kind. A column would be a second
//!   source of truth able to disagree with `kind`.
//! - **Secrets are write-only in the edit path.** [`update`] reads
//!   `None` for `token` as "keep what is stored". The modal never echoes
//!   a credential back, so blank cannot mean "erase".
//!
//! One convention of its own: [`set_token`] exists separately from
//! [`update`] because the Plex sign-in writes a token nobody typed into
//! a form, and routing that through the form path would mean the form
//! path has to understand a flow it has nothing to do with.

use time::OffsetDateTime;
use url::Url;
use uuid::Uuid;

use brarr_media_server::{MediaServerApi, MediaServerConfig, MediaServerKind};
use sqlx::{Row, sqlite::SqliteRow};

use crate::{AppError, db::Pool};

/// One configured media server.
#[derive(Debug, Clone)]
pub struct MediaServerRow {
    /// Stable UUID v4.
    pub id: Uuid,
    /// Operator-chosen display name, unique.
    pub name: String,
    /// Which server this is.
    pub kind: MediaServerKind,
    /// Base URL, including any reverse-proxy path prefix.
    pub base_url: String,
    /// `X-Plex-Token` or `X-MediaBrowser-Token`. `None` on a Plex row
    /// that has not been signed in yet.
    pub token: Option<String>,
    /// Drain mode: `false` stops notifications without losing the row.
    pub enabled: bool,
    /// Row creation timestamp.
    pub created_at: OffsetDateTime,
    /// When the last notification succeeded.
    pub last_notified_at: Option<OffsetDateTime>,
    /// Why the last notification failed, cleared by the next success.
    pub last_error: Option<String>,
}

impl MediaServerRow {
    /// Which HTTP dialect this row speaks. Derived from `kind`.
    #[must_use]
    pub fn api(&self) -> MediaServerApi {
        self.kind.api()
    }

    /// `true` when a credential is stored. Never exposes the value —
    /// the edit modal renders this, not the token.
    #[must_use]
    pub fn has_token(&self) -> bool {
        self.token.as_deref().is_some_and(|t| !t.trim().is_empty())
    }

    /// Build the client configuration for this row.
    ///
    /// # Errors
    ///
    /// [`AppError::InvalidInput`] when `base_url` does not parse. It is
    /// validated on the way in, so this only fires on a row edited
    /// outside brarr.
    pub fn to_config(&self) -> Result<MediaServerConfig, AppError> {
        let base_url = Url::parse(&self.base_url)
            .map_err(|e| AppError::InvalidInput(format!("URL inválida em {}: {e}", self.name)))?;
        Ok(MediaServerConfig {
            name: self.name.clone(),
            kind: self.kind,
            base_url,
            token: self.token.clone(),
        })
    }
}

/// Values used to create a server.
#[derive(Debug, Clone, Copy)]
pub struct NewMediaServer<'a> {
    /// Display name.
    pub name: &'a str,
    /// Which server.
    pub kind: MediaServerKind,
    /// Base URL.
    pub base_url: &'a str,
    /// Credential, when the operator has one to paste. A Plex row is
    /// normally created without and signed in afterwards.
    pub token: Option<&'a str>,
}

/// Values used to edit a server.
///
/// `kind` is deliberately absent, exactly as it is in
/// [`crate::db::download_clients::DownloadClientUpdate`]: changing it
/// would change the dialect and the authentication scheme under a
/// credential obtained for the old one.
#[derive(Debug, Clone, Copy)]
pub struct MediaServerUpdate<'a> {
    /// Display name.
    pub name: &'a str,
    /// Base URL.
    pub base_url: &'a str,
    /// `None` means "keep the stored credential".
    pub token: Option<&'a str>,
}

const COLUMNS: &str =
    "id, name, kind, base_url, token, enabled, created_at, last_notified_at, last_error";

/// Trim, and treat an empty string as absent.
///
/// Keeps `""` out of the database, where it would be a third state
/// alongside NULL and a real value.
fn clean(value: Option<&str>) -> Option<&str> {
    value.map(str::trim).filter(|v| !v.is_empty())
}

/// Reject a URL brarr could never call.
fn validate_url(raw: &str) -> Result<String, AppError> {
    let trimmed = raw.trim();
    let url = Url::parse(trimmed).map_err(|e| {
        AppError::InvalidInput(format!(
            "URL inválida ({e}) — informe algo como http://10.0.1.248:32400"
        ))
    })?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(AppError::InvalidInput(
            "a URL precisa começar com http:// ou https://".into(),
        ));
    }
    Ok(trimmed.to_owned())
}

fn row_to_server(row: &SqliteRow) -> Result<MediaServerRow, AppError> {
    let id: String = row.try_get("id")?;
    let kind: String = row.try_get("kind")?;
    let created_at: i64 = row.try_get("created_at")?;
    let last_notified_at: Option<i64> = row.try_get("last_notified_at")?;
    let enabled: i64 = row.try_get("enabled")?;
    Ok(MediaServerRow {
        id: Uuid::parse_str(&id)
            .map_err(|e| AppError::InvalidInput(format!("invalid media_server id: {e}")))?,
        name: row.try_get("name")?,
        // A kind outside the CHECK cannot be stored, so this only fires
        // on a row written by hand.
        kind: MediaServerKind::from_label(&kind).ok_or_else(|| {
            AppError::InvalidInput(format!("tipo de media server desconhecido: {kind}"))
        })?,
        base_url: row.try_get("base_url")?,
        token: row.try_get("token")?,
        enabled: enabled != 0,
        created_at: OffsetDateTime::from_unix_timestamp(created_at)
            .map_err(|e| AppError::InvalidInput(format!("invalid created_at: {e}")))?,
        last_notified_at: last_notified_at
            .map(OffsetDateTime::from_unix_timestamp)
            .transpose()
            .map_err(|e| AppError::InvalidInput(format!("invalid last_notified_at: {e}")))?,
        last_error: row.try_get("last_error")?,
    })
}

/// Register a server.
///
/// # Errors
///
/// - [`AppError::InvalidInput`] when the name is blank, the URL does not
///   parse, or the name is taken.
/// - [`AppError::Database`] on any other SQL failure.
pub async fn insert(pool: &Pool, new: NewMediaServer<'_>) -> Result<MediaServerRow, AppError> {
    let name = new.name.trim();
    if name.is_empty() {
        return Err(AppError::InvalidInput("o nome não pode ser vazio".into()));
    }
    let base_url = validate_url(new.base_url)?;
    let token = clean(new.token);

    let id = Uuid::new_v4();
    let now = OffsetDateTime::now_utc();
    let result = sqlx::query(
        "INSERT INTO media_servers (id, name, kind, base_url, token, enabled, created_at) \
         VALUES (?, ?, ?, ?, ?, 1, ?)",
    )
    .bind(id.to_string())
    .bind(name)
    .bind(new.kind.label())
    .bind(&base_url)
    .bind(token)
    .bind(now.unix_timestamp())
    .execute(pool)
    .await;

    if let Err(sqlx::Error::Database(db)) = &result {
        if db.is_unique_violation() {
            return Err(AppError::InvalidInput(format!(
                "já existe um servidor chamado {name}"
            )));
        }
    }
    result?;

    Ok(MediaServerRow {
        id,
        name: name.to_owned(),
        kind: new.kind,
        base_url,
        token: token.map(str::to_owned),
        enabled: true,
        created_at: now,
        last_notified_at: None,
        last_error: None,
    })
}

/// Every server, for the admin screen.
///
/// # Errors
///
/// Returns [`AppError::Database`] on SQL failure.
pub async fn list_all(pool: &Pool) -> Result<Vec<MediaServerRow>, AppError> {
    let rows = sqlx::query(&format!(
        "SELECT {COLUMNS} FROM media_servers ORDER BY enabled DESC, name ASC"
    ))
    .fetch_all(pool)
    .await?;
    rows.iter().map(row_to_server).collect()
}

/// Every server that should hear about a change.
///
/// # Errors
///
/// Returns [`AppError::Database`] on SQL failure.
pub async fn list_enabled(pool: &Pool) -> Result<Vec<MediaServerRow>, AppError> {
    let rows = sqlx::query(&format!(
        "SELECT {COLUMNS} FROM media_servers WHERE enabled = 1 ORDER BY name ASC"
    ))
    .fetch_all(pool)
    .await?;
    rows.iter().map(row_to_server).collect()
}

/// One row by id.
///
/// # Errors
///
/// Returns [`AppError::NotFound`] when absent, [`AppError::Database`] on
/// SQL failure.
pub async fn get_by_id(pool: &Pool, id: Uuid) -> Result<MediaServerRow, AppError> {
    let row = sqlx::query(&format!("SELECT {COLUMNS} FROM media_servers WHERE id = ?"))
        .bind(id.to_string())
        .fetch_optional(pool)
        .await?;
    match row {
        Some(row) => row_to_server(&row),
        None => Err(AppError::NotFound(format!("media_server {id}"))),
    }
}

/// Edit a server, keeping the stored credential when none is supplied.
///
/// `COALESCE(?, token)` in the SQL is what makes `None` mean "keep": the
/// modal never echoes a secret back, so a blank field cannot be read as
/// "erase it".
///
/// # Errors
///
/// - [`AppError::NotFound`] when the row is gone.
/// - [`AppError::InvalidInput`] when the name is blank or taken, or the
///   URL does not parse.
/// - [`AppError::Database`] on any other SQL failure.
pub async fn update(
    pool: &Pool,
    id: Uuid,
    update: MediaServerUpdate<'_>,
) -> Result<MediaServerRow, AppError> {
    get_by_id(pool, id).await?;

    let name = update.name.trim();
    if name.is_empty() {
        return Err(AppError::InvalidInput("o nome não pode ser vazio".into()));
    }
    let base_url = validate_url(update.base_url)?;

    let result = sqlx::query(
        "UPDATE media_servers SET name = ?, base_url = ?, token = COALESCE(?, token) \
         WHERE id = ?",
    )
    .bind(name)
    .bind(&base_url)
    .bind(clean(update.token))
    .bind(id.to_string())
    .execute(pool)
    .await;

    if let Err(sqlx::Error::Database(db)) = &result {
        if db.is_unique_violation() {
            return Err(AppError::InvalidInput(format!(
                "já existe um servidor chamado {name}"
            )));
        }
    }
    result?;

    get_by_id(pool, id).await
}

/// Store a credential obtained outside the form — the Plex sign-in.
///
/// Also clears `last_error`: a fresh token invalidates whatever the last
/// failure said, and leaving a stale "token refused" beside a working
/// login is the kind of lie a screen never recovers from.
///
/// # Errors
///
/// [`AppError::NotFound`] when the row is gone, [`AppError::Database`]
/// on SQL failure.
pub async fn set_token(pool: &Pool, id: Uuid, token: &str) -> Result<(), AppError> {
    let res = sqlx::query("UPDATE media_servers SET token = ?, last_error = NULL WHERE id = ?")
        .bind(token.trim())
        .bind(id.to_string())
        .execute(pool)
        .await?;
    if res.rows_affected() == 0 {
        return Err(AppError::NotFound(format!("media_server {id}")));
    }
    Ok(())
}

/// Flip drain mode. Returns the new value.
///
/// # Errors
///
/// [`AppError::NotFound`] when the row is gone, [`AppError::Database`]
/// on SQL failure.
pub async fn set_enabled(pool: &Pool, id: Uuid, enabled: bool) -> Result<bool, AppError> {
    let res = sqlx::query("UPDATE media_servers SET enabled = ? WHERE id = ?")
        .bind(i64::from(enabled))
        .bind(id.to_string())
        .execute(pool)
        .await?;
    if res.rows_affected() == 0 {
        return Err(AppError::NotFound(format!("media_server {id}")));
    }
    Ok(enabled)
}

/// Record that a notification went through.
///
/// # Errors
///
/// Returns [`AppError::Database`] on SQL failure.
pub async fn mark_notified(pool: &Pool, id: Uuid) -> Result<(), AppError> {
    sqlx::query("UPDATE media_servers SET last_notified_at = ?, last_error = NULL WHERE id = ?")
        .bind(OffsetDateTime::now_utc().unix_timestamp())
        .bind(id.to_string())
        .execute(pool)
        .await?;
    Ok(())
}

/// Record why a notification did not.
///
/// Deliberately does **not** touch `last_notified_at`: "it worked at
/// 14:02 and has been failing since" is two facts, and collapsing them
/// loses the one that says whether this ever worked at all.
///
/// # Errors
///
/// Returns [`AppError::Database`] on SQL failure.
pub async fn mark_notify_error(pool: &Pool, id: Uuid, error: &str) -> Result<(), AppError> {
    sqlx::query("UPDATE media_servers SET last_error = ? WHERE id = ?")
        .bind(error)
        .bind(id.to_string())
        .execute(pool)
        .await?;
    Ok(())
}

/// Remove a server. `true` when a row went away.
///
/// Its path mappings go with it (ON DELETE CASCADE): a mapping without a
/// server is dead configuration.
///
/// # Errors
///
/// Returns [`AppError::Database`] on SQL failure.
pub async fn delete_by_id(pool: &Pool, id: Uuid) -> Result<bool, AppError> {
    let res = sqlx::query("DELETE FROM media_servers WHERE id = ?")
        .bind(id.to_string())
        .execute(pool)
        .await?;
    Ok(res.rows_affected() > 0)
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "tests assert on happy paths"
)]
mod tests {
    use super::*;
    use crate::db;

    async fn pool() -> Pool {
        db::open(":memory:").await.expect("in-memory db")
    }

    fn new(name: &str, kind: MediaServerKind) -> NewMediaServer<'_> {
        NewMediaServer {
            name,
            kind,
            base_url: "http://10.0.1.248:32400",
            token: None,
        }
    }

    #[tokio::test]
    async fn a_plex_row_starts_without_a_credential() {
        let pool = pool().await;
        let row = insert(&pool, new("Plex", MediaServerKind::Plex))
            .await
            .unwrap();
        assert!(
            !row.has_token(),
            "the token arrives from the sign-in, not from the create form"
        );
        assert!(row.enabled);
    }

    #[tokio::test]
    async fn the_dialect_follows_the_kind_and_is_not_stored() {
        let pool = pool().await;
        for (name, kind, api) in [
            ("Plex", MediaServerKind::Plex, MediaServerApi::Plex),
            (
                "Jellyfin",
                MediaServerKind::Jellyfin,
                MediaServerApi::MediaBrowser,
            ),
            ("Emby", MediaServerKind::Emby, MediaServerApi::MediaBrowser),
        ] {
            let row = insert(&pool, new(name, kind)).await.unwrap();
            assert_eq!(row.api(), api);
        }
        // Nothing in the table names a dialect; asking the row is the
        // only way to get one.
        let stored: Vec<String> = sqlx::query("SELECT kind FROM media_servers")
            .fetch_all(&pool)
            .await
            .unwrap()
            .iter()
            .map(|r| r.get::<String, _>("kind"))
            .collect();
        assert_eq!(stored.len(), 3);
    }

    #[tokio::test]
    async fn update_keeps_the_stored_token_when_the_form_sends_nothing() {
        let pool = pool().await;
        let row = insert(
            &pool,
            NewMediaServer {
                token: Some("segredo"),
                ..new("Jellyfin", MediaServerKind::Jellyfin)
            },
        )
        .await
        .unwrap();

        let after = update(
            &pool,
            row.id,
            MediaServerUpdate {
                name: "Jellyfin renomeado",
                base_url: "http://10.0.1.9:8096",
                token: None,
            },
        )
        .await
        .unwrap();

        assert_eq!(after.name, "Jellyfin renomeado");
        assert_eq!(
            after.token.as_deref(),
            Some("segredo"),
            "a blank field means keep, because the modal never echoed it"
        );
    }

    #[tokio::test]
    async fn update_replaces_the_token_when_one_is_supplied() {
        let pool = pool().await;
        let row = insert(
            &pool,
            NewMediaServer {
                token: Some("velho"),
                ..new("Emby", MediaServerKind::Emby)
            },
        )
        .await
        .unwrap();

        let after = update(
            &pool,
            row.id,
            MediaServerUpdate {
                name: "Emby",
                base_url: "http://10.0.1.224:8096",
                token: Some("novo"),
            },
        )
        .await
        .unwrap();
        assert_eq!(after.token.as_deref(), Some("novo"));
    }

    #[tokio::test]
    async fn signing_in_stores_the_token_and_clears_the_last_failure() {
        let pool = pool().await;
        let row = insert(&pool, new("Plex", MediaServerKind::Plex))
            .await
            .unwrap();
        mark_notify_error(&pool, row.id, "o Plex recusou o token")
            .await
            .unwrap();

        set_token(&pool, row.id, "  um-token-novo  ").await.unwrap();

        let after = get_by_id(&pool, row.id).await.unwrap();
        assert_eq!(after.token.as_deref(), Some("um-token-novo"), "trimmed");
        assert_eq!(
            after.last_error, None,
            "a stale refusal beside a fresh login is a lie the screen never recovers from"
        );
    }

    #[tokio::test]
    async fn a_failure_does_not_erase_when_it_last_worked() {
        let pool = pool().await;
        let row = insert(&pool, new("Plex", MediaServerKind::Plex))
            .await
            .unwrap();
        mark_notified(&pool, row.id).await.unwrap();
        mark_notify_error(&pool, row.id, "host fora do ar")
            .await
            .unwrap();

        let after = get_by_id(&pool, row.id).await.unwrap();
        assert!(
            after.last_notified_at.is_some(),
            "\"it worked at 14:02 and has failed since\" is two facts"
        );
        assert_eq!(after.last_error.as_deref(), Some("host fora do ar"));
    }

    #[tokio::test]
    async fn a_duplicate_name_is_form_input_and_not_a_500() {
        let pool = pool().await;
        insert(&pool, new("Plex", MediaServerKind::Plex))
            .await
            .unwrap();
        let err = insert(&pool, new("Plex", MediaServerKind::Jellyfin))
            .await
            .expect_err("the name is taken");
        assert!(matches!(err, AppError::InvalidInput(_)), "got {err:?}");
    }

    #[tokio::test]
    async fn a_url_without_a_scheme_is_refused_before_it_reaches_a_request() {
        let pool = pool().await;
        let err = insert(
            &pool,
            NewMediaServer {
                base_url: "10.0.1.248:32400",
                ..new("Plex", MediaServerKind::Plex)
            },
        )
        .await
        .expect_err("no scheme");
        assert!(matches!(err, AppError::InvalidInput(_)), "got {err:?}");
    }

    #[tokio::test]
    async fn disabling_keeps_the_row_out_of_the_notify_set() {
        let pool = pool().await;
        let row = insert(&pool, new("Plex", MediaServerKind::Plex))
            .await
            .unwrap();
        set_enabled(&pool, row.id, false).await.unwrap();
        assert!(list_enabled(&pool).await.unwrap().is_empty());
        assert_eq!(list_all(&pool).await.unwrap().len(), 1, "drained, not gone");
    }
}
