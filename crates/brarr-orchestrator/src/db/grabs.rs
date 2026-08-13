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

/// What a grab is *about*.
///
/// Recorded rather than inferred from `(episode_id, season_number)`,
/// because the absence of a value cannot distinguish "this grab is about
/// the whole item" from "this grab lost what it was about". A grab taken
/// for an episode stays [`Self::Episode`] forever: if its FK is nulled
/// again it covers **nothing**, which is visible and repairable, instead
/// of covering everything, which is not. See the migration.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum GrabScope {
    /// The item as a whole — every film, and nothing else today.
    #[default]
    Item,
    /// One season, as a pack.
    Season,
    /// One episode.
    Episode,
}

impl GrabScope {
    /// The scope a reservation with these coordinates is taking.
    ///
    /// Derived at insert and never passed in by a caller: it is exactly
    /// the encoding the two columns already carried, and letting six
    /// call sites set it by hand would be a sixth way to get it wrong.
    #[must_use]
    pub fn of(episode_id: Option<Uuid>, season_number: Option<i32>) -> Self {
        match (episode_id, season_number) {
            (Some(_), _) => Self::Episode,
            (None, Some(_)) => Self::Season,
            (None, None) => Self::Item,
        }
    }

    /// Persisted label.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Item => "item",
            Self::Season => "season",
            Self::Episode => "episode",
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
            "item" => Ok(Self::Item),
            "season" => Ok(Self::Season),
            "episode" => Ok(Self::Episode),
            other => Err(AppError::InvalidInput(format!(
                "unknown grabs.scope: {other}"
            ))),
        }
    }
}

/// One acquisition attempt.
#[derive(Debug, Clone)]
pub struct Grab {
    /// Stable UUID v4.
    pub id: Uuid,
    /// Catalogue entry being acquired.
    pub item_id: Uuid,
    /// What this grab is about. Survives its `episode_id` being nulled,
    /// which is the entire reason it is stored.
    pub scope: GrabScope,
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
    /// Why the last import attempt could not proceed. `None` when it
    /// could. Deliberately not [`Self::error`]: that column means "this
    /// grab failed" everywhere else in the code, and the whole point of
    /// this one is that waiting is not failing.
    pub import_wait_reason: Option<String>,
    /// When the importer last looked at this grab, whatever it
    /// concluded. Distinct from [`Self::updated_at`], which means "the
    /// state changed" — waiting changes no state, so without a separate
    /// clock a stuck grab keeps its place at the head of the import
    /// queue forever and starves everything behind it.
    pub import_attempted_at: Option<OffsetDateTime>,
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

const GRAB_COLUMNS: &str = "id, item_id, scope, episode_id, season_number, decision_id, provider_id, \
     provider_name, release_id_remote, release_name, download_url, protocol, \
     client_id, client_item_id, status, error, imported_path, file_missing_at, \
     import_wait_reason, import_attempted_at, grabbed_at, updated_at";

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
    let scope_raw: String = row.try_get("scope")?;
    let grabbed: i64 = row.try_get("grabbed_at")?;
    let updated: i64 = row.try_get("updated_at")?;
    Ok(Grab {
        id,
        item_id,
        scope: GrabScope::from_label(&scope_raw)?,
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
        import_wait_reason: row.try_get("import_wait_reason")?,
        import_attempted_at: row
            .try_get::<Option<i64>, _>("import_attempted_at")?
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
            id, item_id, scope, episode_id, season_number, decision_id, provider_id, \
            provider_name, release_id_remote, release_name, download_url, \
            protocol, status, grabbed_at, updated_at \
         ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, 'reserved', ?, ?) \
         ON CONFLICT DO NOTHING",
    )
    .bind(id.to_string())
    .bind(new.item_id.to_string())
    .bind(GrabScope::of(new.episode_id, new.season_number).label())
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

/// Written to `grabs.provider_name`, which is NOT NULL and is what the
/// detail screen's "Provider" column renders.
pub const LOCAL_PROVIDER_NAME: &str = "disco local";

/// Everything needed to reserve a file that was already on disk.
///
/// A separate type from [`NewGrab`] on purpose. Loosening
/// [`NewGrab::provider_id`] to `Option<Uuid>` would silently remove the
/// barrier from every *other* caller that got it wrong, and two of them
/// write releases today. Adoption gets its own type, its own index, and
/// no way to name a provider.
#[derive(Debug, Clone)]
pub struct LocalGrab<'a> {
    /// Catalogue entry the file belongs to.
    pub item_id: Uuid,
    /// Episode, when the file is one. Deliberately outside the barrier
    /// key — see `idx_grabs_unique_local`.
    pub episode_id: Option<Uuid>,
    /// Absolute path as found on disk. It is the second half of the key,
    /// stored in `release_id_remote`.
    pub source_path: &'a str,
    /// File name, for the grab history.
    pub release_name: &'a str,
}

