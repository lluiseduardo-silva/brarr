//! `grabs` — one row per acquisition attempt, and the idempotency
//! barrier that makes double-grabbing impossible.
//!
//! See `migrations/20260804120000_library.sql` for schema notes.
//!
//! The barrier is the whole point of this module. The path it replaces
//! read like this:
//!
//! ```text
//! SELECT COUNT(*) FROM push_history WHERE ...   -- has it been pushed?
//! POST /api/v3/release/push                      -- HTTP round-trip
//! INSERT INTO push_history ...                   -- record it
//! ```
//!
//! with no transaction, no unique index, and an unconditional
//! `Uuid::new_v4()` on the insert. The window between the check and the
//! write spanned the entire round-trip, so two tasks that started
//! together both passed the check and both pushed.
//!
//! Wrapping that in a transaction does not fix it: it would hold a SQLite
//! write lock open across a network call. Instead [`reserve`] inserts a
//! `reserved` row *before* any network work and lets the unique index
//! decide the winner. Losers get `Ok(None)` and simply stop.

use sqlx::{Row, sqlite::SqliteRow};
use time::OffsetDateTime;
use uuid::Uuid;

use crate::{AppError, db::Pool};

/// Transport the release travels over.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Protocol {
    /// BitTorrent — UNIT3D trackers and Torznab indexers.
    Torrent,
    /// Usenet — Newznab indexers.
    Usenet,
    /// Neither: a file that was already on disk when brarr met it, taken
    /// over by the library adoption. It has no download client and no
    /// provider — only a path.
    Local,
}

impl Protocol {
    /// Short tag for the `protocol` column.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Torrent => "torrent",
            Self::Usenet => "usenet",
            Self::Local => "local",
        }
    }

    /// Parse from the persisted label.
    ///
    /// # Errors
    ///
    /// Returns [`AppError::InvalidInput`] for anything the CHECK
    /// constraint would have rejected.
    pub fn from_label(s: &str) -> Result<Self, AppError> {
        match s {
            "torrent" => Ok(Self::Torrent),
            "usenet" => Ok(Self::Usenet),
            "local" => Ok(Self::Local),
            other => Err(AppError::InvalidInput(format!(
                "unknown grabs.protocol: {other}"
            ))),
        }
    }
}

/// Lifecycle of one acquisition.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GrabStatus {
    /// The idempotency gate: written before any network call. A row in
    /// this state means "someone is working on it, keep out".
    Reserved,
    /// Handed to the download client.
    Sent,
    /// The client reports it in flight.
    Downloading,
    /// Files are on disk, not yet moved into the library.
    Completed,
    /// Moved and renamed into the library (phase 2).
    Imported,
    /// Gave up. `error` carries the reason.
    Failed,
    /// Refused before download — below cutoff, blocklisted, etc.
    Rejected,
}

impl GrabStatus {
    /// Short tag for the `status` column.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Reserved => "reserved",
            Self::Sent => "sent",
            Self::Downloading => "downloading",
            Self::Completed => "completed",
            Self::Imported => "imported",
            Self::Failed => "failed",
            Self::Rejected => "rejected",
        }
    }

    /// Parse from the persisted label.
    ///
    /// # Errors
    ///
    /// Returns [`AppError::InvalidInput`] for anything the CHECK
    /// constraint would have rejected.
    pub fn from_label(s: &str) -> Result<Self, AppError> {
        match s {
            "reserved" => Ok(Self::Reserved),
            "sent" => Ok(Self::Sent),
            "downloading" => Ok(Self::Downloading),
            "completed" => Ok(Self::Completed),
            "imported" => Ok(Self::Imported),
            "failed" => Ok(Self::Failed),
            "rejected" => Ok(Self::Rejected),
            other => Err(AppError::InvalidInput(format!(
                "unknown grabs.status: {other}"
            ))),
        }
    }

    /// Whether the grab still occupies the item — an active grab is what
    /// stops the scanner from starting a second one.
    #[must_use]
    pub fn is_active(self) -> bool {
        matches!(self, Self::Reserved | Self::Sent | Self::Downloading)
    }

    /// Whether a grab in this state means "this item is taken care of",
    /// so the scanner must not search for it again.
    ///
    /// This is brarr's answer to "do I already have the file?" — the
    /// operator chose to infer it from the acquisition record rather
    /// than by walking the library on every sweep. Everything except the
    /// two terminal failures counts: `failed` and `rejected` are exactly
    /// the states where trying again is right.
    ///
    /// **Status alone is not the whole answer.** A grab whose imported
    /// file has since vanished carries `file_missing_at` and stops
    /// covering its item, which is why the queries filter on that column
    /// rather than calling this. See [`crate::verify`].
    #[must_use]
    pub fn blocks_search(self) -> bool {
        !matches!(self, Self::Failed | Self::Rejected)
    }
}

