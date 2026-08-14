//! `library_items` / `library_seasons` / `library_episodes` — brarr's own
//! catalogue of monitored media.
//!
//! See `migrations/20260804120000_library.sql` for schema notes. The
//! split that matters here: **metadata is a cache, monitoring is state**.
//! A TMDB refresh rewrites title/overview/poster/dates but must never
//! touch `monitored`, `profile_id`, `root_folder` or `added_at` — those
//! belong to the operator, not to the upstream API.

use std::collections::{HashMap, HashSet};

use sqlx::{Row, sqlite::SqliteRow};
use time::OffsetDateTime;
use uuid::Uuid;

use brarr_core::{ExternalId, MetadataSource};

use crate::db::item_ids;
use crate::{AppError, db::Pool};

/// Movie or series. Persisted as the short label in `media_type`.
///
/// **Declared in `brarr-core` and re-exported here**, so the metadata
/// provider trait can dispatch on media kind without depending on this
/// module — a leaf crate cannot import the orchestrator's data layer, and
/// two enums with the same two variants would need converting at every
/// boundary. Call sites keep saying `library::MediaType`.
pub use brarr_core::MediaType;

/// Read the persisted label, wording the refusal for this crate.
///
/// The parse itself lives on the core type; only the error type is local,
/// because `brarr-core` has no [`AppError`] and should not grow one.
///
/// # Errors
///
/// Returns [`AppError::InvalidInput`] for anything the CHECK constraint
/// would have rejected.
pub fn media_type_from_label(raw: &str) -> Result<MediaType, AppError> {
    MediaType::parse(raw)
        .ok_or_else(|| AppError::InvalidInput(format!("unknown library_items.media_type: {raw}")))
}

/// How much of a title the operator wants chased.
///
/// Persisted in `library_items.monitor_scope`, and this column's job is
/// **deliberately narrow**: it decides the default for a season or
/// episode row [`sync_seasons`] has never seen before. It is not, and
/// must never be read as, a summary of what is monitored right now — the
/// tree is the truth for that, and the operator can edit it by hand at
/// any time.
///
/// Without the column the default is always "monitored", so "only the
/// first season" silently becomes "all seasons" the day TMDB publishes
/// the second — the same invisible default this work exists to remove,
/// one refresh later.
///
/// There is no "latest season only" variant: it would need a never-seen
/// row to be monitored (the new season) *and* the previous one to stop
/// being monitored, and a single stored value cannot express both.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MonitorScope {
    /// Everything, specials included. The column default, and therefore
    /// behaviour identical to before this migration.
    #[default]
    All,
    /// Only episodes that have not aired yet. An episode with no date
    /// counts as future: TMDB leaves the field blank until it schedules
    /// one, and marking it unmonitored would strand it forever — the
    /// tree preserves flags by number, so it would never come back on
    /// its own.
    FutureEpisodes,
    /// Only the lowest-numbered real season.
    FirstSeason,
    /// Catalogued and never chased.
    Nothing,
}

impl MonitorScope {
    /// Persisted label, and what the dialog posts.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::All => "all",
            Self::FutureEpisodes => "future",
            Self::FirstSeason => "first-season",
            Self::Nothing => "none",
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
            "all" => Ok(Self::All),
            "future" => Ok(Self::FutureEpisodes),
            "first-season" => Ok(Self::FirstSeason),
            "none" => Ok(Self::Nothing),
            other => Err(AppError::InvalidInput(format!(
                "monitoramento inválido: {other}"
            ))),
        }
    }

    /// Whether `library_items.monitored` — the one flag [`monitored`]
    /// reads — stays on.
    #[must_use]
    pub fn monitors_item(self) -> bool {
        self != Self::Nothing
    }

    /// Whether a season/episode row this scope has never seen starts
    /// monitored. `aired` is `false` for an episode with no air date or
    /// one still in the future.
    #[must_use]
    pub fn wants_new_row(self, season_number: i32, first_season: i32, aired: bool) -> bool {
        match self {
            Self::All => true,
            Self::Nothing => false,
            Self::FirstSeason => season_number == first_season,
            Self::FutureEpisodes => !aired,
        }
    }
}

/// One catalogue entry.
#[derive(Debug, Clone)]
pub struct LibraryItem {
    /// Stable UUID v4.
    pub id: Uuid,
    /// Movie or series.
    pub media_type: MediaType,
    /// TMDB id — the library's canonical axis.
    pub tmdb_id: i64,
    /// Canonical `ttNNNNNNN` form, prefix included.
    pub imdb_id: Option<String>,
    /// Series only; TMDB does not expose a tvdb id for movies.
    pub tvdb_id: Option<i64>,
    /// Localised title (pt-BR when a translation exists).
    pub title: String,
    /// Original-language title.
    pub original_title: Option<String>,
    /// Release / first-air year.
    pub year: Option<i32>,
    /// Synopsis. May be empty — TMDB has no automatic language fallback.
    pub overview: Option<String>,
    /// Poster path relative to the TMDB image CDN. Never the bytes.
    pub poster_path: Option<String>,
    /// Backdrop path, same rules as [`Self::poster_path`].
    pub backdrop_path: Option<String>,
    /// TMDB status string (`Returning Series`, `Ended`, `Released`, …).
    pub tmdb_status: Option<String>,
    /// Runtime in minutes (movies; episode average for series).
    pub runtime_minutes: Option<i32>,
    /// Air date of the next unaired episode, when known.
    pub next_air_date: Option<OffsetDateTime>,
    /// Digital release date — searching before this is wasted effort.
    pub digital_release_at: Option<OffsetDateTime>,
    /// Physical release date.
    pub physical_release_at: Option<OffsetDateTime>,
    /// Whether the scanner should look for missing files.
    pub monitored: bool,
    /// Quality profile driving the score threshold.
    pub profile_id: Option<Uuid>,
    /// Destination directory handed to the download client.
    pub root_folder: Option<String>,
    /// How much of the title to chase. Governs the default for season
    /// and episode rows [`sync_seasons`] has never seen — **not** a
    /// summary of what is monitored now. See the migration.
    pub monitor_scope: MonitorScope,
    /// When the operator added it.
    pub added_at: OffsetDateTime,
    /// When the TMDB metadata was last refreshed. Drives the TTL sweep.
    pub metadata_refreshed_at: OffsetDateTime,
}

/// Metadata for an insert-or-refresh. Deliberately carries *only* the
/// TMDB-owned fields: operator state is set through the dedicated
/// setters so a metadata refresh can never clobber it.
#[derive(Debug, Clone, Default)]
pub struct NewLibraryItem {
    /// Movie or series.
    pub media_type: Option<MediaType>,
    /// TMDB id.
    pub tmdb_id: i64,
    /// Canonical `ttNNNNNNN`.
    pub imdb_id: Option<String>,
    /// Series only.
    pub tvdb_id: Option<i64>,
    /// Localised title.
    pub title: String,
    /// Original-language title.
    pub original_title: Option<String>,
    /// Release / first-air year.
    pub year: Option<i32>,
    /// Synopsis.
    pub overview: Option<String>,
    /// Poster path.
    pub poster_path: Option<String>,
    /// Backdrop path.
    pub backdrop_path: Option<String>,
    /// TMDB status string.
    pub tmdb_status: Option<String>,
    /// Runtime in minutes.
    pub runtime_minutes: Option<i32>,
    /// Next episode air date.
    pub next_air_date: Option<OffsetDateTime>,
    /// Digital release date.
    pub digital_release_at: Option<OffsetDateTime>,
    /// Physical release date.
    pub physical_release_at: Option<OffsetDateTime>,
}

/// One season of a series.
#[derive(Debug, Clone)]
pub struct Season {
    /// Stable UUID v4.
    pub id: Uuid,
    /// Parent item.
    pub item_id: Uuid,
    /// Season number; `0` is the specials season on TMDB.
    pub season_number: i32,
    /// Episode count reported by TMDB.
    pub episode_count: i32,
    /// First air date of the season.
    pub air_date: Option<OffsetDateTime>,
    /// Whether the scanner should chase this season.
    pub monitored: bool,
}