/// Take the reservation for a file that was already on disk.
///
/// Sibling of [`reserve`], separate from it because an adopted file has
/// no provider and [`NewGrab::provider_id`] cannot express that. Same
/// contract: `Ok(Some(grab))` means this caller won and may touch the
/// filesystem; `Ok(None)` means this item already has a live grab for
/// this path and the caller must write **nothing** — not retry, not
/// link, not record.
///
/// Reserving before placing, rather than inserting straight as
/// `imported`, is what stops a failed hardlink from leaving a
/// half-adopted state: the caller calls [`release_reservation`] and the
/// key goes free again. Delivery never sees the row — it goes from
/// `reserved` to `imported` without passing through `sent`.
///
/// `season_number` stays NULL even for an episode, mirroring what the
/// sweep writes: a non-null `season_number` on a row with
/// `episode_id IS NULL` is what [`blocking_for`] reads as "this pack
/// covers the whole season", and adoption never produces that.
///
/// # Errors
///
/// Returns [`AppError::Database`] on SQL failure.
pub async fn reserve_local(pool: &Pool, new: &LocalGrab<'_>) -> Result<Option<Grab>, AppError> {
    let id = Uuid::new_v4();
    let now = OffsetDateTime::now_utc().unix_timestamp();
    // Bare `ON CONFLICT DO NOTHING` for the same reason as `reserve`:
    // `INSERT OR IGNORE` would also swallow CHECK and FK violations.
    let res = sqlx::query(
        "INSERT INTO grabs ( \
            id, item_id, scope, episode_id, season_number, decision_id, provider_id, \
            provider_name, release_id_remote, release_name, download_url, \
            protocol, status, grabbed_at, updated_at \
         ) VALUES (?, ?, ?, ?, NULL, NULL, NULL, ?, ?, ?, NULL, 'local', 'reserved', ?, ?) \
         ON CONFLICT DO NOTHING",
    )
    .bind(id.to_string())
    .bind(new.item_id.to_string())
    .bind(GrabScope::of(new.episode_id, None).label())
    .bind(new.episode_id.map(|e| e.to_string()))
    .bind(LOCAL_PROVIDER_NAME)
    .bind(new.source_path)
    .bind(new.release_name)
    .bind(now)
    .bind(now)
    .execute(pool)
    .await?;

    if res.rows_affected() == 0 {
        return Ok(None);
    }
    get_by_id(pool, id).await.map(Some)
}

/// `true` when brarr adopted this file where it stood and wrote nothing.
///
/// In an in-place adoption the source path *is* the imported path; in a
/// link they differ by construction. That invariant is what lets undo
/// know, without an extra column, whether there is a file to remove.
#[must_use]
pub fn is_in_place(grab: &Grab) -> bool {
    grab.protocol == Protocol::Local
        && grab.imported_path.as_deref() == Some(grab.release_id_remote.as_str())
}

/// Delete an adoption. **Only** `protocol = 'local'` rows — a tracker
/// grab is acquisition history and is never deleted from the UI.
///
/// Returns the row so the caller can undo the link it created. Deleting
/// rather than marking: an adoption created no acquisition history worth
/// preserving, and deleting is what frees the key for the operator to
/// adopt again after fixing whatever was wrong. [`mark_file_missing`]
/// would be wrong here — it says "the file vanished", and it did not.
///
/// # Errors
///
/// [`AppError::NotFound`] when absent or not local, [`AppError::Database`]
/// on SQL failure.
pub async fn delete_adopted(pool: &Pool, id: Uuid) -> Result<Grab, AppError> {
    let grab = get_by_id(pool, id).await?;
    if grab.protocol != Protocol::Local {
        return Err(AppError::NotFound(format!("adoption {id}")));
    }
    sqlx::query("DELETE FROM grabs WHERE id = ? AND protocol = 'local'")
        .bind(id.to_string())
        .execute(pool)
        .await?;
    Ok(grab)
}

/// Delete local reservations for an item older than `cutoff`.
///
/// A crash between the reservation and the import leaves a `reserved`
/// row holding the key with nothing behind it. Nothing else in brarr
/// clears it: `queue::snapshot` answers `Probe::Unreachable` for a grab
/// with no client and `next_status` never acts on that, deliberately.
/// Adoption is interactive and ends within one request, so anything
/// older than an hour is debris.
///
/// # Errors
///
/// Returns [`AppError::Database`] on SQL failure.
pub async fn clear_stale_local_reservations(
    pool: &Pool,
    item_id: Uuid,
    cutoff: OffsetDateTime,
) -> Result<u64, AppError> {
    let res = sqlx::query(
        "DELETE FROM grabs WHERE protocol = 'local' AND status = 'reserved' \
           AND item_id = ? AND updated_at < ?",
    )
    .bind(item_id.to_string())
    .bind(cutoff.unix_timestamp())
    .execute(pool)
    .await?;
    Ok(res.rows_affected())
}

/// Every imported grab of an item, marked missing or not.
///
/// Unlike [`imported_present`] this does **not** filter on
/// `file_missing_at`: the per-item check runs in both directions, and an
/// already-marked row is exactly the one that has to be looked at again
/// to notice the operator put the file back.
///
/// # Errors
///
/// Returns [`AppError::Database`] on SQL failure.
pub async fn imported_for_item(pool: &Pool, item_id: Uuid) -> Result<Vec<Grab>, AppError> {
    let rows = sqlx::query(&format!(
        "SELECT {GRAB_COLUMNS} FROM grabs \
         WHERE item_id = ? AND status = 'imported' AND imported_path IS NOT NULL \
         ORDER BY grabbed_at DESC"
    ))
    .bind(item_id.to_string())
    .fetch_all(pool)
    .await?;
    rows.iter().map(row_to_grab).collect()
}

