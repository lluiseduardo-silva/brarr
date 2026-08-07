//! `library_items` / `library_seasons` / `library_episodes` — brarr's own
//! catalogue of monitored media.
//!
//! See `migrations/20260804120000_library.sql` for schema notes. The
//! split that matters here: **metadata is a cache, monitoring is state**.
//! A TMDB refresh rewrites title/overview/poster/dates but must never
//! touch `monitored`, `profile_id`, `root_folder` or `added_at` — those
//! belong to the operator, not to the upstream API.

use std::collections::HashMap;

use sqlx::{Row, sqlite::SqliteRow};
use time::OffsetDateTime;
use uuid::Uuid;

use crate::{AppError, db::Pool};

/// Movie or series. Persisted as the short label in `media_type`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaType {
    /// A single film.
    Movie,
    /// A series, with seasons and episodes hanging off it.
    Tv,
}

impl MediaType {
    /// Short tag for the `media_type` column.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Movie => "movie",
            Self::Tv => "tv",
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
            "movie" => Ok(Self::Movie),
            "tv" => Ok(Self::Tv),
            other => Err(AppError::InvalidInput(format!(
                "unknown library_items.media_type: {other}"
            ))),
        }
    }
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
        media_type: MediaType::from_label(&media_type_raw)?,
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
    get_by_tmdb(pool, media_type, new.tmdb_id).await
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

/// The per-season and per-episode monitoring flags currently stored, so
/// [`sync_seasons`] can rebuild the tree without losing them.
type MonitoringFlags = (Vec<(i32, bool)>, Vec<(i32, i32, bool)>);