/// One episode.
#[derive(Debug, Clone)]
pub struct Episode {
    /// Stable UUID v4.
    pub id: Uuid,
    /// TMDB's own episode id, once a refresh has filled it in.
    pub tmdb_episode_id: Option<i64>,
    /// Parent item.
    pub item_id: Uuid,
    /// Parent season.
    pub season_id: Uuid,
    /// Denormalised from the season for cheap filtering.
    pub season_number: i32,
    /// Episode number within the season.
    pub episode_number: i32,
    /// Episode title.
    pub title: Option<String>,
    /// Air date.
    pub air_date: Option<OffsetDateTime>,
    /// Whether the scanner should chase this episode.
    pub monitored: bool,
}

/// A season plus its episodes, as returned by a TMDB season fetch.
#[derive(Debug, Clone)]
pub struct NewSeason {
    /// Season number.
    pub season_number: i32,
    /// Episode count reported by TMDB.
    pub episode_count: i32,
    /// First air date.
    pub air_date: Option<OffsetDateTime>,
    /// Episodes belonging to this season.
    pub episodes: Vec<NewEpisode>,
}

/// One episode from a TMDB season fetch.
#[derive(Debug, Clone)]
pub struct NewEpisode {
    /// TMDB's own episode id, when the payload carried one.
    ///
    /// The identity that survives a re-numbering: neither the local UUID
    /// nor the (season, episode) pair does, and this is what lets an
    /// episode move between seasons as an UPDATE rather than a delete
    /// plus an insert.
    pub tmdb_episode_id: Option<i64>,
    /// Episode number within the season.
    pub episode_number: i32,
    /// Episode title.
    pub title: Option<String>,
    /// Air date.
    pub air_date: Option<OffsetDateTime>,
}

/// Headline counters for the dashboard.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct LibraryCounts {
    /// Monitored movies.
    pub movies: u64,
    /// Monitored series.
    pub series: u64,
    /// Catalogue entries with monitoring switched off.
    pub unmonitored: u64,
}

fn uuid_at(row: &SqliteRow, col: &str) -> Result<Uuid, AppError> {
    let raw: String = row.try_get(col)?;
    Uuid::parse_str(&raw)
        .map_err(|e| AppError::InvalidInput(format!("invalid uuid in library.{col}: {e}")))
}

fn opt_uuid_at(row: &SqliteRow, col: &str) -> Result<Option<Uuid>, AppError> {
    let raw: Option<String> = row.try_get(col)?;
    match raw {
        Some(s) => Ok(Some(Uuid::parse_str(&s).map_err(|e| {
            AppError::InvalidInput(format!("invalid uuid in library.{col}: {e}"))
        })?)),
        None => Ok(None),
    }
}

fn ts_at(row: &SqliteRow, col: &str) -> Result<OffsetDateTime, AppError> {
    let raw: i64 = row.try_get(col)?;
    OffsetDateTime::from_unix_timestamp(raw)
        .map_err(|e| AppError::InvalidInput(format!("invalid timestamp in library.{col}: {e}")))
}

fn opt_ts_at(row: &SqliteRow, col: &str) -> Result<Option<OffsetDateTime>, AppError> {
    let raw: Option<i64> = row.try_get(col)?;
    match raw {
        // A stored value outside the representable range is bad data, not
        // a reason to fail the whole read — degrade to "unknown date".
        Some(v) => Ok(OffsetDateTime::from_unix_timestamp(v).ok()),
        None => Ok(None),
    }
}

fn row_to_item(row: &SqliteRow) -> Result<LibraryItem, AppError> {
    let media_type_raw: String = row.try_get("media_type")?;
    let monitored: i64 = row.try_get("monitored")?;
    let scope_raw: String = row.try_get("monitor_scope")?;
    Ok(LibraryItem {
        id: uuid_at(row, "id")?,
        media_type: media_type_from_label(&media_type_raw)?,
        tmdb_id: row.try_get("tmdb_id")?,
        imdb_id: row.try_get("imdb_id")?,
        tvdb_id: row.try_get("tvdb_id")?,
        title: row.try_get("title")?,
        original_title: row.try_get("original_title")?,
        year: row
            .try_get::<Option<i64>, _>("year")?
            .and_then(|v| i32::try_from(v).ok()),
        overview: row.try_get("overview")?,
        poster_path: row.try_get("poster_path")?,
        backdrop_path: row.try_get("backdrop_path")?,
        tmdb_status: row.try_get("tmdb_status")?,
        runtime_minutes: row
            .try_get::<Option<i64>, _>("runtime_minutes")?
            .and_then(|v| i32::try_from(v).ok()),
        next_air_date: opt_ts_at(row, "next_air_date")?,
        digital_release_at: opt_ts_at(row, "digital_release_at")?,
        physical_release_at: opt_ts_at(row, "physical_release_at")?,
        monitored: monitored != 0,
        profile_id: opt_uuid_at(row, "profile_id")?,
        root_folder: row.try_get("root_folder")?,
        monitor_scope: MonitorScope::from_label(&scope_raw)?,
        added_at: ts_at(row, "added_at")?,
        metadata_refreshed_at: ts_at(row, "metadata_refreshed_at")?,
    })
}

const ITEM_COLUMNS: &str = "id, media_type, tmdb_id, imdb_id, tvdb_id, title, original_title, \
     year, overview, poster_path, backdrop_path, tmdb_status, runtime_minutes, \
     next_air_date, digital_release_at, physical_release_at, monitored, \
     profile_id, root_folder, monitor_scope, added_at, metadata_refreshed_at";

/// Insert a catalogue entry, or refresh the metadata of the existing one
/// with the same `(media_type, tmdb_id)`.
///
/// The `ON CONFLICT` branch updates only TMDB-owned columns. `monitored`,
/// `profile_id`, `root_folder` and `added_at` are left untouched, so a
/// metadata sweep can run over the whole library without resetting what
/// the operator configured.
///
/// # Errors
///
/// Returns [`AppError::InvalidInput`] when `media_type` is absent, and
/// [`AppError::Database`] on SQL failure.
pub async fn upsert(pool: &Pool, new: &NewLibraryItem) -> Result<LibraryItem, AppError> {
    let media_type = new
        .media_type
        .ok_or_else(|| AppError::InvalidInput("library item needs a media_type".to_owned()))?;
    let id = Uuid::new_v4();
    let now = OffsetDateTime::now_utc().unix_timestamp();
    sqlx::query(
        "INSERT INTO library_items ( \
            id, media_type, tmdb_id, imdb_id, tvdb_id, title, original_title, year, \
            overview, poster_path, backdrop_path, tmdb_status, runtime_minutes, \
            next_air_date, digital_release_at, physical_release_at, \
            monitored, added_at, metadata_refreshed_at \
         ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, 1, ?, ?) \
         ON CONFLICT(media_type, tmdb_id) DO UPDATE SET \
            imdb_id = excluded.imdb_id, \
            tvdb_id = excluded.tvdb_id, \
            title = excluded.title, \
            original_title = excluded.original_title, \
            year = excluded.year, \
            overview = excluded.overview, \
            poster_path = excluded.poster_path, \
            backdrop_path = excluded.backdrop_path, \
            tmdb_status = excluded.tmdb_status, \
            runtime_minutes = excluded.runtime_minutes, \
            next_air_date = excluded.next_air_date, \
            digital_release_at = excluded.digital_release_at, \
            physical_release_at = excluded.physical_release_at, \
            metadata_refreshed_at = excluded.metadata_refreshed_at",
    )
    .bind(id.to_string())
    .bind(media_type.label())
    .bind(new.tmdb_id)
    .bind(new.imdb_id.as_deref())
    .bind(new.tvdb_id)
    .bind(&new.title)
    .bind(new.original_title.as_deref())
    .bind(new.year.map(i64::from))
    .bind(new.overview.as_deref())
    .bind(new.poster_path.as_deref())
    .bind(new.backdrop_path.as_deref())
    .bind(new.tmdb_status.as_deref())
    .bind(new.runtime_minutes.map(i64::from))
    .bind(new.next_air_date.map(OffsetDateTime::unix_timestamp))
    .bind(new.digital_release_at.map(OffsetDateTime::unix_timestamp))
    .bind(new.physical_release_at.map(OffsetDateTime::unix_timestamp))
    .bind(now)
    .bind(now)
    .execute(pool)
    .await?;
    let stored = get_by_tmdb(pool, media_type, new.tmdb_id).await?;
    write_identity(pool, &stored).await?;
    Ok(stored)
}