/// One acquisition attempt.
#[derive(Debug, Clone)]
pub struct Grab {
    /// Stable UUID v4.
    pub id: Uuid,
    /// Catalogue entry being acquired.
    pub item_id: Uuid,
    /// Set for a per-episode grab; `None` for a movie or season pack.
    pub episode_id: Option<Uuid>,
    /// Set for a season pack.
    pub season_number: Option<i32>,
    /// Scoring decision snapshot. `None` once `decisions` is pruned.
    pub decision_id: Option<Uuid>,
    /// Provider the release came from. `None` once the provider is gone.
    pub provider_id: Option<Uuid>,
    /// Provider name snapshot, survives provider deletion.
    pub provider_name: String,
    /// Provider-side release identifier. TEXT because Newznab guids are
    /// not numeric.
    pub release_id_remote: String,
    /// Release title snapshot.
    pub release_name: String,
    /// Where to fetch the `.torrent` / `.nzb`.
    pub download_url: Option<String>,
    /// Transport.
    pub protocol: Protocol,
    /// Download client the release was handed to. `None` before the
    /// hand-off, and after the client row is deleted (`ON DELETE SET
    /// NULL` — the history outlives the client).
    pub client_id: Option<Uuid>,
    /// Handle the client answered with, when it gives one (SABnzbd's
    /// `nzo_id`). qBittorrent answers a bare `Ok.`, so `None` there is
    /// normal rather than a failure.
    pub client_item_id: Option<String>,
    /// Lifecycle position.
    pub status: GrabStatus,
    /// Failure reason when [`GrabStatus::Failed`].
    pub error: Option<String>,
    /// Where the import placed the file. `None` until the grab reaches
    /// [`GrabStatus::Imported`] — and the only record of which path in
    /// the library belongs to this acquisition.
    pub imported_path: Option<String>,
    /// When the verification pass found [`Self::imported_path`] gone.
    /// A grab in this state stops covering its item *and* releases its
    /// barrier key, so the same release can be acquired again — usually
    /// exactly what an operator who deleted a file by accident wants.
    pub file_missing_at: Option<OffsetDateTime>,
    /// When the reservation was taken.
    pub grabbed_at: OffsetDateTime,
    /// Last status change.
    pub updated_at: OffsetDateTime,
}

/// Everything needed to take a reservation.
#[derive(Debug, Clone)]
pub struct NewGrab<'a> {
    /// Catalogue entry.
    pub item_id: Uuid,
    /// Episode, for a per-episode grab.
    pub episode_id: Option<Uuid>,
    /// Season number, for a season pack.
    pub season_number: Option<i32>,
    /// Scoring decision that chose this release.
    pub decision_id: Option<Uuid>,
    /// Provider. Required: the uniqueness key is built on it, and a NULL
    /// would opt the row out of the barrier entirely.
    pub provider_id: Uuid,
    /// Provider name snapshot.
    pub provider_name: &'a str,
    /// Provider-side release identifier.
    pub release_id_remote: &'a str,
    /// Release title snapshot.
    pub release_name: &'a str,
    /// Where to fetch the release file.
    pub download_url: Option<&'a str>,
    /// Transport.
    pub protocol: Protocol,
}

const GRAB_COLUMNS: &str = "id, item_id, episode_id, season_number, decision_id, provider_id, \
     provider_name, release_id_remote, release_name, download_url, protocol, \
     client_id, client_item_id, status, error, imported_path, file_missing_at, \
     grabbed_at, updated_at";

fn opt_uuid_at(row: &SqliteRow, col: &str) -> Result<Option<Uuid>, AppError> {
    let raw: Option<String> = row.try_get(col)?;
    match raw {
        Some(s) => Ok(Some(Uuid::parse_str(&s).map_err(|e| {
            AppError::InvalidInput(format!("invalid uuid in grabs.{col}: {e}"))
        })?)),
        None => Ok(None),
    }
}

fn row_to_grab(row: &SqliteRow) -> Result<Grab, AppError> {
    let id_raw: String = row.try_get("id")?;
    let id = Uuid::parse_str(&id_raw)
        .map_err(|e| AppError::InvalidInput(format!("invalid uuid in grabs.id: {e}")))?;
    let item_raw: String = row.try_get("item_id")?;
    let item_id = Uuid::parse_str(&item_raw)
        .map_err(|e| AppError::InvalidInput(format!("invalid uuid in grabs.item_id: {e}")))?;
    let protocol_raw: String = row.try_get("protocol")?;
    let status_raw: String = row.try_get("status")?;
    let grabbed: i64 = row.try_get("grabbed_at")?;
    let updated: i64 = row.try_get("updated_at")?;
    Ok(Grab {
        id,
        item_id,
        episode_id: opt_uuid_at(row, "episode_id")?,
        season_number: row
            .try_get::<Option<i64>, _>("season_number")?
            .and_then(|v| i32::try_from(v).ok()),
        decision_id: opt_uuid_at(row, "decision_id")?,
        provider_id: opt_uuid_at(row, "provider_id")?,
        provider_name: row.try_get("provider_name")?,
        release_id_remote: row.try_get("release_id_remote")?,
        release_name: row.try_get("release_name")?,
        download_url: row.try_get("download_url")?,
        protocol: Protocol::from_label(&protocol_raw)?,
        client_id: opt_uuid_at(row, "client_id")?,
        client_item_id: row.try_get("client_item_id")?,
        status: GrabStatus::from_label(&status_raw)?,
        error: row.try_get("error")?,
        imported_path: row.try_get("imported_path")?,
        file_missing_at: row
            .try_get::<Option<i64>, _>("file_missing_at")?
            .and_then(|ts| OffsetDateTime::from_unix_timestamp(ts).ok()),
        grabbed_at: OffsetDateTime::from_unix_timestamp(grabbed)
            .map_err(|e| AppError::InvalidInput(format!("invalid grabs.grabbed_at: {e}")))?,
        updated_at: OffsetDateTime::from_unix_timestamp(updated)
            .map_err(|e| AppError::InvalidInput(format!("invalid grabs.updated_at: {e}")))?,
    })
}