/// Read the flags the operator has already chosen.
///
/// Split out of [`sync_seasons`] to keep that function under the line
/// limit, and because "what has the operator already chosen" is a
/// question worth being able to ask on its own.
async fn existing_monitoring(pool: &Pool, item_id: Uuid) -> Result<MonitoringFlags, AppError> {
    let existing =
        sqlx::query("SELECT season_number, monitored FROM library_seasons WHERE item_id = ?")
            .bind(item_id.to_string())
            .fetch_all(pool)
            .await?;
    let mut season_flags: Vec<(i32, bool)> = Vec::with_capacity(existing.len());
    for row in &existing {
        let number: i64 = row.try_get("season_number")?;
        let flag: i64 = row.try_get("monitored")?;
        season_flags.push((i32::try_from(number).unwrap_or(0), flag != 0));
    }

    let existing_eps = sqlx::query(
        "SELECT season_number, episode_number, monitored FROM library_episodes WHERE item_id = ?",
    )
    .bind(item_id.to_string())
    .fetch_all(pool)
    .await?;
    let mut episode_flags: Vec<(i32, i32, bool)> = Vec::with_capacity(existing_eps.len());
    for row in &existing_eps {
        let s: i64 = row.try_get("season_number")?;
        let e: i64 = row.try_get("episode_number")?;
        let flag: i64 = row.try_get("monitored")?;
        episode_flags.push((
            i32::try_from(s).unwrap_or(0),
            i32::try_from(e).unwrap_or(0),
            flag != 0,
        ));
    }
    Ok((season_flags, episode_flags))
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

/// Replace the season/episode tree of a series from a TMDB fetch.
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
/// # Errors
///
/// Returns [`AppError::Database`] on SQL failure.
pub async fn sync_seasons(
    pool: &Pool,
    item_id: Uuid,
    seasons: &[NewSeason],
) -> Result<(), AppError> {
    let (season_flags, episode_flags) = existing_monitoring(pool, item_id).await?;

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

    // Rebuild from scratch: cheaper and far less error-prone than
    // diffing, and the tree is small (tens of rows per series).
    sqlx::query("DELETE FROM library_seasons WHERE item_id = ?")
        .bind(item_id.to_string())
        .execute(pool)
        .await?;

    for season in seasons {
        let season_id = Uuid::new_v4();
        let season_aired = season.air_date.is_some_and(|d| d <= now);
        let monitored = season_flags
            .iter()
            .find(|(n, _)| *n == season.season_number)
            .map_or_else(
                || scope.wants_new_row(season.season_number, first_season, season_aired),
                |(_, flag)| *flag,
            );
        sqlx::query(
            "INSERT INTO library_seasons \
                (id, item_id, season_number, episode_count, air_date, monitored) \
             VALUES (?, ?, ?, ?, ?, ?)",
        )
        .bind(season_id.to_string())
        .bind(item_id.to_string())
        .bind(i64::from(season.season_number))
        .bind(i64::from(season.episode_count))
        .bind(season.air_date.map(OffsetDateTime::unix_timestamp))
        .bind(i64::from(monitored))
        .execute(pool)
        .await?;

        for episode in &season.episodes {
            // No air date counts as "not aired": TMDB leaves it blank
            // until it schedules one, and calling that aired would mark
            // the episode unmonitored under `future`, where it would
            // stay forever — the tree preserves flags by number, so it
            // could never come back on its own.
            let aired = episode.air_date.is_some_and(|d| d <= now);
            let ep_monitored = episode_flags
                .iter()
                .find(|(s, e, _)| *s == season.season_number && *e == episode.episode_number)
                .map_or_else(
                    || scope.wants_new_row(season.season_number, first_season, aired),
                    |(_, _, flag)| *flag,
                );
            sqlx::query(
                "INSERT INTO library_episodes \
                    (id, item_id, season_id, season_number, episode_number, title, air_date, monitored) \
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
            )
            .bind(Uuid::new_v4().to_string())
            .bind(item_id.to_string())
            .bind(season_id.to_string())
            .bind(i64::from(season.season_number))
            .bind(i64::from(episode.episode_number))
            .bind(episode.title.as_deref())
            .bind(episode.air_date.map(OffsetDateTime::unix_timestamp))
            .bind(i64::from(ep_monitored))
            .execute(pool)
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
        "SELECT id, item_id, season_id, season_number, episode_number, title, air_date, monitored \
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

    fn movie(tmdb_id: i64, title: &str) -> NewLibraryItem {
        NewLibraryItem {
            media_type: Some(MediaType::Movie),
            tmdb_id,
            title: title.to_owned(),
            imdb_id: Some("tt0133093".to_owned()),
            ..NewLibraryItem::default()
        }
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
        let series = NewLibraryItem {
            media_type: Some(MediaType::Tv),
            tmdb_id: 1399,
            title: "A Series".to_owned(),
            ..NewLibraryItem::default()
        };
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
        upsert(
            &pool,
            &NewLibraryItem {
                media_type: Some(MediaType::Tv),
                tmdb_id: 3,
                title: "S".to_owned(),
                ..NewLibraryItem::default()
            },
        )
        .await
        .unwrap();
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
                    episode_number: n,
                    title: Some(format!("E{n}")),
                    air_date: None,
                })
                .collect(),
        }
    }

    #[tokio::test]
    async fn sync_seasons_builds_the_tree() {
        let pool = open_memory().await.unwrap();
        let item = upsert(
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

        sync_seasons(&pool, item.id, &[season(1, 8), season(2, 8)])
            .await
            .unwrap();

        assert_eq!(seasons(&pool, item.id).await.unwrap().len(), 2);
        assert_eq!(episodes(&pool, item.id).await.unwrap().len(), 16);
    }

    #[tokio::test]
    async fn sync_seasons_preserves_monitoring_across_refreshes() {
        let pool = open_memory().await.unwrap();
        let item = upsert(
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
    async fn deleting_an_item_cascades_the_tree() {
        let pool = open_memory().await.unwrap();
        let item = upsert(
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
            },
            NewSeason {
                season_number: 2,
                episode_count: 1,
                air_date: None,
                episodes: vec![NewEpisode {
                    episode_number: 1,
                    title: None,
                    air_date: None,
                }],
            },
        ]
    }

    async fn series(pool: &Pool) -> LibraryItem {
        upsert(
            pool,
            &NewLibraryItem {
                media_type: Some(MediaType::Tv),
                tmdb_id: 76_479,
                title: "The Boys".to_owned(),
                ..NewLibraryItem::default()
            },
        )
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