/// Every grab of an item that still counts as coverage.
///
/// The set [`blocking_for`] would return for *some* target, read once,
/// so a preview of 120 episodes asks one question instead of 120. The
/// per-target predicate runs in Rust as [`covers`].
///
/// # Errors
///
/// Returns [`AppError::Database`] on SQL failure.
pub async fn live_for_item(pool: &Pool, item_id: Uuid) -> Result<Vec<Grab>, AppError> {
    let rows = sqlx::query(&format!(
        "SELECT {GRAB_COLUMNS} FROM grabs \
         WHERE item_id = ? AND status NOT IN ('failed', 'rejected') \
           AND file_missing_at IS NULL \
         ORDER BY grabbed_at DESC"
    ))
    .bind(item_id.to_string())
    .fetch_all(pool)
    .await?;
    rows.iter().map(row_to_grab).collect()
}

/// A file that came back. Clears [`Grab::file_missing_at`], so the row
/// covers its item again and resumes its barrier key.
///
/// `UPDATE OR IGNORE` because resuming the key can legitimately fail:
/// while the file was gone the sweep may have acquired a replacement
/// that now holds it. Losing that race leaves this row marked missing,
/// which is the right answer — the operator has two copies and the newer
/// grab owns the slot.
///
/// `OR IGNORE` is otherwise forbidden in this module, which chooses
/// `ON CONFLICT DO NOTHING` precisely because it does not swallow CHECK
/// and FK violations. It is safe here and the reason is written down:
/// the statement touches only `file_missing_at` and `updated_at`,
/// neither of which takes part in a CHECK or a foreign key, so the only
/// violation possible is uniqueness — which is exactly the one to
/// swallow.
///
/// # Errors
///
/// Returns [`AppError::Database`] on SQL failure.
pub async fn clear_file_missing(pool: &Pool, id: Uuid) -> Result<bool, AppError> {
    let now = OffsetDateTime::now_utc().unix_timestamp();
    let res = sqlx::query(
        "UPDATE OR IGNORE grabs SET file_missing_at = NULL, updated_at = ? \
         WHERE id = ? AND file_missing_at IS NOT NULL",
    )
    .bind(now)
    .bind(id.to_string())
    .execute(pool)
    .await?;
    Ok(res.rows_affected() > 0)
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
    let res = sqlx::query(
        "UPDATE grabs SET status = ?, error = ?, import_wait_reason = NULL, \
         updated_at = ? WHERE id = ?",
    )
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
/// | `scope = episode`, `episode_id = X` | episode X only |
/// | `scope = item` | the whole item — a film, or a full-series grab |
/// | `scope = season`, `season_number = 4` | every episode of season 4 |
/// | `scope = episode`, `episode_id` NULL | **nothing** |
///
/// The season row is why the season has to travel with the question. A
/// pack of season 4 used to satisfy the query for *any* episode, so
/// season 5 read as acquired the moment one pack landed.
///
/// The last row is why `scope` is a column. It used to be indexed the
/// same way as `scope = item`, because both are "no episode, no season"
/// — so a per-episode grab whose FK a metadata refresh nulled started
/// answering for the entire series, and the library rendered complete.
/// The scope survives the FK, so a grab that lost its episode now covers
/// nothing until [`crate::relink`] puts it back.
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
                 scope = 'item' \
              OR (scope = 'season'  AND (? IS NULL OR season_number IS ?)) \
              OR (scope = 'episode' AND episode_id IS ? AND episode_id IS NOT NULL) \
           ) \
         ORDER BY grabbed_at DESC"
    ))
    .bind(item_id.to_string())
    .bind(target.episode_id.map(|e| e.to_string()))
    .bind(target.season_number.map(i64::from))
    .bind(target.episode_id.map(|e| e.to_string()))
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

/// The predicate [`blocking_for`] applies in SQL, in Rust.
///
/// One arm per scope, matching the three-branch `OR` in the query. It
/// used to be a null-safe comparison of the two coordinate columns,
/// which was a faithful translation of the SQL *and* of the encoding's
/// flaw: `(NULL, NULL)` meant both "about the whole item" and "lost what
/// it was about", so a decayed row covered everything.
///
/// A test confronts the two over a matrix of fixtures, the same way
/// `ProviderScope` and `Protocol::matches_kind` are kept in agreement.
#[must_use]
pub fn covers(grab: &Grab, target: GrabTarget) -> bool {
    if !grab.status.blocks_search() || grab.file_missing_at.is_some() {
        return false;
    }
    covers_target(grab.scope, grab.episode_id, grab.season_number, target)
}

/// The coordinate half of [`covers`], without the status checks.
///
/// Split out so [`Coverage`] — which is already filtered to live rows in
/// SQL — can ask the same question without carrying a whole [`Grab`].
/// There is exactly one place this rule is written in Rust, and the test
/// that confronts it with the SQL guards that one.
#[must_use]
pub fn covers_target(
    scope: GrabScope,
    episode_id: Option<Uuid>,
    season_number: Option<i32>,
    target: GrabTarget,
) -> bool {
    match scope {
        // A whole-item acquisition answers every question about the
        // item, including "do I have episode 7".
        GrabScope::Item => true,
        // A pack answers for its own season, and for the item-wide
        // "is anything covering this at all".
        GrabScope::Season => target.episode_id.is_none() || season_number == target.season_number,
        // An episode answers only for itself — and a row that lost its
        // episode answers for nothing, which is the point of the column.
        GrabScope::Episode => target.episode_id.is_some() && episode_id == target.episode_id,
    }
}