/// Take the reservation for a release **before** doing any network work.
///
/// Returns `Ok(Some(grab))` when this caller won the race and should go
/// on to hand the release to a download client, and `Ok(None)` when the
/// same `(provider, release, item[, episode])` was already reserved — in
/// which case the caller must stop, not retry.
///
/// This is the only correct place to decide "should I grab this?".
/// Checking first and inserting later reopens the window this closes.
///
/// # Errors
///
/// Returns [`AppError::Database`] on SQL failure.
pub async fn reserve(pool: &Pool, new: &NewGrab<'_>) -> Result<Option<Grab>, AppError> {
    let id = Uuid::new_v4();
    let now = OffsetDateTime::now_utc().unix_timestamp();
    // Bare `ON CONFLICT DO NOTHING` rather than `INSERT OR IGNORE`: the
    // latter also swallows CHECK and FK violations, which would turn a
    // genuine bug into a silent no-op.
    let res = sqlx::query(
        "INSERT INTO grabs ( \
            id, item_id, episode_id, season_number, decision_id, provider_id, \
            provider_name, release_id_remote, release_name, download_url, \
            protocol, status, grabbed_at, updated_at \
         ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, 'reserved', ?, ?) \
         ON CONFLICT DO NOTHING",
    )
    .bind(id.to_string())
    .bind(new.item_id.to_string())
    .bind(new.episode_id.map(|e| e.to_string()))
    .bind(new.season_number.map(i64::from))
    .bind(new.decision_id.map(|d| d.to_string()))
    .bind(new.provider_id.to_string())
    .bind(new.provider_name)
    .bind(new.release_id_remote)
    .bind(new.release_name)
    .bind(new.download_url)
    .bind(new.protocol.label())
    .bind(now)
    .bind(now)
    .execute(pool)
    .await?;

    if res.rows_affected() == 0 {
        return Ok(None);
    }
    get_by_id(pool, id).await.map(Some)
}

/// Fetch one grab by primary key.
///
/// # Errors
///
/// Returns [`AppError::NotFound`] when absent, [`AppError::Database`]
/// on SQL failure.
pub async fn get_by_id(pool: &Pool, id: Uuid) -> Result<Grab, AppError> {
    let row = sqlx::query(&format!("SELECT {GRAB_COLUMNS} FROM grabs WHERE id = ?"))
        .bind(id.to_string())
        .fetch_optional(pool)
        .await?;
    match row {
        Some(r) => row_to_grab(&r),
        None => Err(AppError::NotFound(format!("grab {id}"))),
    }
}

/// Advance a grab's lifecycle. `error` is only meaningful for
/// [`GrabStatus::Failed`] and is cleared otherwise so a later retry does
/// not carry a stale message.
///
/// # Errors
///
/// Returns [`AppError::NotFound`] when the id is absent,
/// [`AppError::Database`] on SQL failure.
pub async fn set_status(
    pool: &Pool,
    id: Uuid,
    status: GrabStatus,
    error: Option<&str>,
) -> Result<(), AppError> {
    let error = if status == GrabStatus::Failed {
        error
    } else {
        None
    };
    let res = sqlx::query("UPDATE grabs SET status = ?, error = ?, updated_at = ? WHERE id = ?")
        .bind(status.label())
        .bind(error)
        .bind(OffsetDateTime::now_utc().unix_timestamp())
        .bind(id.to_string())
        .execute(pool)
        .await?;
    if res.rows_affected() == 0 {
        return Err(AppError::NotFound(format!("grab {id}")));
    }
    Ok(())
}

/// Release a reservation that never made it out the door — a failed
/// hand-off to the download client, say. Deleting rather than marking
/// failed frees the uniqueness key so a later attempt at the same
/// release can proceed.
///
/// # Errors
///
/// Returns [`AppError::Database`] on SQL failure.
pub async fn release_reservation(pool: &Pool, id: Uuid) -> Result<(), AppError> {
    sqlx::query("DELETE FROM grabs WHERE id = ? AND status = 'reserved'")
        .bind(id.to_string())
        .execute(pool)
        .await?;
    Ok(())
}

/// Grabs still occupying an item — what the scanner checks before
/// starting a new one.
///
/// # Errors
///
/// Returns [`AppError::Database`] on SQL failure.
pub async fn active_for_item(pool: &Pool, item_id: Uuid) -> Result<Vec<Grab>, AppError> {
    let rows = sqlx::query(&format!(
        "SELECT {GRAB_COLUMNS} FROM grabs \
         WHERE item_id = ? AND status IN ('reserved', 'sent', 'downloading') \
         ORDER BY grabbed_at DESC"
    ))
    .bind(item_id.to_string())
    .fetch_all(pool)
    .await?;
    rows.iter().map(row_to_grab).collect()
}

