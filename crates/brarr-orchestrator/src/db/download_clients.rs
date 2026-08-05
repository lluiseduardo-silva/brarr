//! `download_clients` table — where a grab actually goes.
//!
//! See `migrations/20260804130000_download_clients.sql` for the schema
//! rationale. The admin UI writes through this module; the delivery path
//! will read through [`pick_for_protocol`] to choose which client gets a
//! given release.
//!
//! Two conventions worth knowing before editing:
//!
//! - **Protocol is derived, never stored.** [`DownloadClientRow::protocol`]
//!   asks the kind. A column would be a second source of truth that can
//!   drift from the first.
//! - **Secrets are write-only in the edit path.** [`update`] takes
//!   `None` for `password` / `api_key` to mean "keep what is stored",
//!   mirroring how `/settings` treats the TMDB token: the form never
//!   echoes a credential back, so a blank field cannot be read as "erase
//!   it". Clearing a credential outright means deleting and recreating
//!   the row.

use brarr_download_client::{DownloadClientConfig, DownloadClientKind, Protocol};
use sqlx::{Row, sqlite::SqliteRow};
use time::OffsetDateTime;
use url::Url;
use uuid::Uuid;

use crate::{AppError, db::Pool};

/// One configured download client.
#[derive(Debug, Clone)]
pub struct DownloadClientRow {
    /// Stable UUID v4 used in URLs and `grabs.client_id`.
    pub id: Uuid,
    /// Operator-chosen display name. Unique.
    pub name: String,
    /// qBittorrent or SABnzbd.
    pub kind: DownloadClientKind,
    /// Base URL of the web interface.
    pub base_url: Url,
    /// qBittorrent WebUI user. `None` when the instance bypasses auth.
    pub username: Option<String>,
    /// qBittorrent WebUI password.
    pub password: Option<String>,
    /// SABnzbd apikey.
    pub api_key: Option<String>,
    /// Category / label downloads are filed under. `None` leaves the
    /// client's own default alone.
    pub category: Option<String>,
    /// `false` takes the client out of rotation without losing its
    /// configuration (drain mode), mirroring `providers.enabled`.
    pub enabled: bool,
    /// Tie-break among clients serving the same protocol. Lowest wins.
    pub priority: u32,
    /// Row creation timestamp.
    pub created_at: OffsetDateTime,
}

impl DownloadClientRow {
    /// Transport this client serves — a function of [`Self::kind`].
    #[must_use]
    pub fn protocol(&self) -> Protocol {
        self.kind.protocol()
    }

    /// Snapshot for [`brarr_download_client::build`]. Cheap clone; the
    /// HTTP client is built separately by the caller.
    #[must_use]
    pub fn to_config(&self) -> DownloadClientConfig {
        DownloadClientConfig {
            name: self.name.clone(),
            kind: self.kind,
            base_url: self.base_url.clone(),
            username: self.username.clone(),
            password: self.password.clone(),
            api_key: self.api_key.clone(),
            category: self.category.clone(),
        }
    }
}

/// Values used to create a new download client row.
#[derive(Debug, Clone)]
pub struct NewDownloadClient<'a> {
    /// Display name. Must be unique.
    pub name: &'a str,
    /// Which program.
    pub kind: DownloadClientKind,
    /// Base URL of the web interface.
    pub base_url: &'a Url,
    /// qBittorrent user, when the WebUI requires a login.
    pub username: Option<&'a str>,
    /// qBittorrent password.
    pub password: Option<&'a str>,
    /// SABnzbd apikey. Required for that kind.
    pub api_key: Option<&'a str>,
    /// Category / label.
    pub category: Option<&'a str>,
    /// Selection tie-break. Defaults to `1` when `None`.
    pub priority: Option<u32>,
    /// Enabled flag. Defaults to `true` when `None`.
    pub enabled: Option<bool>,
}