/// Mirror the three identity columns into `library_item_ids`.
///
/// The double write exists for one phase only. Two sources of truth is
/// where this repository's defects hide, so it comes with a date to
/// close: the readers move next, the columns come out with the wipe that
/// precedes the re-import, and this function goes with them.
///
/// Deliberately **not** transactional with the row above. A failure here
/// would otherwise roll back a catalogue write over a table nothing reads
/// yet; the reconciliation is a `SELECT` away for as long as both forms
/// exist, and after that the new form is the only one.
async fn write_identity(pool: &Pool, item: &LibraryItem) -> Result<(), AppError> {
    let mut ids = Vec::with_capacity(3);
    if let Ok(id) = ExternalId::new(MetadataSource::Tmdb, &item.tmdb_id.to_string()) {
        ids.push(id);
    }
    if let Some(raw) = item.imdb_id.as_deref()
        && let Ok(id) = ExternalId::new(MetadataSource::Imdb, raw)
    {
        ids.push(id);
    }
    if let Some(raw) = item.tvdb_id
        && let Ok(id) = ExternalId::new(MetadataSource::Tvdb, &raw.to_string())
    {
        ids.push(id);
    }
    for id in &ids {
        // Asserted, never vouched: these came off a column that records
        // only the value, not who confirmed it. Claiming otherwise would
        // stop a sweep from ever checking a pairing nobody checked.
        item_ids::put(
            pool,
            item.id,
            item.media_type,
            id,
            item_ids::Verification::Asserted,
        )
        .await?;
    }
    Ok(())
}

/// Fetch one entry by primary key.
///
/// # Errors
///
/// Returns [`AppError::NotFound`] when absent, [`AppError::Database`]
/// on SQL failure.
pub async fn get_by_id(pool: &Pool, id: Uuid) -> Result<LibraryItem, AppError> {
    let row = sqlx::query(&format!(
        "SELECT {ITEM_COLUMNS} FROM library_items WHERE id = ?"
    ))
    .bind(id.to_string())
    .fetch_optional(pool)
    .await?;
    match row {
        Some(r) => row_to_item(&r),
        None => Err(AppError::NotFound(format!("library_item {id}"))),
    }
}

/// Fetch one entry by its TMDB coordinates.
///
/// # Errors
///
/// Returns [`AppError::NotFound`] when absent, [`AppError::Database`]
/// on SQL failure.
pub async fn get_by_tmdb(
    pool: &Pool,
    media_type: MediaType,
    tmdb_id: i64,
) -> Result<LibraryItem, AppError> {
    let row = sqlx::query(&format!(
        "SELECT {ITEM_COLUMNS} FROM library_items WHERE media_type = ? AND tmdb_id = ?"
    ))
    .bind(media_type.label())
    .bind(tmdb_id)
    .fetch_optional(pool)
    .await?;
    match row {
        Some(r) => row_to_item(&r),
        None => Err(AppError::NotFound(format!(
            "library_item {}/{tmdb_id}",
            media_type.label()
        ))),
    }
}

/// Every entry, newest first.
///
/// # Errors
///
/// Returns [`AppError::Database`] on SQL failure.
pub async fn list(pool: &Pool) -> Result<Vec<LibraryItem>, AppError> {
    let rows = sqlx::query(&format!(
        "SELECT {ITEM_COLUMNS} FROM library_items ORDER BY added_at DESC, title ASC"
    ))
    .fetch_all(pool)
    .await?;
    rows.iter().map(row_to_item).collect()
}

/// Entries the scanner should chase, oldest-refreshed first so a sweep
/// naturally starts with the most stale metadata.
///
/// # Errors
///
/// Returns [`AppError::Database`] on SQL failure.
pub async fn monitored(pool: &Pool) -> Result<Vec<LibraryItem>, AppError> {
    let rows = sqlx::query(&format!(
        "SELECT {ITEM_COLUMNS} FROM library_items WHERE monitored = 1 \
         ORDER BY metadata_refreshed_at ASC"
    ))
    .fetch_all(pool)
    .await?;
    rows.iter().map(row_to_item).collect()
}

/// Flip the monitoring flag.
///
/// # Errors
///
/// Returns [`AppError::NotFound`] when the id is absent,
/// [`AppError::Database`] on SQL failure.
pub async fn set_monitored(pool: &Pool, id: Uuid, monitored: bool) -> Result<(), AppError> {
    let res = sqlx::query("UPDATE library_items SET monitored = ? WHERE id = ?")
        .bind(i64::from(monitored))
        .bind(id.to_string())
        .execute(pool)
        .await?;
    if res.rows_affected() == 0 {
        return Err(AppError::NotFound(format!("library_item {id}")));
    }
    Ok(())
}

/// Flip the monitoring flag on many titles at once.
///
/// One transaction rather than one statement with a built `IN (…)`
/// list: the id count is operator-chosen and unbounded, SQLite caps how
/// many parameters a statement may bind, and string-building the list
/// is the shape that invites an injection the day someone passes
/// something other than a `Uuid`. A few hundred updates inside one
/// transaction is a single fsync and a few milliseconds.
///
/// Returns how many rows actually changed. An id that no longer exists
/// is **not** an error here — a bulk action on a stale page should do
/// what it can and report the number, not fail whole because one title
/// was deleted in another tab.
///
/// # Errors
///
/// Returns [`AppError::Database`] on SQL failure.
pub async fn set_monitored_many(
    pool: &Pool,
    ids: &[Uuid],
    monitored: bool,
) -> Result<u64, AppError> {
    if ids.is_empty() {
        return Ok(0);
    }
    let mut tx = pool.begin().await?;
    let mut changed = 0;
    for id in ids {
        changed += sqlx::query("UPDATE library_items SET monitored = ? WHERE id = ?")
            .bind(i64::from(monitored))
            .bind(id.to_string())
            .execute(&mut *tx)
            .await?
            .rows_affected();
    }
    tx.commit().await?;
    Ok(changed)
}

/// Attach a quality profile to many titles at once. `None` detaches.
///
/// Same transaction-and-count contract as [`set_monitored_many`].
///
/// # Errors
///
/// Returns [`AppError::Database`] on SQL failure.
pub async fn set_profile_many(
    pool: &Pool,
    ids: &[Uuid],
    profile_id: Option<Uuid>,
) -> Result<u64, AppError> {
    if ids.is_empty() {
        return Ok(0);
    }
    let mut tx = pool.begin().await?;
    let mut changed = 0;
    for id in ids {
        changed += sqlx::query("UPDATE library_items SET profile_id = ? WHERE id = ?")
            .bind(profile_id.map(|p| p.to_string()))
            .bind(id.to_string())
            .execute(&mut *tx)
            .await?
            .rows_affected();
    }
    tx.commit().await?;
    Ok(changed)
}

/// Point many titles at a root folder. `None` restores "the default for
/// the type".
///
/// Deliberately does **not** touch `profile_id`, unlike
/// [`set_placement`], which writes both columns: a bulk action must
/// change only the thing it names, or selecting forty titles to set a
/// folder would silently blank forty hand-picked profiles.
///
/// # Errors
///
/// Returns [`AppError::Database`] on SQL failure.
pub async fn set_root_folder_many(
    pool: &Pool,
    ids: &[Uuid],
    root_folder: Option<&str>,
) -> Result<u64, AppError> {
    if ids.is_empty() {
        return Ok(0);
    }
    let mut tx = pool.begin().await?;
    let mut changed = 0;
    for id in ids {
        changed += sqlx::query("UPDATE library_items SET root_folder = ? WHERE id = ?")
            .bind(root_folder)
            .bind(id.to_string())
            .execute(&mut *tx)
            .await?
            .rows_affected();
    }
    tx.commit().await?;
    Ok(changed)
}