/// Grabs that keep the scanner away from an item — see
/// [`GrabStatus::blocks_search`].
///
/// `target` says what is being asked about: the whole item (a movie), or
/// one episode. A grab covers the target when it names that episode, or
/// when it names no episode *and* its season does not exclude it:
///
/// | grab | covers |
/// |---|---|
/// | `episode_id = X` | episode X only |
/// | no episode, no season | the whole item — a movie, or a full-series grab |
/// | no episode, `season_number = 4` | every episode of season 4 |
///
/// That last row is why the season has to travel with the question. A
/// pack of season 4 used to satisfy the query for *any* episode, so
/// season 5 read as acquired the moment one pack landed.
///
/// # Errors
///
/// Returns [`AppError::Database`] on SQL failure.
pub async fn blocking_for(
    pool: &Pool,
    item_id: Uuid,
    target: GrabTarget,
) -> Result<Vec<Grab>, AppError> {
    let rows = sqlx::query(&format!(
        "SELECT {GRAB_COLUMNS} FROM grabs \
         WHERE item_id = ? AND status NOT IN ('failed', 'rejected') \
           AND file_missing_at IS NULL \
           AND ( \
                 episode_id IS ? \
              OR (episode_id IS NULL AND (season_number IS NULL OR season_number IS ?)) \
           ) \
         ORDER BY grabbed_at DESC"
    ))
    .bind(item_id.to_string())
    .bind(target.episode_id.map(|e| e.to_string()))
    .bind(target.season_number.map(i64::from))
    .fetch_all(pool)
    .await?;
    rows.iter().map(row_to_grab).collect()
}

/// What [`blocking_for`] is being asked about.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct GrabTarget {
    /// The episode in question. `None` asks about the item as a whole,
    /// which is what a movie always does.
    pub episode_id: Option<Uuid>,
    /// Season the episode belongs to, so a pack of another season does
    /// not answer for it. Ignored when `episode_id` is `None`.
    pub season_number: Option<i32>,
}

impl GrabTarget {
    /// The item as a whole — a movie, or "is anything covering this
    /// series at all".
    #[must_use]
    pub fn item() -> Self {
        Self::default()
    }

    /// One episode of a season.
    #[must_use]
    pub fn episode(episode_id: Uuid, season_number: i32) -> Self {
        Self {
            episode_id: Some(episode_id),
            season_number: Some(season_number),
        }
    }
}

/// Record a successful hand-off: the client accepted the release.
///
/// `client_item_id` is the handle the client answered with when it gives
/// one (SABnzbd's `nzo_id`); qBittorrent returns none, so `None` is a
/// normal outcome, not a failure.
///
/// # Errors
///
/// - [`AppError::NotFound`] when the id is absent.
/// - [`AppError::Database`] on SQL failure.
pub async fn mark_sent(
    pool: &Pool,
    id: Uuid,
    client_id: Uuid,
    client_item_id: Option<&str>,
) -> Result<(), AppError> {
    let res = sqlx::query(
        "UPDATE grabs SET status = 'sent', error = NULL, client_id = ?, \
         client_item_id = ?, updated_at = ? WHERE id = ?",
    )
    .bind(client_id.to_string())
    .bind(client_item_id)
    .bind(OffsetDateTime::now_utc().unix_timestamp())
    .bind(id.to_string())
    .execute(pool)
    .await?;
    if res.rows_affected() == 0 {
        return Err(AppError::NotFound(format!("grab {id}")));
    }
    Ok(())
}

/// Grabs the download client has finished with, waiting for the import
/// to move them into the library. Oldest first — the one that has been
/// waiting longest goes first.
///
/// # Errors
///
/// Returns [`AppError::Database`] on SQL failure.
pub async fn awaiting_import(pool: &Pool) -> Result<Vec<Grab>, AppError> {
    let rows = sqlx::query(&format!(
        "SELECT {GRAB_COLUMNS} FROM grabs WHERE status = 'completed' ORDER BY updated_at ASC"
    ))
    .fetch_all(pool)
    .await?;
    rows.iter().map(row_to_grab).collect()
}

/// Record a finished import: the grab is done, and `path` is where the
/// file now lives.
///
/// # Errors
///
/// - [`AppError::NotFound`] when the id is absent.
/// - [`AppError::Database`] on SQL failure.
pub async fn mark_imported(pool: &Pool, id: Uuid, path: &str) -> Result<(), AppError> {
    let res = sqlx::query(
        "UPDATE grabs SET status = 'imported', error = NULL, imported_path = ?, \
         updated_at = ? WHERE id = ?",
    )
    .bind(path)
    .bind(OffsetDateTime::now_utc().unix_timestamp())
    .bind(id.to_string())
    .execute(pool)
    .await?;
    if res.rows_affected() == 0 {
        return Err(AppError::NotFound(format!("grab {id}")));
    }
    Ok(())
}

/// Imported grabs whose file is still believed to be on disk — the set
/// the verification pass checks.
///
/// # Errors
///
/// Returns [`AppError::Database`] on SQL failure.
pub async fn imported_present(pool: &Pool) -> Result<Vec<Grab>, AppError> {
    let rows = sqlx::query(&format!(
        "SELECT {GRAB_COLUMNS} FROM grabs \
         WHERE status = 'imported' AND file_missing_at IS NULL \
           AND imported_path IS NOT NULL \
         ORDER BY updated_at ASC"
    ))
    .fetch_all(pool)
    .await?;
    rows.iter().map(row_to_grab).collect()
}

/// Record that an imported file is no longer where brarr put it.
///
/// The status stays `imported` — it *was* imported, and rewriting that
/// would erase what happened. The timestamp is what stops the grab
/// covering its item, and what frees its barrier key.
///
/// # Errors
///
/// - [`AppError::NotFound`] when the id is absent.
/// - [`AppError::Database`] on SQL failure.
pub async fn mark_file_missing(pool: &Pool, id: Uuid) -> Result<(), AppError> {
    let now = OffsetDateTime::now_utc().unix_timestamp();
    let res = sqlx::query("UPDATE grabs SET file_missing_at = ?, updated_at = ? WHERE id = ?")
        .bind(now)
        .bind(now)
        .bind(id.to_string())
        .execute(pool)
        .await?;
    if res.rows_affected() == 0 {
        return Err(AppError::NotFound(format!("grab {id}")));
    }
    Ok(())
}