/// Fields the edit form may overwrite. `kind` is deliberately absent:
/// swapping qBittorrent ↔ SABnzbd changes which credential fields mean
/// anything *and* the protocol every linked grab was routed under, so
/// the operator should delete and recreate instead.
#[derive(Debug, Clone)]
pub struct DownloadClientUpdate<'a> {
    /// New display name.
    pub name: &'a str,
    /// New base URL.
    pub base_url: &'a Url,
    /// New username. `None` clears it (qBittorrent auth bypass).
    pub username: Option<&'a str>,
    /// New password. `None` **keeps the stored one** — see the module
    /// docs.
    pub password: Option<&'a str>,
    /// New apikey. `None` **keeps the stored one**.
    pub api_key: Option<&'a str>,
    /// New category. `None` clears it.
    pub category: Option<&'a str>,
    /// New selection tie-break.
    pub priority: u32,
}

const CLIENT_COLUMNS: &str = "id, name, kind, base_url, username, password, api_key, category, \
     enabled, priority, created_at";

/// Normalise an optional string field: trimmed, and empty reads as
/// absent. Keeps `""` out of the database, where it would be a third
/// state alongside NULL and a real value.
fn clean(value: Option<&str>) -> Option<&str> {
    value.map(str::trim).filter(|v| !v.is_empty())
}

/// SABnzbd has no authentication mechanism other than the apikey, so a
/// row without one can never work. Caught here rather than at grab time.
fn require_credentials(kind: DownloadClientKind, api_key: Option<&str>) -> Result<(), AppError> {
    if kind == DownloadClientKind::Sabnzbd && api_key.is_none() {
        return Err(AppError::InvalidInput(
            "SABnzbd exige uma apikey (Config → General → API Key)".into(),
        ));
    }
    Ok(())
}