/// The coordinates of one grab that still counts as coverage.
///
/// A [`Grab`] is ~20 columns and the library index needs three of them
/// across every row in the table. Loading the full struct for a
/// collection with thousands of adopted files, once per page render,
/// is not a cost the answer justifies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Coverage {
    /// Catalogue entry the grab belongs to.
    pub item_id: Uuid,
    /// What the grab is about.
    pub scope: GrabScope,
    /// Episode it names, when it names one.
    pub episode_id: Option<Uuid>,
    /// Season it names, for a pack.
    pub season_number: Option<i32>,
}

impl Coverage {
    /// Whether this grab answers for `target`.
    #[must_use]
    pub fn covers(self, target: GrabTarget) -> bool {
        covers_target(self.scope, self.episode_id, self.season_number, target)
    }
}

const COVERAGE_WHERE: &str = "status NOT IN ('failed', 'rejected') AND file_missing_at IS NULL";

fn row_to_coverage(row: &SqliteRow) -> Result<Coverage, AppError> {
    let item: String = row.try_get("item_id")?;
    let scope_raw: String = row.try_get("scope")?;
    let episode: Option<String> = row.try_get("episode_id")?;
    let season: Option<i64> = row.try_get("season_number")?;
    Ok(Coverage {
        item_id: Uuid::parse_str(&item)
            .map_err(|e| AppError::InvalidInput(format!("invalid grabs.item_id: {e}")))?,
        scope: GrabScope::from_label(&scope_raw)?,
        episode_id: episode
            .as_deref()
            .map(Uuid::parse_str)
            .transpose()
            .map_err(|e| AppError::InvalidInput(format!("invalid grabs.episode_id: {e}")))?,
        season_number: season.and_then(|v| i32::try_from(v).ok()),
    })
}

/// Every live grab's coordinates, for the whole library, in one query.
///
/// The status and `file_missing_at` filters live in the SQL because they
/// are what "live" means; the per-target rule stays in Rust as
/// [`covers_target`], so it is not written a third time.
///
/// # Errors
///
/// Returns [`AppError::Database`] on SQL failure.
pub async fn live_coverage(pool: &Pool) -> Result<Vec<Coverage>, AppError> {
    let rows = sqlx::query(&format!(
        "SELECT item_id, scope, episode_id, season_number FROM grabs WHERE {COVERAGE_WHERE}"
    ))
    .fetch_all(pool)
    .await?;
    rows.iter().map(row_to_coverage).collect()
}