/// Set how much of a title to chase.
///
/// Also flips `monitored`, because [`MonitorScope::Nothing`] and an
/// unmonitored item are the same statement made twice — letting them
/// disagree would give the scanner and the screen two different answers.
///
/// # Errors
///
/// Returns [`AppError::NotFound`] when the id is absent,
/// [`AppError::Database`] on SQL failure.
pub async fn set_monitor_scope(pool: &Pool, id: Uuid, scope: MonitorScope) -> Result<(), AppError> {
    let res = sqlx::query("UPDATE library_items SET monitor_scope = ?, monitored = ? WHERE id = ?")
        .bind(scope.label())
        .bind(i64::from(scope.monitors_item()))
        .bind(id.to_string())
        .execute(pool)
        .await?;
    if res.rows_affected() == 0 {
        return Err(AppError::NotFound(format!("library_item {id}")));
    }
    Ok(())
}

/// Every item's title, by id. For screens that list grabs and need a
/// catalogue title per row without loading the whole library.
///
/// # Errors
///
/// Returns [`AppError::Database`] on SQL failure.
pub async fn titles_by_id(pool: &Pool) -> Result<HashMap<Uuid, String>, AppError> {
    let rows = sqlx::query("SELECT id, title FROM library_items")
        .fetch_all(pool)
        .await?;
    let mut out = HashMap::with_capacity(rows.len());
    for row in &rows {
        let id: String = row.try_get("id")?;
        let title: String = row.try_get("title")?;
        let id = Uuid::parse_str(&id)
            .map_err(|e| AppError::InvalidInput(format!("invalid library_items.id: {e}")))?;
        out.insert(id, title);
    }
    Ok(out)
}

/// Attach (or clear, with `None`) the quality profile and root folder.
///
/// # Errors
///
/// Returns [`AppError::NotFound`] when the id is absent,
/// [`AppError::Database`] on SQL failure.
pub async fn set_placement(
    pool: &Pool,
    id: Uuid,
    profile_id: Option<Uuid>,
    root_folder: Option<&str>,
) -> Result<(), AppError> {
    let res = sqlx::query("UPDATE library_items SET profile_id = ?, root_folder = ? WHERE id = ?")
        .bind(profile_id.map(|p| p.to_string()))
        .bind(root_folder)
        .bind(id.to_string())
        .execute(pool)
        .await?;
    if res.rows_affected() == 0 {
        return Err(AppError::NotFound(format!("library_item {id}")));
    }
    Ok(())
}

/// Drop an entry. Seasons, episodes and grabs cascade.
///
/// # Errors
///
/// Returns [`AppError::NotFound`] when the id is absent,
/// [`AppError::Database`] on SQL failure.
pub async fn delete(pool: &Pool, id: Uuid) -> Result<(), AppError> {
    let res = sqlx::query("DELETE FROM library_items WHERE id = ?")
        .bind(id.to_string())
        .execute(pool)
        .await?;
    if res.rows_affected() == 0 {
        return Err(AppError::NotFound(format!("library_item {id}")));
    }
    Ok(())
}

/// Headline counters for the dashboard stat tiles.
///
/// # Errors
///
/// Returns [`AppError::Database`] on SQL failure.
pub async fn counts(pool: &Pool) -> Result<LibraryCounts, AppError> {
    let row = sqlx::query(
        "SELECT \
            SUM(CASE WHEN media_type = 'movie' AND monitored = 1 THEN 1 ELSE 0 END) AS movies, \
            SUM(CASE WHEN media_type = 'tv'    AND monitored = 1 THEN 1 ELSE 0 END) AS series, \
            SUM(CASE WHEN monitored = 0 THEN 1 ELSE 0 END) AS unmonitored \
         FROM library_items",
    )
    .fetch_one(pool)
    .await?;
    let movies: i64 = row.try_get("movies").unwrap_or(0);
    let series: i64 = row.try_get("series").unwrap_or(0);
    let unmonitored: i64 = row.try_get("unmonitored").unwrap_or(0);
    Ok(LibraryCounts {
        movies: u64::try_from(movies).unwrap_or(0),
        series: u64::try_from(series).unwrap_or(0),
        unmonitored: u64::try_from(unmonitored).unwrap_or(0),
    })
}

/// A season row as it stands before a sync: its identity and the flag
/// the operator set.
#[derive(Debug, Clone, Copy)]
struct StoredSeason {
    id: Uuid,
    monitored: bool,
}

/// An episode row as it stands before a sync.
#[derive(Debug, Clone, Copy)]
struct StoredEpisode {
    id: Uuid,
    monitored: bool,
}

/// The tree as currently stored, keyed the way TMDB numbers it.
///
/// The flags are what the operator chose. **The ids are what holds the
/// library to the disk**: `grabs.episode_id` is the only link between a
/// file and the episode it is, and the FK is `ON DELETE SET NULL`. A
/// refresh that recreated these rows unlinked every file, and an
/// unlinked grab reads as `(NULL, NULL)` — the encoding of "covers the
/// whole item" — so the series went on rendering as complete while
/// every episode pointed at an arbitrary file. Reusing the ids is the
/// whole reason this struct exists.
/// **Two keys, because neither one survives both changes an episode can
/// go through.** The number pair survives a UUID churn but is exactly
/// what a re-numbering changes; TMDB's episode id survives a
/// re-numbering but is absent until a refresh has filled it in. Matching
/// by the id first and falling back to the pair is what lets a series be
/// re-ordered without any row — or any grab — being lost.
struct StoredTree {
    /// By season number.
    seasons: HashMap<i32, StoredSeason>,
    /// By `(season_number, episode_number)`.
    episodes: HashMap<(i32, i32), StoredEpisode>,
    /// By TMDB episode id, for the rows that carry one.
    by_tmdb: HashMap<i64, StoredEpisode>,
}

impl StoredTree {
    /// The row this payload episode belongs to, if the tree already has
    /// it. The id wins: it is the only key that means the same thing
    /// before and after a re-numbering.
    fn resolve(&self, tmdb_id: Option<i64>, season: i32, number: i32) -> Option<StoredEpisode> {
        tmdb_id
            .and_then(|id| self.by_tmdb.get(&id))
            .or_else(|| self.episodes.get(&(season, number)))
            .copied()
    }
}

/// Read the tree the operator already has, ids included.
///
/// Split out of [`sync_seasons`] to keep that function under the line
/// limit, and because "what is stored right now" is a question worth
/// being able to ask on its own.
async fn existing_tree(pool: &Pool, item_id: Uuid) -> Result<StoredTree, AppError> {
    let existing =
        sqlx::query("SELECT id, season_number, monitored FROM library_seasons WHERE item_id = ?")
            .bind(item_id.to_string())
            .fetch_all(pool)
            .await?;
    let mut seasons = HashMap::with_capacity(existing.len());
    for row in &existing {
        let number: i64 = row.try_get("season_number")?;
        let flag: i64 = row.try_get("monitored")?;
        seasons.insert(
            i32::try_from(number).unwrap_or(0),
            StoredSeason {
                id: uuid_at(row, "id")?,
                monitored: flag != 0,
            },
        );
    }

    let existing_eps = sqlx::query(
        "SELECT id, season_number, episode_number, monitored, tmdb_episode_id \
         FROM library_episodes WHERE item_id = ?",
    )
    .bind(item_id.to_string())
    .fetch_all(pool)
    .await?;
    let mut episodes = HashMap::with_capacity(existing_eps.len());
    let mut by_tmdb = HashMap::with_capacity(existing_eps.len());
    for row in &existing_eps {
        let s: i64 = row.try_get("season_number")?;
        let e: i64 = row.try_get("episode_number")?;
        let flag: i64 = row.try_get("monitored")?;
        let tmdb: Option<i64> = row.try_get("tmdb_episode_id")?;
        let stored = StoredEpisode {
            id: uuid_at(row, "id")?,
            monitored: flag != 0,
        };
        episodes.insert(
            (i32::try_from(s).unwrap_or(0), i32::try_from(e).unwrap_or(0)),
            stored,
        );
        if let Some(id) = tmdb {
            by_tmdb.insert(id, stored);
        }
    }
    Ok(StoredTree {
        seasons,
        episodes,
        by_tmdb,
    })
}