/// Every grab for an item, newest first.
///
/// # Errors
///
/// Returns [`AppError::Database`] on SQL failure.
pub async fn for_item(pool: &Pool, item_id: Uuid) -> Result<Vec<Grab>, AppError> {
    let rows = sqlx::query(&format!(
        "SELECT {GRAB_COLUMNS} FROM grabs WHERE item_id = ? ORDER BY grabbed_at DESC"
    ))
    .bind(item_id.to_string())
    .fetch_all(pool)
    .await?;
    rows.iter().map(row_to_grab).collect()
}

/// The whole queue: everything not yet finished, oldest first so the UI
/// shows what has been waiting longest at the top.
///
/// # Errors
///
/// Returns [`AppError::Database`] on SQL failure.
pub async fn queue(pool: &Pool) -> Result<Vec<Grab>, AppError> {
    let rows = sqlx::query(&format!(
        "SELECT {GRAB_COLUMNS} FROM grabs \
         WHERE status IN ('reserved', 'sent', 'downloading', 'completed') \
         ORDER BY grabbed_at ASC"
    ))
    .fetch_all(pool)
    .await?;
    rows.iter().map(row_to_grab).collect()
}

/// Most recent grabs across the whole library.
///
/// # Errors
///
/// Returns [`AppError::Database`] on SQL failure.
pub async fn recent(pool: &Pool, limit: u32) -> Result<Vec<Grab>, AppError> {
    let limit = match limit {
        0 => 50,
        n if n > 500 => 500,
        n => n,
    };
    let rows = sqlx::query(&format!(
        "SELECT {GRAB_COLUMNS} FROM grabs ORDER BY grabbed_at DESC LIMIT ?"
    ))
    .bind(i64::from(limit))
    .fetch_all(pool)
    .await?;
    rows.iter().map(row_to_grab).collect()
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "tests assert on happy paths"
)]
mod tests {
    use super::*;
    use crate::db::{
        library::{self, MediaType, NewEpisode, NewLibraryItem, NewSeason},
        open_memory,
    };

    async fn fixture(pool: &Pool) -> (Uuid, Uuid) {
        let item = library::upsert(
            pool,
            &NewLibraryItem {
                media_type: Some(MediaType::Movie),
                tmdb_id: 603,
                title: "The Matrix".to_owned(),
                ..NewLibraryItem::default()
            },
        )
        .await
        .unwrap();
        // The provider FK is real, so insert through the same path the
        // app uses rather than hand-rolling the INSERT.
        let base_url = url::Url::parse("https://capybarabr.com/").unwrap();
        let provider = crate::db::providers::insert(
            pool,
            crate::db::providers::NewProvider {
                name: "capybara",
                base_url: &base_url,
                api_token: "tok",
                kind: "unit3d",
                plugin_path: None,
            },
        )
        .await
        .unwrap();
        (item.id, provider.id)
    }