/// The same, narrowed to one item.
///
/// # Errors
///
/// Returns [`AppError::Database`] on SQL failure.
pub async fn live_coverage_for_item(pool: &Pool, item_id: Uuid) -> Result<Vec<Coverage>, AppError> {
    let rows = sqlx::query(&format!(
        "SELECT item_id, scope, episode_id, season_number FROM grabs \
         WHERE item_id = ? AND {COVERAGE_WHERE}"
    ))
    .bind(item_id.to_string())
    .fetch_all(pool)
    .await?;
    rows.iter().map(row_to_coverage).collect()
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
/// to move them into the library.
///
/// **Least recently attempted first, then oldest.** Ordering by
/// `updated_at` alone starves the queue: a grab that waits does not
/// change state, so `updated_at` does not move, so the same few rows sit
/// at the head of a `MAX_IMPORTS_PER_PASS`-sized window forever and
/// everything behind them is never tried. `import_attempted_at` is the
/// importer's own clock — when it *looked*, not when the grab *changed*.
/// NULL sorts first in ASC on SQLite, so a freshly completed grab still
/// cuts ahead of the stuck set.
///
/// # Errors
///
/// Returns [`AppError::Database`] on SQL failure.
pub async fn awaiting_import(pool: &Pool) -> Result<Vec<Grab>, AppError> {
    let rows = sqlx::query(&format!(
        "SELECT {GRAB_COLUMNS} FROM grabs WHERE status = 'completed' \
         ORDER BY import_attempted_at ASC, updated_at ASC"
    ))
    .fetch_all(pool)
    .await?;
    rows.iter().map(row_to_grab).collect()
}

/// Record that the importer looked at this grab.
///
/// Deliberately does not touch `updated_at`: nothing about the grab
/// changed state, and `updated_at` is what [`crate::queue`]'s
/// `MISSING_GRACE` measures from — moving it here would keep resetting
/// that grace period. Deliberately does not check `rows_affected`: a
/// grab deleted mid-pass is not worth aborting the pass over.
///
/// # Errors
///
/// Returns [`AppError::Database`] on SQL failure.
pub async fn mark_import_attempted(pool: &Pool, id: Uuid) -> Result<(), AppError> {
    sqlx::query("UPDATE grabs SET import_attempted_at = ? WHERE id = ?")
        .bind(OffsetDateTime::now_utc().unix_timestamp())
        .bind(id.to_string())
        .execute(pool)
        .await?;
    Ok(())
}

/// Record why the last import attempt could not proceed, or clear it.
/// Also leaves `updated_at` alone, for the same reason.
///
/// # Errors
///
/// Returns [`AppError::Database`] on SQL failure.
pub async fn set_import_wait_reason(
    pool: &Pool,
    id: Uuid,
    reason: Option<&str>,
) -> Result<(), AppError> {
    sqlx::query("UPDATE grabs SET import_wait_reason = ? WHERE id = ?")
        .bind(reason)
        .bind(id.to_string())
        .execute(pool)
        .await?;
    Ok(())
}

/// Put a failed grab back in the import queue. `true` when a row moved.
///
/// One statement rather than read-then-write: the `WHERE` clause **is**
/// the lock, so a double click is a no-op and no URL can drag a
/// `reserved` or `imported` grab into `completed`. The two client
/// conditions are there because a grab that failed during *delivery*
/// never reached a client at all — requeuing it would only make it wait
/// forever on "o grab não registrou cliente ou identificador".
/// Re-delivering is a different feature.
///
/// Nothing is downloaded again, and the barrier key does not change
/// hands: it is the same row moving between two states that both occupy
/// it. `import_attempted_at = NULL` sends it to the front of the queue —
/// the operator asked for this one, now.
///
/// # Errors
///
/// Returns [`AppError::Database`] on SQL failure.
pub async fn requeue_import(pool: &Pool, id: Uuid) -> Result<bool, AppError> {
    let res = sqlx::query(
        "UPDATE grabs \
            SET status = 'completed', error = NULL, import_wait_reason = NULL, \
                import_attempted_at = NULL, updated_at = ? \
          WHERE id = ? AND status = 'failed' AND imported_path IS NULL \
            AND client_id IS NOT NULL AND client_item_id IS NOT NULL",
    )
    .bind(OffsetDateTime::now_utc().unix_timestamp())
    .bind(id.to_string())
    .execute(pool)
    .await?;
    Ok(res.rows_affected() > 0)
}

/// Failed grabs whose download a client may still hold — exactly the set
/// [`requeue_import`] can act on, and what the mappings screen lists.
///
/// **No filtering by message.** The predicate does not know *why* each
/// one failed, and pretending it does is how a "reimport everything"
/// button stays lit forever: it requeues genuinely permanent failures,
/// they fail again, and the count never drops. The UI shows each row's
/// `error` and lets the operator decide.
///
/// # Errors
///
/// Returns [`AppError::Database`] on SQL failure.
pub async fn retryable_imports(pool: &Pool) -> Result<Vec<Grab>, AppError> {
    let rows = sqlx::query(&format!(
        "SELECT {GRAB_COLUMNS} FROM grabs \
          WHERE status = 'failed' AND imported_path IS NULL \
            AND client_id IS NOT NULL AND client_item_id IS NOT NULL \
          ORDER BY updated_at DESC"
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
        "UPDATE grabs SET status = 'imported', error = NULL, import_wait_reason = NULL, \
         imported_path = ?, updated_at = ? WHERE id = ?",
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

/// What [`relink_episode`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Relink {
    /// The grab now names the episode.
    Linked,
    /// The grab already named one, or is gone. Nothing was touched.
    AlreadyLinked,
    /// A live grab of the same release already answers for that episode,
    /// so this orphan is a duplicate rather than the record to keep.
    Occupied,
}

/// Point a grab that lost its episode back at the one it holds.
///
/// `WHERE episode_id IS NULL` **is the lock**. This can only ever fill a
/// blank — never move a file from one episode to another — so running the
/// repair twice, or two of them at once, is a no-op the second time.
/// That matters because every caller is a sweep.
///
/// # Errors
///
/// - [`AppError::NotFound`] when the id is absent.
/// - [`AppError::Database`] on SQL failure.
pub async fn relink_episode(pool: &Pool, id: Uuid, episode_id: Uuid) -> Result<Relink, AppError> {
    let now = OffsetDateTime::now_utc().unix_timestamp();
    let res = sqlx::query(
        "UPDATE grabs SET episode_id = ?, updated_at = ? WHERE id = ? AND episode_id IS NULL",
    )
    .bind(episode_id.to_string())
    .bind(now)
    .bind(id.to_string())
    .execute(pool)
    .await;
    match res {
        Ok(done) if done.rows_affected() > 0 => Ok(Relink::Linked),
        // Filling the blank moves the row out of `idx_grabs_unique_item`
        // and into `idx_grabs_unique_episode`, so a live sibling of the
        // same release already sitting on that episode refuses it.
        Err(sqlx::Error::Database(e)) if e.is_unique_violation() => Ok(Relink::Occupied),
        Err(e) => Err(e.into()),
        Ok(_) => Ok(Relink::AlreadyLinked),
    }
}

/// Grabs taken for an episode that no longer name one, and hold a file.
///
/// This is the shape a metadata refresh left behind. Before `scope`
/// existed the query had to infer it — both coordinates NULL *and* the
/// item a series, since for a film that shape is the correct one — which
/// is exactly the ambiguity the column removes: a row that says
/// `scope = 'episode'` with no `episode_id` cannot be anything but a
/// decayed one.
///
/// # Errors
///
/// Returns [`AppError::Database`] on SQL failure.
pub async fn unlinked_episode_grabs(pool: &Pool) -> Result<Vec<Grab>, AppError> {
    let rows = sqlx::query(&format!(
        "SELECT {GRAB_COLUMNS} FROM grabs \
         WHERE scope = 'episode' AND episode_id IS NULL \
           AND imported_path IS NOT NULL \
           AND file_missing_at IS NULL \
           AND status NOT IN ('failed', 'rejected') \
         ORDER BY grabbed_at ASC"
    ))
    .fetch_all(pool)
    .await?;
    rows.iter().map(row_to_grab).collect()
}

/// The live grab holding `path` for this item, if there is one.
///
/// The repair path needs it: when [`reserve_local`] refuses because the
/// file is already recorded, the caller knows which episode the \*arr
/// says it is but not which row to correct.
///
/// # Errors
///
/// Returns [`AppError::Database`] on SQL failure.
pub async fn local_by_path(
    pool: &Pool,
    item_id: Uuid,
    path: &str,
) -> Result<Option<Grab>, AppError> {
    let row = sqlx::query(&format!(
        "SELECT {GRAB_COLUMNS} FROM grabs \
         WHERE item_id = ? AND release_id_remote = ? \
           AND protocol = 'local' AND file_missing_at IS NULL"
    ))
    .bind(item_id.to_string())
    .bind(path)
    .fetch_optional(pool)
    .await?;
    row.as_ref().map(row_to_grab).transpose()
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

    /// Season 4 with two episodes — the shape most of these tests need.
    fn two_episode_season(season_number: i32) -> NewSeason {
        NewSeason {
            season_number,
            episode_count: 2,
            air_date: None,
            episodes: (1..=2)
                .map(|episode_number| NewEpisode {
                    tmdb_episode_id: None,
                    episode_number,
                    title: None,
                    air_date: None,
                })
                .collect(),
        }
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
        library::sync_seasons(&pool, series.id, &[two_episode_season(4)])
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
    async fn a_per_episode_grab_survives_a_metadata_refresh() {
        // The passive *arr sweep calls `library::sync_seasons` for every
        // series on every pass (arr_import.rs, outside the `if created`
        // gate). While that function rebuilt the tree with fresh UUIDs,
        // every per-episode grab was unlinked every half hour — and the
        // damage rendered as *complete*, because an unlinked grab is
        // `(NULL, NULL)`, which `covers_target` reads as "the whole
        // item". Nothing in the UI asked for action and the scanner
        // stopped too.
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
        let tree = [NewSeason {
            season_number: 4,
            episode_count: 2,
            air_date: None,
            episodes: vec![
                NewEpisode {
                    tmdb_episode_id: None,
                    episode_number: 1,
                    title: None,
                    air_date: None,
                },
                NewEpisode {
                    tmdb_episode_id: None,
                    episode_number: 2,
                    title: None,
                    air_date: None,
                },
            ],
        }];
        library::sync_seasons(&pool, series.id, &tree)
            .await
            .unwrap();
        let eps = library::episodes(&pool, series.id).await.unwrap();

        let mut wanted = new_grab(series.id, provider_id, "s04e01");
        wanted.episode_id = Some(eps[0].id);
        reserve(&pool, &wanted).await.unwrap().unwrap();

        // A plain metadata refresh: same numbering, same shape.
        library::sync_seasons(&pool, series.id, &tree)
            .await
            .unwrap();

        let after = for_item(&pool, series.id).await.unwrap();
        assert_eq!(after.len(), 1);
        assert_eq!(
            after[0].episode_id,
            Some(eps[0].id),
            "a refresh must not unlink the file from its episode"
        );
        assert!(
            !covers(&after[0], GrabTarget::episode(eps[1].id, 4)),
            "and it must not start answering for the episode it never named"
        );
    }

    #[tokio::test]
    async fn a_grab_that_lost_its_episode_covers_nothing() {
        // The whole reason `scope` is a column. Before it, this row was
        // indistinguishable from a whole-item acquisition — both are
        // "no episode, no season" — so it answered for *every* episode
        // of the series and the library rendered complete with each row
        // pointing at an arbitrary file.
        let pool = open_memory().await.unwrap();
        let (_, provider_id) = fixture(&pool).await;
        let series = library::upsert(
            &pool,
            &NewLibraryItem {
                media_type: Some(MediaType::Tv),
                tmdb_id: 62715,
                title: "Dragon Ball Super".to_owned(),
                ..NewLibraryItem::default()
            },
        )
        .await
        .unwrap();
        library::sync_seasons(
            &pool,
            series.id,
            &[NewSeason {
                season_number: 1,
                episode_count: 2,
                air_date: None,
                episodes: vec![
                    NewEpisode {
                        tmdb_episode_id: None,
                        episode_number: 1,
                        title: None,
                        air_date: None,
                    },
                    NewEpisode {
                        tmdb_episode_id: None,
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

        let mut held = new_grab(series.id, provider_id, "s01e01");
        held.episode_id = Some(eps[0].id);
        let grab = reserve(&pool, &held).await.unwrap().unwrap();
        // Forge what the old `sync_seasons` left behind. No code path
        // produces this any more, which is why the test has to.
        sqlx::query("UPDATE grabs SET episode_id = NULL WHERE id = ?")
            .bind(grab.id.to_string())
            .execute(&pool)
            .await
            .unwrap();

        let decayed = get_by_id(&pool, grab.id).await.unwrap();
        assert_eq!(decayed.scope, GrabScope::Episode, "the scope survives");
        assert!(
            !covers(&decayed, GrabTarget::episode(eps[1].id, 1)),
            "it must not answer for the episode it never named"
        );
        assert!(
            !covers(&decayed, GrabTarget::episode(eps[0].id, 1)),
            "nor for the one it did — it no longer knows which that was"
        );
        assert!(
            !covers(&decayed, GrabTarget::item()),
            "nor for the series as a whole"
        );
        assert!(
            blocking_for(&pool, series.id, GrabTarget::episode(eps[1].id, 1))
                .await
                .unwrap()
                .is_empty(),
            "and the SQL has to agree, or the scanner and the UI disagree"
        );
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
        library::sync_seasons(&pool, series.id, &[two_episode_season(4)])
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
        library::sync_seasons(&pool2, series2.id, &[two_episode_season(4)])
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
                        tmdb_episode_id: None,
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
                        tmdb_episode_id: None,
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

    fn local(item_id: Uuid, path: &str) -> LocalGrab<'_> {
        LocalGrab {
            item_id,
            episode_id: None,
            source_path: path,
            release_name: "Matrix.1999.1080p.BluRay.PT-BR.mkv",
        }
    }

    #[tokio::test]
    async fn a_local_file_is_reserved_once_per_item_and_path() {
        let pool = open_memory().await.unwrap();
        let (item_id, _) = fixture(&pool).await;
        let first = reserve_local(&pool, &local(item_id, "/midias/Filmes/x.mkv"))
            .await
            .unwrap();
        let first = first.expect("first adoption wins");
        assert_eq!(first.protocol, Protocol::Local);
        assert_eq!(first.provider_name, LOCAL_PROVIDER_NAME);
        assert!(first.provider_id.is_none(), "adoption has no provider");
        assert!(
            reserve_local(&pool, &local(item_id, "/midias/Filmes/x.mkv"))
                .await
                .unwrap()
                .is_none(),
            "the same path must not be adopted twice for one item"
        );
    }

    /// The episode is **inside** the key, since `20260813120000`.
    ///
    /// It used to be outside, so that an operator fixing a wrong match by
    /// adopting again — instead of undoing first — could not end up with
    /// one file covering two episodes. That guard cost more than it saved:
    /// a two-episode file is a real, ordinary thing (`S05E33E34` here,
    /// `S33E06E07` on the Simpsons) and Sonarr, Plex and the operator all
    /// read it as two. Under the old key the second episode was uncovered
    /// forever *and* unfixable, because every release the scanner found
    /// for it was refused by the same path key.
    ///
    /// The mis-match case is still handled — by undo, which is one row
    /// delete and touches no file.
    #[tokio::test]
    async fn the_local_key_carries_the_episode() {
        let pool = open_memory().await.unwrap();
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
        library::sync_seasons(&pool, series.id, &[two_episode_season(4)])
            .await
            .unwrap();
        let eps = library::episodes(&pool, series.id).await.unwrap();

        let mut first = local(series.id, "/midias/Series/The Boys/s04e01.mkv");
        first.episode_id = Some(eps[0].id);
        let mut again = local(series.id, "/midias/Series/The Boys/s04e01.mkv");
        again.episode_id = Some(eps[1].id);

        assert!(reserve_local(&pool, &first).await.unwrap().is_some());
        assert!(
            reserve_local(&pool, &again).await.unwrap().is_some(),
            "one file covering two episodes is two rows, because it covers two episodes"
        );
        // The key gained the episode; it did not stop being a key. Same
        // path, same episode, still refused — which is what makes the
        // passive sweep safe to run on a timer.
        assert!(
            reserve_local(&pool, &first).await.unwrap().is_none(),
            "the same file against the same episode is still adopted once"
        );
    }

    /// A local row carries `provider_id = NULL`, which puts it outside
    /// both tracker indexes. It must not weaken them.
    #[tokio::test]
    async fn a_local_grab_does_not_disturb_the_tracker_barrier() {
        let pool = open_memory().await.unwrap();
        let (item_id, provider_id) = fixture(&pool).await;
        assert!(
            reserve_local(&pool, &local(item_id, "/midias/Filmes/x.mkv"))
                .await
                .unwrap()
                .is_some()
        );
        assert!(
            reserve(&pool, &new_grab(item_id, provider_id, "abc"))
                .await
                .unwrap()
                .is_some(),
            "an adoption must not block a tracker acquisition"
        );
        assert!(
            reserve(&pool, &new_grab(item_id, provider_id, "abc"))
                .await
                .unwrap()
                .is_none(),
            "and the tracker key still holds"
        );
    }

    /// `covers` is the SQL predicate rewritten in Rust so a 120-episode
    /// preview asks one question instead of 120. The two must not drift.
    #[tokio::test]
    async fn covers_agrees_with_blocking_for() {
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
                        tmdb_episode_id: None,
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
                        tmdb_episode_id: None,
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
        let (e4, e5) = (&eps[0], &eps[1]);

        // One grab of each shape the barrier can see.
        let mut per_episode = new_grab(series.id, provider_id, "ep");
        per_episode.episode_id = Some(e4.id);
        reserve(&pool, &per_episode).await.unwrap();

        let whole = new_grab(series.id, provider_id, "whole");
        reserve(&pool, &whole).await.unwrap();

        let mut pack = new_grab(series.id, provider_id, "pack");
        pack.season_number = Some(4);
        reserve(&pool, &pack).await.unwrap();

        reserve_local(&pool, &local(series.id, "/midias/x.mkv"))
            .await
            .unwrap();

        // The fifth shape, and the one that used to make the two
        // implementations agree on the *wrong* answer: a per-episode
        // grab whose FK a metadata refresh nulled. It has to be in the
        // matrix, because a `covers` that special-cases it and a SQL
        // clause that does not is exactly the drift this test exists
        // for.
        let mut decayed = new_grab(series.id, provider_id, "decayed");
        decayed.episode_id = Some(e5.id);
        let decayed = reserve(&pool, &decayed).await.unwrap().unwrap();
        sqlx::query("UPDATE grabs SET episode_id = NULL WHERE id = ?")
            .bind(decayed.id.to_string())
            .execute(&pool)
            .await
            .unwrap();

        let all = for_item(&pool, series.id).await.unwrap();
        for target in [
            GrabTarget::item(),
            GrabTarget::episode(e4.id, 4),
            GrabTarget::episode(e5.id, 5),
        ] {
            let from_sql: Vec<Uuid> = blocking_for(&pool, series.id, target)
                .await
                .unwrap()
                .iter()
                .map(|g| g.id)
                .collect();
            for grab in &all {
                assert_eq!(
                    covers(grab, target),
                    from_sql.contains(&grab.id),
                    "covers disagreed with blocking_for for grab {} ({:?}) on target {target:?}",
                    grab.release_id_remote,
                    grab.episode_id,
                );
            }
        }
    }

    #[tokio::test]
    async fn in_place_is_told_apart_from_a_link() {
        let pool = open_memory().await.unwrap();
        let (item_id, _) = fixture(&pool).await;
        let path = "/midias/Filmes/Matrix (1999)/Matrix (1999).mkv";

        let adopted = reserve_local(&pool, &local(item_id, path))
            .await
            .unwrap()
            .unwrap();
        mark_imported(&pool, adopted.id, path).await.unwrap();
        assert!(is_in_place(&get_by_id(&pool, adopted.id).await.unwrap()));

        let linked = reserve_local(&pool, &local(item_id, "/data/torrents/y.mkv"))
            .await
            .unwrap()
            .unwrap();
        mark_imported(&pool, linked.id, "/midias/Filmes/y/y.mkv")
            .await
            .unwrap();
        assert!(
            !is_in_place(&get_by_id(&pool, linked.id).await.unwrap()),
            "a link wrote a file, so undo has something to remove"
        );
    }

    #[tokio::test]
    async fn delete_adopted_refuses_a_tracker_grab() {
        let pool = open_memory().await.unwrap();
        let (item_id, provider_id) = fixture(&pool).await;
        let tracker = reserve(&pool, &new_grab(item_id, provider_id, "abc"))
            .await
            .unwrap()
            .unwrap();
        assert!(
            delete_adopted(&pool, tracker.id).await.is_err(),
            "acquisition history is never deleted from the UI"
        );
        assert!(get_by_id(&pool, tracker.id).await.is_ok());

        let adopted = reserve_local(&pool, &local(item_id, "/midias/x.mkv"))
            .await
            .unwrap()
            .unwrap();
        let removed = delete_adopted(&pool, adopted.id).await.unwrap();
        assert_eq!(removed.id, adopted.id);
        assert!(get_by_id(&pool, adopted.id).await.is_err());
    }

    #[tokio::test]
    async fn stale_local_reservations_are_cleared_and_fresh_ones_are_not() {
        let pool = open_memory().await.unwrap();
        let (item_id, _) = fixture(&pool).await;
        let old = reserve_local(&pool, &local(item_id, "/midias/old.mkv"))
            .await
            .unwrap()
            .unwrap();
        let fresh = reserve_local(&pool, &local(item_id, "/midias/fresh.mkv"))
            .await
            .unwrap()
            .unwrap();
        // Age the first one past the cutoff.
        sqlx::query("UPDATE grabs SET updated_at = ? WHERE id = ?")
            .bind(OffsetDateTime::now_utc().unix_timestamp() - 7200)
            .bind(old.id.to_string())
            .execute(&pool)
            .await
            .unwrap();

        let cutoff = OffsetDateTime::now_utc() - time::Duration::hours(1);
        let cleared = clear_stale_local_reservations(&pool, item_id, cutoff)
            .await
            .unwrap();
        assert_eq!(cleared, 1);
        assert!(get_by_id(&pool, old.id).await.is_err());
        assert!(get_by_id(&pool, fresh.id).await.is_ok());
    }

    #[tokio::test]
    async fn a_file_that_came_back_covers_its_item_again() {
        let pool = open_memory().await.unwrap();
        let (item_id, _) = fixture(&pool).await;
        let path = "/midias/Filmes/Matrix (1999)/Matrix (1999).mkv";
        let adopted = reserve_local(&pool, &local(item_id, path))
            .await
            .unwrap()
            .unwrap();
        mark_imported(&pool, adopted.id, path).await.unwrap();
        mark_file_missing(&pool, adopted.id).await.unwrap();

        assert!(
            blocking_for(&pool, item_id, GrabTarget::item())
                .await
                .unwrap()
                .is_empty(),
            "a missing file frees the key"
        );
        assert!(clear_file_missing(&pool, adopted.id).await.unwrap());
        assert_eq!(
            blocking_for(&pool, item_id, GrabTarget::item())
                .await
                .unwrap()
                .len(),
            1,
            "and putting it back resumes coverage"
        );
        assert!(
            !clear_file_missing(&pool, adopted.id).await.unwrap(),
            "clearing a row that is not marked changes nothing"
        );
    }
}