/// The item's [`MonitorScope`]. A missing row falls back to
/// [`MonitorScope::All`] — the pre-migration behaviour, and the only
/// safe answer when there is nothing to read.
async fn item_scope(pool: &Pool, item_id: Uuid) -> Result<MonitorScope, AppError> {
    let row = sqlx::query("SELECT monitor_scope FROM library_items WHERE id = ?")
        .bind(item_id.to_string())
        .fetch_optional(pool)
        .await?;
    match row {
        Some(row) => MonitorScope::from_label(&row.try_get::<String, _>("monitor_scope")?),
        None => Ok(MonitorScope::All),
    }
}

/// Refresh the season/episode tree of a series from a TMDB fetch.
///
/// Monitoring flags of seasons and episodes that survive the sync are
/// preserved — the same reasoning as [`upsert`]: TMDB owns the shape,
/// the operator owns what gets chased. Seasons TMDB no longer reports
/// are dropped (and their episodes cascade).
///
/// A row this has never seen takes its default from the item's
/// [`MonitorScope`], which is why "only the first season" survives a
/// refresh that publishes a second one.
///
/// **A surviving row keeps its `id`, and that is the point.** This used
/// to `DELETE` the item's seasons and reinsert everything with fresh
/// UUIDs, which cost nothing visible in the tree — the flags were
/// recopied by number — but nulled every `grabs.episode_id` pointing
/// into it. Since the passive \*arr sweep calls this for every series on
/// every pass, an operator's whole TV library was unlinked from its
/// files every half hour, and because an unlinked grab reads as
/// `(NULL, NULL)` — "covers the whole item" — the damage rendered as
/// *complete*, not as missing. Upserting by number keeps the ids, so the
/// only rows that lose their link are the ones TMDB genuinely dropped.
///
/// **An episode that moves keeps its row.** `(1, 15)` becoming `(2, 1)`
/// under an alternate ordering used to be a new key, so a delete plus an
/// insert, so an unlinked file. Matching on TMDB's episode id — the only
/// identity that means the same thing before and after a re-numbering —
/// makes it an UPDATE of two integers on a row that lives. The number
/// pair remains the fallback for rows a refresh has not filled the id
/// into yet, which is exactly the behaviour that shipped before.
///
/// # Errors
///
/// Returns [`AppError::Database`] on SQL failure.
pub async fn sync_seasons(
    pool: &Pool,
    item_id: Uuid,
    seasons: &[NewSeason],
) -> Result<(), AppError> {
    let stored = existing_tree(pool, item_id).await?;

    // A row this function has never seen takes its default from the
    // item's scope. Rows it *has* seen keep whatever the operator set —
    // the scope decides defaults, it does not overwrite choices.
    let scope = item_scope(pool, item_id).await?;
    // Season 0 is TMDB's specials bucket and never counts as "the first
    // season" — The Boys carries 76 specials against 40 real episodes.
    let first_season = seasons
        .iter()
        .map(|s| s.season_number)
        .filter(|n| *n > 0)
        .min()
        .unwrap_or(1);
    let now = OffsetDateTime::now_utc();

    // One transaction: a failure halfway used to leave the item with a
    // partially rebuilt tree, because the `DELETE` had already committed.
    let mut tx = pool.begin().await?;
    park(&mut tx, item_id).await?;

    let mut claimed: HashSet<Uuid> = HashSet::new();
    for season in seasons {
        let known = stored.seasons.get(&season.season_number);
        let season_id = known.map_or_else(Uuid::new_v4, |s| s.id);
        let season_aired = season.air_date.is_some_and(|d| d <= now);
        let monitored = known.map_or_else(
            || scope.wants_new_row(season.season_number, first_season, season_aired),
            |s| s.monitored,
        );
        claimed.insert(season_id);
        upsert_season(&mut tx, season_id, item_id, season, monitored).await?;

        for episode in &season.episodes {
            // No air date counts as "not aired": TMDB leaves it blank
            // until it schedules one, and calling that aired would mark
            // the episode unmonitored under `future`, where it would
            // stay forever — the tree preserves flags by number, so it
            // could never come back on its own.
            let aired = episode.air_date.is_some_and(|d| d <= now);
            let known = stored.resolve(
                episode.tmdb_episode_id,
                season.season_number,
                episode.episode_number,
            );
            let ep_monitored = known.map_or_else(
                || scope.wants_new_row(season.season_number, first_season, aired),
                |e| e.monitored,
            );
            let ids = EpisodeIds {
                id: known.map_or_else(Uuid::new_v4, |e| e.id),
                item_id,
                season_id,
            };
            claimed.insert(ids.id);
            upsert_episode(&mut tx, ids, season.season_number, episode, ep_monitored).await?;
        }
    }
    prune_tree(&mut tx, &stored, &claimed).await?;
    tx.commit().await?;
    Ok(())
}

/// Move every row of this item out of the positive key space.
///
/// `n → -1 - n` is a bijection into the negatives, so the pairs stay
/// unique and nothing collides while parked. Without it a re-numbering
/// that *permutes* numbers — switching between two orderings — aborts
/// mid-transaction on `idx_library_episodes_number`, because SQLite
/// checks uniqueness per statement and has no deferred constraints.
/// After parking, every final number is free by construction.
///
/// Rows the payload does not claim stay parked and are deleted by
/// [`prune_tree`], which is also what makes "still parked" the honest
/// definition of "TMDB no longer reports this".
async fn park(conn: &mut sqlx::SqliteConnection, item_id: Uuid) -> Result<(), AppError> {
    sqlx::query("UPDATE library_seasons SET season_number = -1 - season_number WHERE item_id = ?")
        .bind(item_id.to_string())
        .execute(&mut *conn)
        .await?;
    sqlx::query("UPDATE library_episodes SET season_number = -1 - season_number WHERE item_id = ?")
        .bind(item_id.to_string())
        .execute(&mut *conn)
        .await?;
    Ok(())
}

/// The three ids an episode row carries, grouped so [`upsert_episode`]
/// stays under the argument threshold.
#[derive(Debug, Clone, Copy)]
struct EpisodeIds {
    /// The row's own id — reused when the episode already exists.
    id: Uuid,
    /// Series it belongs to.
    item_id: Uuid,
    /// Parent season row.
    season_id: Uuid,
}

/// Insert a season, or update the row already carrying this id.
///
/// **The conflict target is the primary key, not the number**, because
/// [`park`] has moved every existing row out of the number space by the
/// time this runs — a season being renumbered no longer collides with
/// the number it is leaving, and the row that owns the id is the one to
/// update.
async fn upsert_season(
    conn: &mut sqlx::SqliteConnection,
    id: Uuid,
    item_id: Uuid,
    season: &NewSeason,
    monitored: bool,
) -> Result<(), AppError> {
    sqlx::query(
        "INSERT INTO library_seasons \
            (id, item_id, season_number, episode_count, air_date, monitored) \
         VALUES (?, ?, ?, ?, ?, ?) \
         ON CONFLICT(id) DO UPDATE SET \
            season_number = excluded.season_number, \
            episode_count = excluded.episode_count, \
            air_date      = excluded.air_date, \
            monitored     = excluded.monitored",
    )
    .bind(id.to_string())
    .bind(item_id.to_string())
    .bind(i64::from(season.season_number))
    .bind(i64::from(season.episode_count))
    .bind(season.air_date.map(OffsetDateTime::unix_timestamp))
    .bind(i64::from(monitored))
    .execute(conn)
    .await?;
    Ok(())
}

/// Insert an episode, or update the one already stored under this
/// `(season_number, episode_number)`.
///
/// `season_id` *is* updated: the parent row survives a refresh now, but
/// a season that TMDB dropped and republished gets a new one, and the
/// child has to follow it or the CASCADE would take the wrong rows.
async fn upsert_episode(
    conn: &mut sqlx::SqliteConnection,
    ids: EpisodeIds,
    season_number: i32,
    episode: &NewEpisode,
    monitored: bool,
) -> Result<(), AppError> {
    sqlx::query(
        "INSERT INTO library_episodes \
            (id, item_id, season_id, season_number, episode_number, title, air_date, \
             monitored, tmdb_episode_id) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?) \
         ON CONFLICT(id) DO UPDATE SET \
            season_id       = excluded.season_id, \
            season_number   = excluded.season_number, \
            episode_number  = excluded.episode_number, \
            title           = excluded.title, \
            air_date        = excluded.air_date, \
            monitored       = excluded.monitored, \
            tmdb_episode_id = COALESCE(excluded.tmdb_episode_id, tmdb_episode_id)",
    )
    .bind(ids.id.to_string())
    .bind(ids.item_id.to_string())
    .bind(ids.season_id.to_string())
    .bind(i64::from(season_number))
    .bind(i64::from(episode.episode_number))
    .bind(episode.title.as_deref())
    .bind(episode.air_date.map(OffsetDateTime::unix_timestamp))
    .bind(i64::from(monitored))
    .bind(episode.tmdb_episode_id)
    .execute(conn)
    .await?;
    Ok(())
}