/// Insert a new download client, returning the persisted row.
///
/// # Errors
///
/// - [`AppError::InvalidInput`] when the name is blank or a SABnzbd row
///   carries no apikey.
/// - [`AppError::Database`] on `UNIQUE(name)` violation or other SQL
///   error.
pub async fn insert(
    pool: &Pool,
    new: NewDownloadClient<'_>,
) -> Result<DownloadClientRow, AppError> {
    let name = new.name.trim();
    if name.is_empty() {
        return Err(AppError::InvalidInput(
            "download client name cannot be empty".into(),
        ));
    }
    let username = clean(new.username);
    let password = clean(new.password);
    let api_key = clean(new.api_key);
    let category = clean(new.category);
    require_credentials(new.kind, api_key)?;

    let id = Uuid::new_v4();
    let now = OffsetDateTime::now_utc();
    let priority = new.priority.unwrap_or(1);
    let enabled = new.enabled.unwrap_or(true);

    sqlx::query(
        "INSERT INTO download_clients \
         (id, name, kind, base_url, username, password, api_key, category, enabled, priority, created_at) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(id.to_string())
    .bind(name)
    .bind(new.kind.label())
    .bind(new.base_url.as_str())
    .bind(username)
    .bind(password)
    .bind(api_key)
    .bind(category)
    .bind(i64::from(u8::from(enabled)))
    .bind(i64::from(priority))
    .bind(now.unix_timestamp())
    .execute(pool)
    .await?;

    get_by_id(pool, id).await
}

/// Every download client, enabled first then by priority and name — the
/// order the admin table renders.
///
/// # Errors
///
/// Returns [`AppError::Database`] on SQL failure.
pub async fn list_all(pool: &Pool) -> Result<Vec<DownloadClientRow>, AppError> {
    let rows = sqlx::query(&format!(
        "SELECT {CLIENT_COLUMNS} FROM download_clients \
         ORDER BY enabled DESC, priority ASC, name ASC"
    ))
    .fetch_all(pool)
    .await?;
    rows.iter().map(row_to_client).collect()
}

/// Only the clients currently in rotation.
///
/// # Errors
///
/// Returns [`AppError::Database`] on SQL failure.
pub async fn list_enabled(pool: &Pool) -> Result<Vec<DownloadClientRow>, AppError> {
    let rows = sqlx::query(&format!(
        "SELECT {CLIENT_COLUMNS} FROM download_clients WHERE enabled = 1 \
         ORDER BY priority ASC, name ASC"
    ))
    .fetch_all(pool)
    .await?;
    rows.iter().map(row_to_client).collect()
}

/// The client a release of `protocol` should be handed to: enabled,
/// lowest priority wins, name breaks a tie. `None` when nothing is
/// configured for that transport — which the delivery path must report
/// rather than silently drop the grab.
///
/// The protocol match happens in Rust, not SQL, so the kind → protocol
/// mapping stays in one place ([`DownloadClientKind::protocol`]).
///
/// # Errors
///
/// Returns [`AppError::Database`] on SQL failure.
pub async fn pick_for_protocol(
    pool: &Pool,
    protocol: Protocol,
) -> Result<Option<DownloadClientRow>, AppError> {
    let rows = list_enabled(pool).await?;
    Ok(rows.into_iter().find(|r| r.protocol() == protocol))
}

/// Fetch one row by id.
///
/// # Errors
///
/// Returns [`AppError::NotFound`] when no row matches.
pub async fn get_by_id(pool: &Pool, id: Uuid) -> Result<DownloadClientRow, AppError> {
    let row = sqlx::query(&format!(
        "SELECT {CLIENT_COLUMNS} FROM download_clients WHERE id = ?"
    ))
    .bind(id.to_string())
    .fetch_optional(pool)
    .await?;
    match row {
        Some(r) => row_to_client(&r),
        None => Err(AppError::NotFound(format!("download_client {id}"))),
    }
}

/// Rewrite the editable fields of one row.
///
/// # Errors
///
/// - [`AppError::InvalidInput`] when the name is blank, or when the row
///   is a SABnzbd one and the update would leave it without an apikey.
/// - [`AppError::NotFound`] when no row matches `id`.
/// - [`AppError::Database`] on SQL failure (including `UNIQUE(name)`).
pub async fn update(
    pool: &Pool,
    id: Uuid,
    upd: DownloadClientUpdate<'_>,
) -> Result<DownloadClientRow, AppError> {
    let name = upd.name.trim();
    if name.is_empty() {
        return Err(AppError::InvalidInput(
            "download client name cannot be empty".into(),
        ));
    }
    let current = get_by_id(pool, id).await?;
    let api_key = clean(upd.api_key);
    // `None` means keep — so the check has to run against what the row
    // will hold afterwards, not against what the form sent.
    let effective_key = api_key
        .map(str::to_owned)
        .or_else(|| current.api_key.clone());
    require_credentials(current.kind, effective_key.as_deref())?;

    sqlx::query(
        "UPDATE download_clients SET name = ?, base_url = ?, username = ?, \
         password = COALESCE(?, password), api_key = COALESCE(?, api_key), \
         category = ?, priority = ? WHERE id = ?",
    )
    .bind(name)
    .bind(upd.base_url.as_str())
    .bind(clean(upd.username))
    .bind(clean(upd.password))
    .bind(api_key)
    .bind(clean(upd.category))
    .bind(i64::from(upd.priority))
    .bind(id.to_string())
    .execute(pool)
    .await?;

    get_by_id(pool, id).await
}

/// Flip the `enabled` flag in place. Mirrors
/// [`crate::db::providers::set_enabled`].
///
/// # Errors
///
/// - [`AppError::NotFound`] when no row matches `id`.
/// - [`AppError::Database`] on SQL failure.
pub async fn set_enabled(
    pool: &Pool,
    id: Uuid,
    enabled: bool,
) -> Result<DownloadClientRow, AppError> {
    let res = sqlx::query("UPDATE download_clients SET enabled = ? WHERE id = ?")
        .bind(i64::from(u8::from(enabled)))
        .bind(id.to_string())
        .execute(pool)
        .await?;
    if res.rows_affected() == 0 {
        return Err(AppError::NotFound(format!("download_client {id}")));
    }
    get_by_id(pool, id).await
}

/// Delete a row by id. Returns `true` when a row was removed.
///
/// Grabs that went through this client keep their history: the FK is
/// `ON DELETE SET NULL`, so `grabs.client_id` is blanked and nothing
/// else changes.
///
/// # Errors
///
/// Returns [`AppError::Database`] on SQL failure.
pub async fn delete_by_id(pool: &Pool, id: Uuid) -> Result<bool, AppError> {
    let res = sqlx::query("DELETE FROM download_clients WHERE id = ?")
        .bind(id.to_string())
        .execute(pool)
        .await?;
    Ok(res.rows_affected() > 0)
}

fn row_to_client(row: &SqliteRow) -> Result<DownloadClientRow, AppError> {
    let id_raw: String = row.try_get("id")?;
    let id = Uuid::parse_str(&id_raw)
        .map_err(|e| AppError::InvalidInput(format!("invalid uuid in download_clients.id: {e}")))?;
    let kind_raw: String = row.try_get("kind")?;
    let kind = DownloadClientKind::from_label(&kind_raw).ok_or_else(|| {
        AppError::InvalidInput(format!("unknown download_clients.kind: {kind_raw}"))
    })?;
    let base_raw: String = row.try_get("base_url")?;
    let base_url = Url::parse(&base_raw).map_err(|e| {
        AppError::InvalidInput(format!("invalid url in download_clients.base_url: {e}"))
    })?;
    let enabled: i64 = row.try_get("enabled")?;
    let priority: i64 = row.try_get("priority")?;
    let created: i64 = row.try_get("created_at")?;
    Ok(DownloadClientRow {
        id,
        name: row.try_get("name")?,
        kind,
        base_url,
        username: row.try_get("username")?,
        password: row.try_get("password")?,
        api_key: row.try_get("api_key")?,
        category: row.try_get("category")?,
        enabled: enabled != 0,
        priority: u32::try_from(priority).unwrap_or(1),
        created_at: OffsetDateTime::from_unix_timestamp(created).map_err(|e| {
            AppError::InvalidInput(format!("invalid download_clients.created_at: {e}"))
        })?,
    })
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

    fn qb<'a>(name: &'a str, base: &'a Url) -> NewDownloadClient<'a> {
        NewDownloadClient {
            name,
            kind: DownloadClientKind::Qbittorrent,
            base_url: base,
            username: Some("admin"),
            password: Some("hunter2"),
            api_key: None,
            category: Some("brarr"),
            priority: None,
            enabled: None,
        }
    }

    fn sab<'a>(name: &'a str, base: &'a Url, key: Option<&'a str>) -> NewDownloadClient<'a> {
        NewDownloadClient {
            name,
            kind: DownloadClientKind::Sabnzbd,
            base_url: base,
            username: None,
            password: None,
            api_key: key,
            category: None,
            priority: None,
            enabled: None,
        }
    }

    fn url(s: &str) -> Url {
        Url::parse(s).unwrap()
    }

    #[tokio::test]
    async fn insert_and_list_roundtrips() {
        let pool = open_memory().await.unwrap();
        let base = url("http://10.0.1.246:8080/");
        let row = insert(&pool, qb("qbittorrent-main", &base)).await.unwrap();
        assert_eq!(row.kind, DownloadClientKind::Qbittorrent);
        assert_eq!(row.protocol(), Protocol::Torrent);
        assert_eq!(row.username.as_deref(), Some("admin"));
        assert_eq!(row.priority, 1);
        assert!(row.enabled);

        let all = list_all(&pool).await.unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].name, "qbittorrent-main");
    }

    #[tokio::test]
    async fn duplicate_name_violates_unique() {
        let pool = open_memory().await.unwrap();
        let base = url("http://x.example/");
        insert(&pool, qb("dupe", &base)).await.unwrap();
        let err = insert(&pool, qb("dupe", &base)).await.unwrap_err();
        assert!(matches!(err, AppError::Database(_)));
    }

    #[tokio::test]
    async fn empty_name_rejected() {
        let pool = open_memory().await.unwrap();
        let base = url("http://x.example/");
        let err = insert(&pool, qb("   ", &base)).await.unwrap_err();
        assert!(matches!(err, AppError::InvalidInput(_)));
    }

    #[tokio::test]
    async fn a_sabnzbd_row_without_an_apikey_is_refused_at_config_time() {
        let pool = open_memory().await.unwrap();
        let base = url("http://10.0.1.246:8085/");
        let err = insert(&pool, sab("sab", &base, None)).await.unwrap_err();
        assert!(matches!(err, AppError::InvalidInput(_)));
        // Whitespace is not a key either.
        let err = insert(&pool, sab("sab", &base, Some("  ")))
            .await
            .unwrap_err();
        assert!(matches!(err, AppError::InvalidInput(_)));
        // qBittorrent, on the other hand, may legitimately have no
        // credentials at all — the WebUI can bypass authentication.
        let mut no_creds = qb("qb", &base);
        no_creds.username = None;
        no_creds.password = None;
        assert!(insert(&pool, no_creds).await.is_ok());
    }

    #[tokio::test]
    async fn blank_optional_fields_are_stored_as_null_not_empty_string() {
        let pool = open_memory().await.unwrap();
        let base = url("http://x.example/");
        let mut new = qb("blanks", &base);
        new.category = Some("   ");
        new.username = Some("");
        let row = insert(&pool, new).await.unwrap();
        assert!(row.category.is_none());
        assert!(row.username.is_none());
    }

    #[tokio::test]
    async fn pick_for_protocol_picks_by_priority_then_name() {
        let pool = open_memory().await.unwrap();
        let base = url("http://x.example/");
        let mut low = qb("zzz-preferido", &base);
        low.priority = Some(1);
        let mut high = qb("aaa-reserva", &base);
        high.priority = Some(5);
        insert(&pool, high).await.unwrap();
        insert(&pool, low).await.unwrap();
        insert(&pool, sab("sab", &base, Some("k"))).await.unwrap();

        let torrent = pick_for_protocol(&pool, Protocol::Torrent)
            .await
            .unwrap()
            .expect("a qBittorrent client is configured");
        assert_eq!(
            torrent.name, "zzz-preferido",
            "priority wins over alphabetical order"
        );
        let usenet = pick_for_protocol(&pool, Protocol::Usenet)
            .await
            .unwrap()
            .expect("a SABnzbd client is configured");
        assert_eq!(usenet.name, "sab");
    }

    #[tokio::test]
    async fn pick_for_protocol_skips_disabled_clients() {
        let pool = open_memory().await.unwrap();
        let base = url("http://x.example/");
        let row = insert(&pool, qb("qb", &base)).await.unwrap();
        set_enabled(&pool, row.id, false).await.unwrap();
        assert!(
            pick_for_protocol(&pool, Protocol::Torrent)
                .await
                .unwrap()
                .is_none(),
            "a drained client must not receive grabs"
        );
        assert_eq!(
            list_all(&pool).await.unwrap().len(),
            1,
            "but it still exists"
        );
    }

    #[tokio::test]
    async fn pick_for_protocol_is_none_when_nothing_serves_the_transport() {
        let pool = open_memory().await.unwrap();
        let base = url("http://x.example/");
        insert(&pool, qb("qb", &base)).await.unwrap();
        assert!(
            pick_for_protocol(&pool, Protocol::Usenet)
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn update_keeps_the_stored_secret_when_the_form_sends_nothing() {
        let pool = open_memory().await.unwrap();
        let base = url("http://x.example/");
        let row = insert(&pool, sab("sab", &base, Some("original-key")))
            .await
            .unwrap();

        let renamed = url("http://novo.example/sabnzbd/");
        let updated = update(
            &pool,
            row.id,
            DownloadClientUpdate {
                name: "sab-renomeado",
                base_url: &renamed,
                username: None,
                password: None,
                api_key: None,
                category: Some("usenet"),
                priority: 3,
            },
        )
        .await
        .unwrap();

        assert_eq!(updated.name, "sab-renomeado");
        assert_eq!(updated.priority, 3);
        assert_eq!(updated.category.as_deref(), Some("usenet"));
        assert_eq!(
            updated.api_key.as_deref(),
            Some("original-key"),
            "a blank credential field means 'keep', not 'erase'"
        );
    }

    #[tokio::test]
    async fn update_replaces_the_secret_when_one_is_supplied() {
        let pool = open_memory().await.unwrap();
        let base = url("http://x.example/");
        let row = insert(&pool, sab("sab", &base, Some("old"))).await.unwrap();
        let updated = update(
            &pool,
            row.id,
            DownloadClientUpdate {
                name: "sab",
                base_url: &base,
                username: None,
                password: None,
                api_key: Some("new-key"),
                category: None,
                priority: 1,
            },
        )
        .await
        .unwrap();
        assert_eq!(updated.api_key.as_deref(), Some("new-key"));
    }

    #[tokio::test]
    async fn update_404s_on_unknown_id() {
        let pool = open_memory().await.unwrap();
        let base = url("http://x.example/");
        let err = update(
            &pool,
            Uuid::new_v4(),
            DownloadClientUpdate {
                name: "x",
                base_url: &base,
                username: None,
                password: None,
                api_key: None,
                category: None,
                priority: 1,
            },
        )
        .await
        .unwrap_err();
        assert!(matches!(err, AppError::NotFound(_)));
    }

    #[tokio::test]
    async fn delete_returns_true_only_when_a_row_existed() {
        let pool = open_memory().await.unwrap();
        let base = url("http://x.example/");
        let row = insert(&pool, qb("qb", &base)).await.unwrap();
        assert!(delete_by_id(&pool, row.id).await.unwrap());
        assert!(!delete_by_id(&pool, row.id).await.unwrap());
    }

    #[tokio::test]
    async fn get_by_id_404s_when_missing() {
        let pool = open_memory().await.unwrap();
        let err = get_by_id(&pool, Uuid::new_v4()).await.unwrap_err();
        assert!(matches!(err, AppError::NotFound(_)));
    }

    #[tokio::test]
    async fn the_row_converts_into_a_client_config() {
        let pool = open_memory().await.unwrap();
        let base = url("http://10.0.1.246:8080/");
        let row = insert(&pool, qb("qbittorrent-main", &base)).await.unwrap();
        let config = row.to_config();
        assert_eq!(config.name, "qbittorrent-main");
        assert_eq!(config.kind, DownloadClientKind::Qbittorrent);
        assert_eq!(config.base_url, base);
        assert_eq!(config.password.as_deref(), Some("hunter2"));
        // And it builds a live client without touching the network.
        let client = brarr_download_client::build(config).unwrap();
        assert_eq!(client.name(), "qbittorrent-main");
    }

    #[tokio::test]
    async fn deleting_a_client_blanks_its_grabs_without_erasing_them() {
        use crate::db::grabs::{self, NewGrab};
        use crate::db::library::{self, MediaType, NewLibraryItem};

        let pool = open_memory().await.unwrap();
        let base = url("http://x.example/");
        let client = insert(&pool, qb("qb", &base)).await.unwrap();
        let item = library::upsert(
            &pool,
            &NewLibraryItem {
                media_type: Some(MediaType::Movie),
                tmdb_id: 603,
                title: "The Matrix".to_owned(),
                ..NewLibraryItem::default()
            },
        )
        .await
        .unwrap();
        let provider = crate::db::providers::insert(
            &pool,
            crate::db::providers::NewProvider {
                name: "capybara",
                base_url: &url("https://capybarabr.com/"),
                api_token: "tok",
                kind: "unit3d",
                plugin_path: None,
            },
        )
        .await
        .unwrap();
        let grab = grabs::reserve(
            &pool,
            &NewGrab {
                item_id: item.id,
                episode_id: None,
                season_number: None,
                decision_id: None,
                provider_id: provider.id,
                provider_name: "capybara",
                release_id_remote: "abc",
                release_name: "Matrix.1999.1080p",
                download_url: None,
                protocol: grabs::Protocol::Torrent,
            },
        )
        .await
        .unwrap()
        .unwrap();
        sqlx::query("UPDATE grabs SET client_id = ? WHERE id = ?")
            .bind(client.id.to_string())
            .bind(grab.id.to_string())
            .execute(&pool)
            .await
            .unwrap();

        assert!(delete_by_id(&pool, client.id).await.unwrap());

        let still_there = grabs::get_by_id(&pool, grab.id).await.unwrap();
        assert_eq!(still_there.release_name, "Matrix.1999.1080p");
        let client_id: Option<String> = sqlx::query("SELECT client_id FROM grabs WHERE id = ?")
            .bind(grab.id.to_string())
            .fetch_one(&pool)
            .await
            .unwrap()
            .try_get("client_id")
            .unwrap();
        assert!(
            client_id.is_none(),
            "the FK is ON DELETE SET NULL — history survives the client"
        );
    }
}