    fn new_grab(item_id: Uuid, provider_id: Uuid, remote: &str) -> NewGrab<'_> {
        NewGrab {
            item_id,
            episode_id: None,
            season_number: None,
            decision_id: None,
            provider_id,
            provider_name: "capybara",
            release_id_remote: remote,
            release_name: "Matrix.1999.1080p.BluRay.PT-BR",
            download_url: Some("https://capybarabr.com/torrent/1"),
            protocol: Protocol::Torrent,
        }
    }

    #[tokio::test]
    async fn reserve_returns_the_grab_on_first_call() {
        let pool = open_memory().await.unwrap();
        let (item_id, provider_id) = fixture(&pool).await;
        let grab = reserve(&pool, &new_grab(item_id, provider_id, "abc"))
            .await
            .unwrap();
        let grab = grab.expect("first reservation wins");
        assert_eq!(grab.status, GrabStatus::Reserved);
        assert_eq!(grab.release_id_remote, "abc");
    }

    #[tokio::test]
    async fn second_reservation_of_the_same_release_is_refused() {
        let pool = open_memory().await.unwrap();
        let (item_id, provider_id) = fixture(&pool).await;
        assert!(
            reserve(&pool, &new_grab(item_id, provider_id, "abc"))
                .await
                .unwrap()
                .is_some()
        );
        assert!(
            reserve(&pool, &new_grab(item_id, provider_id, "abc"))
                .await
                .unwrap()
                .is_none(),
            "the barrier must refuse the duplicate"
        );
        assert_eq!(for_item(&pool, item_id).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn concurrent_reservations_elect_exactly_one_winner() {
        let pool = open_memory().await.unwrap();
        let (item_id, provider_id) = fixture(&pool).await;

        // The shape a Sonarr `EpisodeAdded` webhook produced: a burst of
        // tasks starting together and converging on the same release.
        let mut handles = Vec::new();
        for _ in 0..8 {
            let pool = pool.clone();
            handles.push(tokio::spawn(async move {
                reserve(&pool, &new_grab(item_id, provider_id, "same-release"))
                    .await
                    .map(|opt| opt.is_some())
            }));
        }
        let mut winners = 0;
        for handle in handles {
            if handle.await.unwrap().unwrap() {
                winners += 1;
            }
        }
        assert_eq!(winners, 1, "exactly one task may grab a release");
        assert_eq!(for_item(&pool, item_id).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn a_non_numeric_remote_id_still_keys_the_barrier() {
        let pool = open_memory().await.unwrap();
        let (item_id, provider_id) = fixture(&pool).await;
        // Newznab returns guids like this. The old path ran
        // `parse::<u64>().unwrap_or(0)` over it, collapsing every such
        // release onto key 0.
        let guid = "d41d8cd98f00b204e9800998ecf8427e";
        assert!(
            reserve(&pool, &new_grab(item_id, provider_id, guid))
                .await
                .unwrap()
                .is_some()
        );
        assert!(
            reserve(&pool, &new_grab(item_id, provider_id, guid))
                .await
                .unwrap()
                .is_none()
        );
        // A *different* guid is a different release and must pass.
        assert!(
            reserve(
                &pool,
                &new_grab(item_id, provider_id, "0cc175b9c0f1b6a831c399e269772661")
            )
            .await
            .unwrap()
            .is_some()
        );
    }

    #[tokio::test]
    async fn per_episode_grabs_of_the_same_release_are_independent() {
        let pool = open_memory().await.unwrap();
        let (_, provider_id) = fixture(&pool).await;
        let series = library::upsert(
            &pool,
            &NewLibraryItem {
                media_type: Some(MediaType::Tv),
                tmdb_id: 76479,
                title: "The Boys".to_owned(),
                ..NewLibraryItem::default()
            },
        )
        .await
        .unwrap();
        library::sync_seasons(
            &pool,
            series.id,
            &[NewSeason {
                season_number: 4,
                episode_count: 2,
                air_date: None,
                episodes: vec![
                    NewEpisode {
                        episode_number: 1,
                        title: None,
                        air_date: None,
                    },
                    NewEpisode {
                        episode_number: 2,
                        title: None,
                        air_date: None,
                    },
                ],
            }],
        )
        .await
        .unwrap();
        let eps = library::episodes(&pool, series.id).await.unwrap();

        let mut first = new_grab(series.id, provider_id, "pack");
        first.episode_id = Some(eps[0].id);
        let mut second = new_grab(series.id, provider_id, "pack");
        second.episode_id = Some(eps[1].id);

        assert!(reserve(&pool, &first).await.unwrap().is_some());
        assert!(
            reserve(&pool, &second).await.unwrap().is_some(),
            "a different episode is a different acquisition"
        );
        // …but repeating one of them is still refused.
        assert!(reserve(&pool, &first).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn releasing_a_reservation_lets_a_retry_through() {
        let pool = open_memory().await.unwrap();
        let (item_id, provider_id) = fixture(&pool).await;
        let grab = reserve(&pool, &new_grab(item_id, provider_id, "abc"))
            .await
            .unwrap()
            .unwrap();

        // Hand-off to the download client blew up before it started.
        release_reservation(&pool, grab.id).await.unwrap();

        assert!(
            reserve(&pool, &new_grab(item_id, provider_id, "abc"))
                .await
                .unwrap()
                .is_some(),
            "a released reservation must not block the retry"
        );
    }

    #[tokio::test]
    async fn releasing_only_touches_reservations() {
        let pool = open_memory().await.unwrap();
        let (item_id, provider_id) = fixture(&pool).await;
        let grab = reserve(&pool, &new_grab(item_id, provider_id, "abc"))
            .await
            .unwrap()
            .unwrap();
        set_status(&pool, grab.id, GrabStatus::Downloading, None)
            .await
            .unwrap();

        release_reservation(&pool, grab.id).await.unwrap();

        assert_eq!(
            for_item(&pool, item_id).await.unwrap().len(),
            1,
            "an in-flight download must survive a stray release call"
        );
    }

    #[tokio::test]
    async fn status_transitions_and_queue_membership() {
        let pool = open_memory().await.unwrap();
        let (item_id, provider_id) = fixture(&pool).await;
        let grab = reserve(&pool, &new_grab(item_id, provider_id, "abc"))
            .await
            .unwrap()
            .unwrap();

        assert_eq!(active_for_item(&pool, item_id).await.unwrap().len(), 1);
        set_status(&pool, grab.id, GrabStatus::Downloading, None)
            .await
            .unwrap();
        assert_eq!(queue(&pool).await.unwrap().len(), 1);

        set_status(&pool, grab.id, GrabStatus::Imported, None)
            .await
            .unwrap();
        assert!(active_for_item(&pool, item_id).await.unwrap().is_empty());
        assert!(queue(&pool).await.unwrap().is_empty());
        assert_eq!(recent(&pool, 10).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn error_is_kept_only_for_failures() {
        let pool = open_memory().await.unwrap();
        let (item_id, provider_id) = fixture(&pool).await;
        let grab = reserve(&pool, &new_grab(item_id, provider_id, "abc"))
            .await
            .unwrap()
            .unwrap();

        set_status(
            &pool,
            grab.id,
            GrabStatus::Failed,
            Some("qbittorrent recusou"),
        )
        .await
        .unwrap();
        assert_eq!(
            get_by_id(&pool, grab.id).await.unwrap().error.as_deref(),
            Some("qbittorrent recusou")
        );

        set_status(&pool, grab.id, GrabStatus::Downloading, Some("ignorado"))
            .await
            .unwrap();
        assert!(
            get_by_id(&pool, grab.id).await.unwrap().error.is_none(),
            "a stale message must not follow the grab into a retry"
        );
    }

    #[tokio::test]
    async fn only_the_terminal_failures_let_the_scanner_back_in() {
        let pool = open_memory().await.unwrap();
        let (item_id, provider_id) = fixture(&pool).await;
        let grab = reserve(&pool, &new_grab(item_id, provider_id, "abc"))
            .await
            .unwrap()
            .unwrap();

        // Everything up to and including `imported` means "taken care
        // of" — that is brarr's whole answer to "do I have this?".
        for status in [
            GrabStatus::Reserved,
            GrabStatus::Sent,
            GrabStatus::Downloading,
            GrabStatus::Completed,
            GrabStatus::Imported,
        ] {
            set_status(&pool, grab.id, status, None).await.unwrap();
            assert!(
                !blocking_for(&pool, item_id, GrabTarget::item())
                    .await
                    .unwrap()
                    .is_empty(),
                "{} must keep the item out of the sweep",
                status.label()
            );
        }
        for status in [GrabStatus::Failed, GrabStatus::Rejected] {
            set_status(&pool, grab.id, status, None).await.unwrap();
            assert!(
                blocking_for(&pool, item_id, GrabTarget::item())
                    .await
                    .unwrap()
                    .is_empty(),
                "{} is exactly when trying again is right",
                status.label()
            );
        }
    }

    #[tokio::test]
    async fn an_item_level_grab_covers_an_episode_query_too() {
        let pool = open_memory().await.unwrap();
        let (_, provider_id) = fixture(&pool).await;
        let series = library::upsert(
            &pool,
            &NewLibraryItem {
                media_type: Some(MediaType::Tv),
                tmdb_id: 76479,
                title: "The Boys".to_owned(),
                ..NewLibraryItem::default()
            },
        )
        .await
        .unwrap();
        library::sync_seasons(
            &pool,
            series.id,
            &[NewSeason {
                season_number: 4,
                episode_count: 2,
                air_date: None,
                episodes: vec![
                    NewEpisode {
                        episode_number: 1,
                        title: None,
                        air_date: None,
                    },
                    NewEpisode {
                        episode_number: 2,
                        title: None,
                        air_date: None,
                    },
                ],
            }],
        )
        .await
        .unwrap();
        let eps = library::episodes(&pool, series.id).await.unwrap();

        // A grab with no episode named — a season pack, or a whole-series
        // acquisition — covers every episode under it.
        reserve(&pool, &new_grab(series.id, provider_id, "pack"))
            .await
            .unwrap()
            .unwrap();
        assert!(
            !blocking_for(&pool, series.id, GrabTarget::episode(eps[0].id, 4))
                .await
                .unwrap()
                .is_empty()
        );

        // …while an episode-level grab is specific to its episode.
        let pool2 = open_memory().await.unwrap();
        let (_, provider2) = fixture(&pool2).await;
        let series2 = library::upsert(
            &pool2,
            &NewLibraryItem {
                media_type: Some(MediaType::Tv),
                tmdb_id: 76479,
                title: "The Boys".to_owned(),
                ..NewLibraryItem::default()
            },
        )
        .await
        .unwrap();
        library::sync_seasons(
            &pool2,
            series2.id,
            &[NewSeason {
                season_number: 4,
                episode_count: 2,
                air_date: None,
                episodes: vec![
                    NewEpisode {
                        episode_number: 1,
                        title: None,
                        air_date: None,
                    },
                    NewEpisode {
                        episode_number: 2,
                        title: None,
                        air_date: None,
                    },
                ],
            }],
        )
        .await
        .unwrap();
        let eps2 = library::episodes(&pool2, series2.id).await.unwrap();
        let mut only_first = new_grab(series2.id, provider2, "ep1");
        only_first.episode_id = Some(eps2[0].id);
        reserve(&pool2, &only_first).await.unwrap().unwrap();
        assert!(
            !blocking_for(&pool2, series2.id, GrabTarget::episode(eps2[0].id, 4))
                .await
                .unwrap()
                .is_empty()
        );
        assert!(
            blocking_for(&pool2, series2.id, GrabTarget::episode(eps2[1].id, 4))
                .await
                .unwrap()
                .is_empty(),
            "the next episode is still wanted"
        );
    }

    #[tokio::test]
    async fn a_missing_file_stops_covering_its_item_and_frees_the_key() {
        let pool = open_memory().await.unwrap();
        let (item_id, provider_id) = fixture(&pool).await;
        let grab = reserve(&pool, &new_grab(item_id, provider_id, "abc"))
            .await
            .unwrap()
            .unwrap();
        mark_imported(
            &pool,
            grab.id,
            "/data/filmes/Matrix (1999)/Matrix (1999).mkv",
        )
        .await
        .unwrap();
        assert!(
            !blocking_for(&pool, item_id, GrabTarget::item())
                .await
                .unwrap()
                .is_empty(),
            "an imported grab covers its item"
        );

        mark_file_missing(&pool, grab.id).await.unwrap();

        assert!(
            blocking_for(&pool, item_id, GrabTarget::item())
                .await
                .unwrap()
                .is_empty(),
            "…until the file is gone, and then the item is wanted again"
        );
        // And the barrier lets the *same* release through, which is
        // usually the one the operator wants back.
        assert!(
            reserve(&pool, &new_grab(item_id, provider_id, "abc"))
                .await
                .unwrap()
                .is_some(),
            "the partial index has to skip the missing row"
        );

        // The history survives: both rows are still there.
        assert_eq!(for_item(&pool, item_id).await.unwrap().len(), 2);
        let old = get_by_id(&pool, grab.id).await.unwrap();
        assert_eq!(old.status, GrabStatus::Imported, "it *was* imported");
        assert!(old.file_missing_at.is_some());
        assert!(old.imported_path.is_some());
    }

    #[tokio::test]
    async fn the_verification_set_is_only_imported_rows_with_a_path() {
        let pool = open_memory().await.unwrap();
        let (item_id, provider_id) = fixture(&pool).await;

        let imported = reserve(&pool, &new_grab(item_id, provider_id, "a"))
            .await
            .unwrap()
            .unwrap();
        mark_imported(&pool, imported.id, "/data/a.mkv")
            .await
            .unwrap();

        // Downloaded but not imported: those files belong to the
        // download client, not to brarr.
        let completed = reserve(&pool, &new_grab(item_id, provider_id, "b"))
            .await
            .unwrap()
            .unwrap();
        set_status(&pool, completed.id, GrabStatus::Completed, None)
            .await
            .unwrap();

        let set = imported_present(&pool).await.unwrap();
        assert_eq!(set.len(), 1);
        assert_eq!(set[0].id, imported.id);

        // Already known missing ⇒ not checked again.
        mark_file_missing(&pool, imported.id).await.unwrap();
        assert!(imported_present(&pool).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_season_pack_covers_its_own_season_and_no_other() {
        let pool = open_memory().await.unwrap();
        let (_, provider_id) = fixture(&pool).await;
        let series = library::upsert(
            &pool,
            &NewLibraryItem {
                media_type: Some(MediaType::Tv),
                tmdb_id: 76479,
                title: "The Boys".to_owned(),
                ..NewLibraryItem::default()
            },
        )
        .await
        .unwrap();
        library::sync_seasons(
            &pool,
            series.id,
            &[
                NewSeason {
                    season_number: 4,
                    episode_count: 1,
                    air_date: None,
                    episodes: vec![NewEpisode {
                        episode_number: 1,
                        title: None,
                        air_date: None,
                    }],
                },
                NewSeason {
                    season_number: 5,
                    episode_count: 1,
                    air_date: None,
                    episodes: vec![NewEpisode {
                        episode_number: 1,
                        title: None,
                        air_date: None,
                    }],
                },
            ],
        )
        .await
        .unwrap();
        let eps = library::episodes(&pool, series.id).await.unwrap();
        let s4 = eps.iter().find(|e| e.season_number == 4).unwrap();
        let s5 = eps.iter().find(|e| e.season_number == 5).unwrap();

        // A pack: no episode named, but a season is.
        let mut pack = new_grab(series.id, provider_id, "s04-pack");
        pack.season_number = Some(4);
        reserve(&pool, &pack).await.unwrap().unwrap();

        assert!(
            !blocking_for(&pool, series.id, GrabTarget::episode(s4.id, 4))
                .await
                .unwrap()
                .is_empty(),
            "the pack covers its own season"
        );
        assert!(
            blocking_for(&pool, series.id, GrabTarget::episode(s5.id, 5))
                .await
                .unwrap()
                .is_empty(),
            "…and must not make the next season look acquired"
        );
    }

    #[tokio::test]
    async fn an_adopted_file_has_no_transport() {
        let pool = open_memory().await.unwrap();
        let (item_id, provider_id) = fixture(&pool).await;
        let mut local = new_grab(
            item_id,
            provider_id,
            "/mnt/midias/Filmes/Matrix (1999)/Matrix.mkv",
        );
        local.protocol = Protocol::Local;
        local.download_url = None;
        let grab = reserve(&pool, &local).await.unwrap().unwrap();
        assert_eq!(
            get_by_id(&pool, grab.id).await.unwrap().protocol,
            Protocol::Local,
            "the CHECK has to accept a file that was never downloaded"
        );
    }

    #[tokio::test]
    async fn mark_sent_records_who_took_it() {
        let pool = open_memory().await.unwrap();
        let (item_id, provider_id) = fixture(&pool).await;
        let grab = reserve(&pool, &new_grab(item_id, provider_id, "abc"))
            .await
            .unwrap()
            .unwrap();
        assert!(grab.client_id.is_none());

        let client = crate::db::download_clients::insert(
            &pool,
            crate::db::download_clients::NewDownloadClient {
                name: "qb",
                kind: brarr_download_client::DownloadClientKind::Qbittorrent,
                base_url: &url::Url::parse("http://10.0.1.246:8080/").unwrap(),
                username: None,
                password: None,
                api_key: None,
                category: None,
                priority: None,
                enabled: None,
            },
        )
        .await
        .unwrap();

        mark_sent(&pool, grab.id, client.id, Some("SABnzbd_nzo_x"))
            .await
            .unwrap();
        let after = get_by_id(&pool, grab.id).await.unwrap();
        assert_eq!(after.status, GrabStatus::Sent);
        assert_eq!(after.client_id, Some(client.id));
        assert_eq!(after.client_item_id.as_deref(), Some("SABnzbd_nzo_x"));
    }

    #[tokio::test]
    async fn deleting_the_item_cascades_its_grabs() {
        let pool = open_memory().await.unwrap();
        let (item_id, provider_id) = fixture(&pool).await;
        reserve(&pool, &new_grab(item_id, provider_id, "abc"))
            .await
            .unwrap();

        library::delete(&pool, item_id).await.unwrap();

        assert!(recent(&pool, 10).await.unwrap().is_empty());
    }
}