/// Drop the rows TMDB no longer reports.
///
/// Deleting by id, from the set difference computed in Rust, rather than
/// by a `NOT IN` list built into the SQL: the payload is tens of rows and
/// the interesting part is which ones vanish, not how the statement is
/// assembled. Episodes go first so the reason each row died is the one
/// stated here, not an incidental CASCADE.
async fn prune_tree(
    conn: &mut sqlx::SqliteConnection,
    stored: &StoredTree,
    claimed: &HashSet<Uuid>,
) -> Result<(), AppError> {
    for episode in stored.episodes.values() {
        if !claimed.contains(&episode.id) {
            sqlx::query("DELETE FROM library_episodes WHERE id = ?")
                .bind(episode.id.to_string())
                .execute(&mut *conn)
                .await?;
        }
    }
    for season in stored.seasons.values() {
        if !claimed.contains(&season.id) {
            sqlx::query("DELETE FROM library_seasons WHERE id = ?")
                .bind(season.id.to_string())
                .execute(&mut *conn)
                .await?;
        }
    }
    Ok(())
}

/// One monitored episode, reduced to what a coverage count reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MonitoredEpisode {
    /// Series it belongs to.
    pub item_id: Uuid,
    /// Episode id, which is what a per-episode grab names.
    pub id: Uuid,
    /// Season, so a pack of another season does not answer for it.
    pub season_number: i32,
    /// Air date. `None` counts as "not aired" everywhere in brarr.
    pub air_date: Option<OffsetDateTime>,
}

/// Every monitored episode of every series, in one query.
///
/// **Season 0 is included.** The tree summary excludes specials because
/// they are not what anyone means by "the show", but the progress count
/// follows monitoring and nothing else — an excluded season makes the
/// operator's own toggle do nothing, which is how The Familiar of Zero
/// read 49/49 with a monitored special on disk. See [`crate::coverage`].
///
/// One query rather than one per item on purpose. The library index used
/// to call [`seasons`] and [`episodes`] per row to build its summary
/// line, which is two round trips per title — 720 of them once the \*arr
/// migration lands 360 titles.
///
/// # Errors
///
/// Returns [`AppError::Database`] on SQL failure.
pub async fn monitored_episodes(pool: &Pool) -> Result<Vec<MonitoredEpisode>, AppError> {
    let rows = sqlx::query(
        "SELECT item_id, id, season_number, air_date FROM library_episodes WHERE monitored = 1",
    )
    .fetch_all(pool)
    .await?;
    rows.iter()
        .map(|row| {
            Ok(MonitoredEpisode {
                item_id: uuid_at(row, "item_id")?,
                id: uuid_at(row, "id")?,
                season_number: i32::try_from(row.try_get::<i64, _>("season_number")?).unwrap_or(0),
                air_date: opt_ts_at(row, "air_date")?,
            })
        })
        .collect()
}

/// Season / episode / special counts per series, in one query.
///
/// Same reasoning as [`monitored_episodes`]: the index needs this for
/// every row, and asking per row is what made the page scale with the
/// catalogue.
///
/// # Errors
///
/// Returns [`AppError::Database`] on SQL failure.
pub async fn tree_counts(pool: &Pool) -> Result<HashMap<Uuid, TreeCounts>, AppError> {
    let rows = sqlx::query(
        "SELECT e.item_id, \
                COUNT(DISTINCT CASE WHEN e.season_number > 0 THEN e.season_number END) AS seasons, \
                SUM(CASE WHEN e.season_number > 0 THEN 1 ELSE 0 END) AS episodes, \
                SUM(CASE WHEN e.season_number = 0 THEN 1 ELSE 0 END) AS specials \
         FROM library_episodes e GROUP BY e.item_id",
    )
    .fetch_all(pool)
    .await?;
    let mut out = HashMap::with_capacity(rows.len());
    for row in &rows {
        let id = uuid_at(row, "item_id")?;
        out.insert(
            id,
            TreeCounts {
                seasons: usize::try_from(row.try_get::<i64, _>("seasons")?).unwrap_or(0),
                episodes: usize::try_from(row.try_get::<i64, _>("episodes")?).unwrap_or(0),
                specials: usize::try_from(row.try_get::<i64, _>("specials")?).unwrap_or(0),
            },
        );
    }
    Ok(out)
}

/// What one series' tree holds, specials counted apart.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TreeCounts {
    /// Real seasons — season 0 excluded.
    pub seasons: usize,
    /// Real episodes.
    pub episodes: usize,
    /// Specials, which The Boys has 76 of against 40 real episodes.
    pub specials: usize,
}

/// Seasons of a series, ascending.
///
/// # Errors
///
/// Returns [`AppError::Database`] on SQL failure.
pub async fn seasons(pool: &Pool, item_id: Uuid) -> Result<Vec<Season>, AppError> {
    let rows = sqlx::query(
        "SELECT id, item_id, season_number, episode_count, air_date, monitored \
         FROM library_seasons WHERE item_id = ? ORDER BY season_number ASC",
    )
    .bind(item_id.to_string())
    .fetch_all(pool)
    .await?;
    rows.iter()
        .map(|row| {
            let monitored: i64 = row.try_get("monitored")?;
            let number: i64 = row.try_get("season_number")?;
            let count: i64 = row.try_get("episode_count")?;
            Ok(Season {
                id: uuid_at(row, "id")?,
                item_id: uuid_at(row, "item_id")?,
                season_number: i32::try_from(number).unwrap_or(0),
                episode_count: i32::try_from(count).unwrap_or(0),
                air_date: opt_ts_at(row, "air_date")?,
                monitored: monitored != 0,
            })
        })
        .collect()
}

/// Episodes of a series, ordered by season then episode.
///
/// # Errors
///
/// Returns [`AppError::Database`] on SQL failure.
pub async fn episodes(pool: &Pool, item_id: Uuid) -> Result<Vec<Episode>, AppError> {
    let rows = sqlx::query(
        "SELECT id, item_id, season_id, season_number, episode_number, title, air_date, monitored, tmdb_episode_id \
         FROM library_episodes WHERE item_id = ? \
         ORDER BY season_number ASC, episode_number ASC",
    )
    .bind(item_id.to_string())
    .fetch_all(pool)
    .await?;
    rows.iter()
        .map(|row| {
            let monitored: i64 = row.try_get("monitored")?;
            let season_number: i64 = row.try_get("season_number")?;
            let episode_number: i64 = row.try_get("episode_number")?;
            Ok(Episode {
                id: uuid_at(row, "id")?,
                tmdb_episode_id: row.try_get("tmdb_episode_id")?,
                item_id: uuid_at(row, "item_id")?,
                season_id: uuid_at(row, "season_id")?,
                season_number: i32::try_from(season_number).unwrap_or(0),
                episode_number: i32::try_from(episode_number).unwrap_or(0),
                title: row.try_get("title")?,
                air_date: opt_ts_at(row, "air_date")?,
                monitored: monitored != 0,
            })
        })
        .collect()
}

/// Flip the monitoring flag of a whole season, cascading to its episodes
/// — "stop following season 1" has to mean its episodes stop too, or the
/// scanner keeps chasing them.
///
/// # Errors
///
/// Returns [`AppError::NotFound`] when the season is absent,
/// [`AppError::Database`] on SQL failure.
pub async fn set_season_monitored(
    pool: &Pool,
    season_id: Uuid,
    monitored: bool,
) -> Result<(), AppError> {
    let res = sqlx::query("UPDATE library_seasons SET monitored = ? WHERE id = ?")
        .bind(i64::from(monitored))
        .bind(season_id.to_string())
        .execute(pool)
        .await?;
    if res.rows_affected() == 0 {
        return Err(AppError::NotFound(format!("library_season {season_id}")));
    }
    sqlx::query("UPDATE library_episodes SET monitored = ? WHERE season_id = ?")
        .bind(i64::from(monitored))
        .bind(season_id.to_string())
        .execute(pool)
        .await?;
    Ok(())
}

/// Flip the monitoring flag of a single episode.
///
/// # Errors
///
/// Returns [`AppError::NotFound`] when the episode is absent,
/// [`AppError::Database`] on SQL failure.
pub async fn set_episode_monitored(
    pool: &Pool,
    episode_id: Uuid,
    monitored: bool,
) -> Result<(), AppError> {
    let res = sqlx::query("UPDATE library_episodes SET monitored = ? WHERE id = ?")
        .bind(i64::from(monitored))
        .bind(episode_id.to_string())
        .execute(pool)
        .await?;
    if res.rows_affected() == 0 {
        return Err(AppError::NotFound(format!("library_episode {episode_id}")));
    }
    Ok(())
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
    use crate::db::seed::{self, Seed};

    /// The IMDb id is derived from the TMDB one rather than fixed:
    /// `library_item_ids` is UNIQUE on `(source, media_type, external_id)`,
    /// and a constant here gave every film in a fixture the same IMDb
    /// identity — two different movies claiming to be The Matrix.
    fn movie(tmdb_id: i64, title: &str) -> NewLibraryItem {
        Seed::movie(tmdb_id, title)
            .imdb(&format!("tt{tmdb_id:07}"))
            .build()
    }

    #[tokio::test]
    async fn upsert_inserts_then_refreshes_metadata() {
        let pool = open_memory().await.unwrap();
        let first = upsert(&pool, &movie(603, "The Matrix")).await.unwrap();
        assert_eq!(first.title, "The Matrix");
        assert!(first.monitored);

        let mut refreshed = movie(603, "Matrix");
        refreshed.overview = Some("Um hacker descobre a verdade.".to_owned());
        let second = upsert(&pool, &refreshed).await.unwrap();

        assert_eq!(second.id, first.id, "same tmdb id must reuse the row");
        assert_eq!(second.title, "Matrix");
        assert_eq!(
            second.overview.as_deref(),
            Some("Um hacker descobre a verdade.")
        );
        assert_eq!(list(&pool).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn metadata_refresh_preserves_operator_state() {
        let pool = open_memory().await.unwrap();
        let item = upsert(&pool, &movie(603, "The Matrix")).await.unwrap();
        set_monitored(&pool, item.id, false).await.unwrap();
        set_placement(&pool, item.id, None, Some("/data/media/filmes"))
            .await
            .unwrap();

        upsert(&pool, &movie(603, "Matrix (1999)")).await.unwrap();

        let after = get_by_id(&pool, item.id).await.unwrap();
        assert_eq!(after.title, "Matrix (1999)", "metadata must refresh");
        assert!(!after.monitored, "monitoring is operator state, not TMDB's");
        assert_eq!(after.root_folder.as_deref(), Some("/data/media/filmes"));
        assert_eq!(after.added_at, item.added_at, "added_at must not move");
    }

    #[tokio::test]
    async fn same_tmdb_id_across_kinds_is_allowed() {
        let pool = open_memory().await.unwrap();
        upsert(&pool, &movie(1399, "A Movie")).await.unwrap();
        let series = Seed::series(1399, "A Series").build();
        upsert(&pool, &series).await.unwrap();
        assert_eq!(list(&pool).await.unwrap().len(), 2);
    }

    #[tokio::test]
    async fn monitored_skips_disabled_entries() {
        let pool = open_memory().await.unwrap();
        let a = upsert(&pool, &movie(1, "A")).await.unwrap();
        upsert(&pool, &movie(2, "B")).await.unwrap();
        set_monitored(&pool, a.id, false).await.unwrap();

        let chased = monitored(&pool).await.unwrap();
        assert_eq!(chased.len(), 1);
        assert_eq!(chased[0].tmdb_id, 2);
    }

    #[tokio::test]
    async fn counts_splits_by_kind_and_monitoring() {
        let pool = open_memory().await.unwrap();
        upsert(&pool, &movie(1, "A")).await.unwrap();
        let b = upsert(&pool, &movie(2, "B")).await.unwrap();
        upsert(&pool, &Seed::series(3, "S").build()).await.unwrap();
        set_monitored(&pool, b.id, false).await.unwrap();

        assert_eq!(
            counts(&pool).await.unwrap(),
            LibraryCounts {
                movies: 1,
                series: 1,
                unmonitored: 1
            }
        );
    }

    fn season(number: i32, episodes: i32) -> NewSeason {
        NewSeason {
            season_number: number,
            episode_count: episodes,
            air_date: None,
            episodes: (1..=episodes)
                .map(|n| NewEpisode {
                    title: Some(format!("E{n}")),
                    ..seed::episode(n)
                })
                .collect(),
        }
    }

    #[tokio::test]
    async fn sync_seasons_builds_the_tree() {
        let pool = open_memory().await.unwrap();
        let item = upsert(&pool, &Seed::series(76479, "The Boys").build())
            .await
            .unwrap();

        sync_seasons(&pool, item.id, &[season(1, 8), season(2, 8)])
            .await
            .unwrap();

        assert_eq!(seasons(&pool, item.id).await.unwrap().len(), 2);
        assert_eq!(episodes(&pool, item.id).await.unwrap().len(), 16);
    }

    #[tokio::test]
    async fn sync_seasons_preserves_monitoring_across_refreshes() {
        let pool = open_memory().await.unwrap();
        let item = upsert(&pool, &Seed::series(76479, "The Boys").build())
            .await
            .unwrap();
        sync_seasons(&pool, item.id, &[season(1, 8)]).await.unwrap();

        let s1 = seasons(&pool, item.id).await.unwrap()[0].clone();
        set_season_monitored(&pool, s1.id, false).await.unwrap();

        // TMDB now reports a second season; the refresh must not
        // re-enable the season the operator switched off.
        sync_seasons(&pool, item.id, &[season(1, 8), season(2, 6)])
            .await
            .unwrap();

        let after = seasons(&pool, item.id).await.unwrap();
        assert_eq!(after.len(), 2);
        assert!(!after[0].monitored, "season 1 stays unmonitored");
        assert!(after[1].monitored, "brand-new season defaults to monitored");

        let eps = episodes(&pool, item.id).await.unwrap();
        assert_eq!(eps.len(), 14);
        assert!(
            eps.iter()
                .filter(|e| e.season_number == 1)
                .all(|e| !e.monitored),
            "season 1 episodes stay unmonitored too"
        );
    }

    #[tokio::test]
    async fn sync_seasons_keeps_the_ids_of_rows_that_survive() {
        // `grabs.episode_id` is the only link between a file on disk and
        // the episode it is, and the FK is `ON DELETE SET NULL`. A
        // refresh that recreates these rows unlinks the whole library —
        // and reads as *complete*, not as missing, because an unlinked
        // grab is `(NULL, NULL)`, the encoding of "covers the item".
        let pool = open_memory().await.unwrap();
        let item = upsert(&pool, &Seed::series(76479, "The Boys").build())
            .await
            .unwrap();
        sync_seasons(&pool, item.id, &[season(1, 8)]).await.unwrap();

        let before: HashMap<(i32, i32), Uuid> = episodes(&pool, item.id)
            .await
            .unwrap()
            .into_iter()
            .map(|e| ((e.season_number, e.episode_number), e.id))
            .collect();
        let season_before = seasons(&pool, item.id).await.unwrap()[0].id;

        // TMDB publishes a second season; season 1 is untouched.
        sync_seasons(&pool, item.id, &[season(1, 8), season(2, 6)])
            .await
            .unwrap();

        assert_eq!(
            seasons(&pool, item.id).await.unwrap()[0].id,
            season_before,
            "the season row must keep its id"
        );
        for e in episodes(&pool, item.id).await.unwrap() {
            if e.season_number != 1 {
                continue;
            }
            assert_eq!(
                before.get(&(e.season_number, e.episode_number)),
                Some(&e.id),
                "S{:02}E{:02} must keep its id across a refresh",
                e.season_number,
                e.episode_number
            );
        }
    }

    /// The whole point of `tmdb_episode_id`: a series can be re-ordered
    /// and every episode keeps its row, so every file keeps its episode.
    /// Dragon Ball Super is 1×131 at TMDB and 14/13/19/30/55 everywhere
    /// else; moving episode 15 from `(1, 15)` to `(2, 1)` used to be a
    /// delete plus an insert, and `grabs.episode_id` is
    /// `ON DELETE SET NULL`.
    #[tokio::test]
    async fn renumbering_a_series_keeps_every_row_and_every_link() {
        let pool = open_memory().await.unwrap();
        let item = upsert(&pool, &Seed::series(62715, "Dragon Ball Super").build())
            .await
            .unwrap();

        // Canonical: one season of 20, each episode carrying its id.
        let flat = vec![NewSeason {
            season_number: 1,
            episode_count: 20,
            air_date: None,
            episodes: (1..=20)
                .map(|n| NewEpisode {
                    tmdb_episode_id: Some(i64::from(1_000_000 + n)),
                    episode_number: n,
                    title: None,
                    air_date: None,
                })
                .collect(),
        }];
        sync_seasons(&pool, item.id, &flat).await.unwrap();

        let before: HashMap<i64, Uuid> = episodes(&pool, item.id)
            .await
            .unwrap()
            .into_iter()
            .filter_map(|e| e.tmdb_episode_id.map(|t| (t, e.id)))
            .collect();
        assert_eq!(before.len(), 20, "the ids reached the database");

        // The same 20 episodes, re-ordered into 14 + 6.
        let split = vec![
            NewSeason {
                season_number: 1,
                episode_count: 14,
                air_date: None,
                episodes: (1..=14)
                    .map(|n| NewEpisode {
                        tmdb_episode_id: Some(i64::from(1_000_000 + n)),
                        episode_number: n,
                        title: None,
                        air_date: None,
                    })
                    .collect(),
            },
            NewSeason {
                season_number: 2,
                episode_count: 6,
                air_date: None,
                episodes: (1..=6)
                    .map(|n| NewEpisode {
                        tmdb_episode_id: Some(i64::from(1_000_000 + 14 + n)),
                        episode_number: n,
                        title: None,
                        air_date: None,
                    })
                    .collect(),
            },
        ];
        sync_seasons(&pool, item.id, &split).await.unwrap();

        let after = episodes(&pool, item.id).await.unwrap();
        assert_eq!(after.len(), 20, "nothing was lost in the re-ordering");
        for e in &after {
            let tmdb = e.tmdb_episode_id.unwrap();
            assert_eq!(
                before.get(&tmdb),
                Some(&e.id),
                "TMDB episode {tmdb} must keep its row across a re-numbering"
            );
        }

        // Canonical 15 is now S02E01, on the same row.
        let moved = after
            .iter()
            .find(|e| e.tmdb_episode_id == Some(1_000_015))
            .unwrap();
        assert_eq!((moved.season_number, moved.episode_number), (2, 1));
        assert_eq!(before[&1_000_015], moved.id);

        // And back again, which is the undo an operator will reach for.
        sync_seasons(&pool, item.id, &flat).await.unwrap();
        let back = episodes(&pool, item.id).await.unwrap();
        assert_eq!(back.len(), 20);
        for e in &back {
            assert_eq!(before.get(&e.tmdb_episode_id.unwrap()), Some(&e.id));
        }
    }

    #[tokio::test]
    async fn sync_seasons_drops_what_tmdb_stopped_reporting() {
        // The upsert must not turn into "append only": a season that
        // vanishes upstream, and an episode count that shrinks, both
        // still have to leave the tree.
        let pool = open_memory().await.unwrap();
        let item = upsert(&pool, &Seed::series(76479, "The Boys").build())
            .await
            .unwrap();
        sync_seasons(&pool, item.id, &[season(1, 8), season(2, 6)])
            .await
            .unwrap();

        sync_seasons(&pool, item.id, &[season(1, 5)]).await.unwrap();

        assert_eq!(seasons(&pool, item.id).await.unwrap().len(), 1);
        let eps = episodes(&pool, item.id).await.unwrap();
        assert_eq!(eps.len(), 5, "the shrunk season keeps only what remains");
        assert!(eps.iter().all(|e| e.season_number == 1));
    }

    #[tokio::test]
    async fn deleting_an_item_cascades_the_tree() {
        let pool = open_memory().await.unwrap();
        let item = upsert(&pool, &Seed::series(76479, "The Boys").build())
            .await
            .unwrap();
        sync_seasons(&pool, item.id, &[season(1, 8)]).await.unwrap();

        delete(&pool, item.id).await.unwrap();

        assert!(seasons(&pool, item.id).await.unwrap().is_empty());
        assert!(episodes(&pool, item.id).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn missing_media_type_is_rejected() {
        let pool = open_memory().await.unwrap();
        let err = upsert(
            &pool,
            &NewLibraryItem {
                tmdb_id: 1,
                title: "X".to_owned(),
                ..NewLibraryItem::default()
            },
        )
        .await
        .unwrap_err();
        assert!(matches!(err, AppError::InvalidInput(_)));
    }

    /// Build a two-season tree, the second season fresh from TMDB.
    fn two_seasons() -> Vec<NewSeason> {
        vec![
            NewSeason {
                season_number: 1,
                episode_count: 2,
                air_date: None,
                episodes: vec![seed::episode(1), seed::episode(2)],
            },
            NewSeason {
                season_number: 2,
                episode_count: 1,
                air_date: None,
                episodes: vec![seed::episode(1)],
            },
        ]
    }

    async fn series(pool: &Pool) -> LibraryItem {
        upsert(pool, &Seed::series(76_479, "The Boys").build())
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn first_season_scope_survives_a_metadata_refresh() {
        // The reason monitor_scope is a column and not just per-season
        // flags: sync_seasons marks every row it has never seen as
        // monitored, so "only the first season" would silently become
        // "all seasons" the day TMDB publishes season 2.
        let pool = crate::db::open_memory().await.unwrap();
        let item = series(&pool).await;
        set_monitor_scope(&pool, item.id, MonitorScope::FirstSeason)
            .await
            .unwrap();

        sync_seasons(&pool, item.id, &two_seasons()).await.unwrap();

        let rows = seasons(&pool, item.id).await.unwrap();
        let s1 = rows.iter().find(|s| s.season_number == 1).unwrap();
        let s2 = rows.iter().find(|s| s.season_number == 2).unwrap();
        assert!(s1.monitored, "the first season is the whole point");
        assert!(
            !s2.monitored,
            "a season the operator never saw must not arrive monitored under this scope"
        );
    }

    #[tokio::test]
    async fn a_refresh_never_overwrites_a_flag_the_operator_set() {
        let pool = crate::db::open_memory().await.unwrap();
        let item = series(&pool).await;
        set_monitor_scope(&pool, item.id, MonitorScope::FirstSeason)
            .await
            .unwrap();
        sync_seasons(&pool, item.id, &two_seasons()).await.unwrap();

        // The operator turns season 2 on by hand.
        let s2 = seasons(&pool, item.id)
            .await
            .unwrap()
            .into_iter()
            .find(|s| s.season_number == 2)
            .unwrap();
        set_season_monitored(&pool, s2.id, true).await.unwrap();

        // A later metadata refresh must not undo that.
        sync_seasons(&pool, item.id, &two_seasons()).await.unwrap();

        let after = seasons(&pool, item.id)
            .await
            .unwrap()
            .into_iter()
            .find(|s| s.season_number == 2)
            .unwrap();
        assert!(
            after.monitored,
            "the scope decides defaults for unseen rows, it does not overwrite choices"
        );
    }

    #[tokio::test]
    async fn the_nothing_scope_unmonitors_the_item_itself() {
        let pool = crate::db::open_memory().await.unwrap();
        let item = series(&pool).await;
        set_monitor_scope(&pool, item.id, MonitorScope::Nothing)
            .await
            .unwrap();
        let after = get_by_id(&pool, item.id).await.unwrap();
        assert!(!after.monitored, "scanner reads library_items.monitored");
        assert_eq!(after.monitor_scope, MonitorScope::Nothing);
    }
}
