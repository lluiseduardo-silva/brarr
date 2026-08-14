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

use brarr_core::{Description, ExternalId, MetadataSource, Ordering, OrderingFamily};

use crate::{AppError, db::Pool};

/// Movie or series. Persisted as the short label in `media_type`.
///
/// **Declared in `brarr-core` and re-exported here**, so the metadata
/// provider trait can dispatch on media kind without depending on this
/// module — a leaf crate cannot import the orchestrator's data layer, and
/// two enums with the same two variants would need converting at every
/// boundary. Call sites keep saying `library::MediaType`.
pub use brarr_core::MediaType;

/// Read a `*_source` column.
///
/// An unregistered label reads as `None` rather than failing the row: a
/// source this build does not know is a reason to render no image, never
/// a reason to make the title unreadable.
fn source_of(row: &SqliteRow, column: &str) -> Result<Option<MetadataSource>, AppError> {
    let raw: Option<String> = row.try_get(column)?;
    Ok(raw.as_deref().and_then(MetadataSource::parse))
}

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

/// Where a work stands, in brarr's own words.
///
/// Re-exported from `brarr-core` rather than declared here: the trait
/// returns it, so a copy in the data layer would be two closed sets that
/// have to agree — and a value valid in one and unknown in the other is
/// precisely the shape of defect this whole block has been closing.
pub use brarr_core::ProductionStatus;

/// One catalogue entry.
#[derive(Debug, Clone)]
pub struct LibraryItem {
    /// Stable UUID v4.
    pub id: Uuid,
    /// Movie or series.
    pub media_type: MediaType,
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
    /// Who stored [`Self::poster_path`], and therefore how to read it.
    ///
    /// Not an owner and not a precedence: TMDB keeps a path relative to
    /// its CDN and TheTVDB returns an absolute URL, so the value cannot
    /// be turned into a URL without knowing who wrote it. `None` for a
    /// row written before the column existed, which renders no image
    /// rather than a broken one.
    pub poster_source: Option<MetadataSource>,
    /// Who stored [`Self::backdrop_path`].
    pub backdrop_source: Option<MetadataSource>,
    /// Who owns title, synopsis and artwork for this title.
    ///
    /// `None` reads as TMDB — the provider that describes both media
    /// kinds, and what every row was before the operator could choose.
    /// Unlike the structure owner this may be changed at any time and by
    /// policy: rewriting a synopsis is cheap and reversible.
    pub descriptive_source: Option<MetadataSource>,
    /// Where the work stands, in brarr's own words.
    pub status: Option<ProductionStatus>,
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
    /// Every catalogue this title is known by.
    ///
    /// A set rather than three named columns, which is what makes a
    /// title only TheTVDB knows representable at all — and what stops
    /// adding one through TMDB and meeting it again on the \*arr's TVDB
    /// axis from producing two rows for one series.
    ///
    /// **Empty is refused** by [`upsert`]: a catalogue row nothing can
    /// name is one nothing can find again, so it would be created afresh
    /// on every sweep.
    pub ids: Vec<ExternalId>,
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
    /// Where the work stands, mapped into brarr's vocabulary by
    /// whoever read the provider.
    pub status: Option<ProductionStatus>,
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
    /// Who numbered this row.
    ///
    /// `None` for every row written before the tree writer learned to
    /// stamp it, which is most of them until a refresh has been round.
    /// Read together with [`Self::external_id`]: the pair is what a
    /// UNIQUE index can hold once two providers can each own a tree,
    /// because TheTVDB's episode 5345648 and TMDB's episode 5345648 are
    /// not the same row.
    pub source: Option<MetadataSource>,
    /// The owning source's own episode id, as text.
    ///
    /// The only identity that survives a renumbering — neither the local
    /// UUID nor the `(season, episode)` pair does.
    pub external_id: Option<String>,
    /// Position in the series as a whole, when the source has one.
    ///
    /// **Evidence and a tiebreak, never a coordinate.** TheTVDB gives
    /// absolute 13 to a Kaiju No. 8 special, so its `S02E01` carries
    /// absolute 14 and an absolute-first join moves a whole season by
    /// one.
    pub absolute_number: Option<i32>,
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

/// Read a nullable INTEGER that Rust holds as an `i32`.
///
/// A stored value outside `i32` is bad data and degrades to `None`, the
/// same call as [`opt_ts_at`]: an absolute number nobody can represent is
/// a reason to have no tiebreak, never a reason to make the episode
/// unreadable.
fn opt_i32_at(row: &SqliteRow, col: &str) -> Result<Option<i32>, AppError> {
    let raw: Option<i64> = row.try_get(col)?;
    Ok(raw.and_then(|v| i32::try_from(v).ok()))
}

fn row_to_item(row: &SqliteRow) -> Result<LibraryItem, AppError> {
    let media_type_raw: String = row.try_get("media_type")?;
    let monitored: i64 = row.try_get("monitored")?;
    let scope_raw: String = row.try_get("monitor_scope")?;
    Ok(LibraryItem {
        id: uuid_at(row, "id")?,
        media_type: media_type_from_label(&media_type_raw)?,
        title: row.try_get("title")?,
        original_title: row.try_get("original_title")?,
        year: row
            .try_get::<Option<i64>, _>("year")?
            .and_then(|v| i32::try_from(v).ok()),
        overview: row.try_get("overview")?,
        poster_path: row.try_get("poster_path")?,
        backdrop_path: row.try_get("backdrop_path")?,
        poster_source: source_of(row, "poster_source")?,
        backdrop_source: source_of(row, "backdrop_source")?,
        descriptive_source: source_of(row, "descriptive_source")?,
        // An unreadable value degrades to `None` rather than failing the
        // row: only a hand-edited database can produce one, and refusing
        // to render a catalogue over it would be the larger harm.
        status: row
            .try_get::<Option<String>, _>("status")?
            .as_deref()
            .and_then(ProductionStatus::parse),
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

const ITEM_COLUMNS: &str = "id, media_type, title, original_title, \
     year, overview, poster_path, backdrop_path, poster_source, backdrop_source,      descriptive_source, \
     status, runtime_minutes, \
     next_air_date, digital_release_at, physical_release_at, monitored, \
     profile_id, root_folder, monitor_scope, added_at, metadata_refreshed_at";

/// Insert a catalogue entry, or refresh the metadata of the one any of
/// `new.ids` already names.
///
/// Only provider-owned columns are refreshed. `monitored`, `profile_id`,
/// `root_folder`, `monitor_scope` and `added_at` are left untouched, so a
/// metadata sweep can run over the whole library without resetting what
/// the operator configured — the rule `db::library` has had since it
/// existed, restated here because the mechanism changed underneath it.
///
/// ## Why this is a transaction, and why it did not need to be before
///
/// Identity used to be `ON CONFLICT(media_type, tmdb_id)`: one statement,
/// and the database refused a second row for the same title on its own.
/// Identity is now a set in `library_item_ids`, so the sequence is
/// *look up, then insert or update, then record the ids* — three
/// statements, and between the first and the third a concurrent caller
/// can look up the same absent title and also decide to insert.
///
/// The unique index on `(source, media_type, external_id)` is what stops
/// two rows existing, but it fires on the **third** statement, and a
/// non-transactional loser would leave behind a `library_items` row that
/// no id names: invisible to `get_by_external`, so created afresh on
/// every sweep, and carrying whatever the operator later set on it.
/// Inside a transaction that violation rolls the insert back with it.
///
/// This is also why `write_identity` stopped being deliberately
/// non-transactional. That was correct while nothing read the table —
/// a failed mirror must not roll back a catalogue write — and is exactly
/// wrong now that the mirror *is* the identity.
///
/// # Errors
///
/// - [`AppError::InvalidInput`] when `media_type` is absent, or when
///   `ids` is empty: a row nothing can name is one nothing can find
///   again, so it would be created afresh on every sweep.
/// - [`AppError::Database`] on SQL failure, including the unique index
///   when a concurrent caller won the race.
pub async fn upsert(pool: &Pool, new: &NewLibraryItem) -> Result<LibraryItem, AppError> {
    let media_type = new
        .media_type
        .ok_or_else(|| AppError::InvalidInput("library item needs a media_type".to_owned()))?;
    if new.ids.is_empty() {
        return Err(AppError::InvalidInput(
            "library item needs at least one external id".to_owned(),
        ));
    }

    let mut tx = pool.begin().await?;
    let now = OffsetDateTime::now_utc().unix_timestamp();

    let id = match find_by_any_id(&mut tx, media_type, &new.ids).await? {
        Some(existing) => {
            refresh_metadata(&mut tx, existing, new, now).await?;
            existing
        }
        None => insert_row(&mut tx, media_type, new, now).await?,
    };
    record_ids(&mut tx, id, media_type, &new.ids).await?;

    tx.commit().await?;
    get_by_id(pool, id).await
}

/// The item any of `ids` already names.
///
/// **Any** of them, which is the whole point of identity being a set: a
/// series added through TMDB and met again on the \*arr's TVDB axis is
/// one row, not two.
async fn find_by_any_id(
    tx: &mut sqlx::SqliteConnection,
    media_type: MediaType,
    ids: &[ExternalId],
) -> Result<Option<Uuid>, AppError> {
    for id in ids {
        let row = sqlx::query(
            "SELECT item_id FROM library_item_ids \
             WHERE source = ? AND media_type = ? AND external_id = ?",
        )
        .bind(id.source().label())
        .bind(media_type.label())
        .bind(id.value())
        .fetch_optional(&mut *tx)
        .await?;
        if let Some(row) = row {
            let raw: String = row.try_get("item_id")?;
            if let Ok(parsed) = Uuid::parse_str(&raw) {
                return Ok(Some(parsed));
            }
        }
    }
    Ok(None)
}

/// Overwrite the provider-owned columns of a row that already exists.
///
/// The list is the contract: `monitored`, `profile_id`, `root_folder`,
/// `monitor_scope` and `added_at` are absent from it, so a metadata sweep
/// over the whole library resets nothing the operator configured.
async fn refresh_metadata(
    tx: &mut sqlx::SqliteConnection,
    id: Uuid,
    new: &NewLibraryItem,
    now: i64,
) -> Result<(), AppError> {
    sqlx::query(
        "UPDATE library_items SET \
            title = ?, original_title = ?, year = ?, overview = ?, \
            poster_path = ?, backdrop_path = ?, status = ?, runtime_minutes = ?, \
            next_air_date = ?, digital_release_at = ?, physical_release_at = ?, \
            metadata_refreshed_at = ? \
         WHERE id = ?",
    )
    .bind(&new.title)
    .bind(new.original_title.as_deref())
    .bind(new.year.map(i64::from))
    .bind(new.overview.as_deref())
    .bind(new.poster_path.as_deref())
    .bind(new.backdrop_path.as_deref())
    .bind(new.status.map(ProductionStatus::label))
    .bind(new.runtime_minutes.map(i64::from))
    .bind(new.next_air_date.map(OffsetDateTime::unix_timestamp))
    .bind(new.digital_release_at.map(OffsetDateTime::unix_timestamp))
    .bind(new.physical_release_at.map(OffsetDateTime::unix_timestamp))
    .bind(now)
    .bind(id.to_string())
    .execute(&mut *tx)
    .await?;
    Ok(())
}

/// Create the row, monitored, and hand back its fresh id.
async fn insert_row(
    tx: &mut sqlx::SqliteConnection,
    media_type: MediaType,
    new: &NewLibraryItem,
    now: i64,
) -> Result<Uuid, AppError> {
    let id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO library_items ( \
            id, media_type, title, original_title, year, \
            overview, poster_path, backdrop_path, status, runtime_minutes, \
            next_air_date, digital_release_at, physical_release_at, \
            monitored, added_at, metadata_refreshed_at \
         ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, 1, ?, ?)",
    )
    .bind(id.to_string())
    .bind(media_type.label())
    .bind(&new.title)
    .bind(new.original_title.as_deref())
    .bind(new.year.map(i64::from))
    .bind(new.overview.as_deref())
    .bind(new.poster_path.as_deref())
    .bind(new.backdrop_path.as_deref())
    .bind(new.status.map(ProductionStatus::label))
    .bind(new.runtime_minutes.map(i64::from))
    .bind(new.next_air_date.map(OffsetDateTime::unix_timestamp))
    .bind(new.digital_release_at.map(OffsetDateTime::unix_timestamp))
    .bind(new.physical_release_at.map(OffsetDateTime::unix_timestamp))
    .bind(now)
    .bind(now)
    .execute(&mut *tx)
    .await?;
    Ok(id)
}

/// Record every id against the row.
///
/// `ON CONFLICT(item_id, source)` handles the same source reporting a new
/// value. The **other** unique index — `(source, media_type, external_id)`
/// — is deliberately not handled: a violation there means another row
/// already carries this id, which inside the caller's transaction rolls
/// the insert back with it. That is what stops the loser of a race
/// leaving behind a `library_items` row no id names, invisible to
/// [`get_by_external`] and therefore recreated on every sweep.
async fn record_ids(
    tx: &mut sqlx::SqliteConnection,
    id: Uuid,
    media_type: MediaType,
    ids: &[ExternalId],
) -> Result<(), AppError> {
    for external in ids {
        // Asserted, never vouched: the caller read these off a payload,
        // no provider was asked to confirm the pairing, and claiming
        // otherwise would stop a cross-resolution from ever checking one
        // nobody checked.
        sqlx::query(
            "INSERT INTO library_item_ids (item_id, source, external_id, media_type) \
             VALUES (?, ?, ?, ?) \
             ON CONFLICT(item_id, source) DO UPDATE SET \
                external_id = excluded.external_id, \
                media_type  = excluded.media_type",
        )
        .bind(id.to_string())
        .bind(external.source().label())
        .bind(external.value())
        .bind(media_type.label())
        .execute(&mut *tx)
        .await?;
    }
    Ok(())
}

/// Apply a description from whoever owns the descriptive facet.
///
/// **Only the fields the owner actually has.** A provider that has no
/// poster must not blank the one already stored: the plan's rule for
/// artwork is "prefer the owner's, and in its absence accept any source
/// that has one", so a TheTVDB-owned title with no image keeps TMDB's
/// and keeps `poster_source` pointing at TMDB — which is what makes the
/// URL still resolvable.
///
/// The identity columns are untouched, and so is everything the operator
/// set. This is the facet writer, not an upsert.
///
/// # Errors
///
/// Returns [`AppError::Database`] on SQL failure.
pub async fn apply_description(
    pool: &Pool,
    item_id: Uuid,
    description: &Description,
) -> Result<(), AppError> {
    let now = OffsetDateTime::now_utc().unix_timestamp();
    sqlx::query(
        "UPDATE library_items SET             title = ?, original_title = ?, year = ?, overview = ?,             status = ?, runtime_minutes = ?, next_air_date = ?,             digital_release_at = ?, physical_release_at = ?,             poster_path   = COALESCE(?, poster_path),             poster_source = COALESCE(?, poster_source),             backdrop_path   = COALESCE(?, backdrop_path),             backdrop_source = COALESCE(?, backdrop_source),             descriptive_source = ?,             descriptive_refreshed_at = ?,             metadata_refreshed_at = ?          WHERE id = ?",
    )
    .bind(&description.title)
    .bind(description.original_title.as_deref())
    .bind(description.year.map(i64::from))
    .bind(description.overview.as_deref())
    .bind(description.status.map(ProductionStatus::label))
    .bind(description.runtime_minutes.map(i64::from))
    .bind(description.next_air_date.map(OffsetDateTime::unix_timestamp))
    .bind(description.digital_release_at.map(OffsetDateTime::unix_timestamp))
    .bind(
        description
            .physical_release_at
            .map(OffsetDateTime::unix_timestamp),
    )
    .bind(description.poster.as_ref().map(|a| a.value.as_str()))
    .bind(description.poster.as_ref().map(|a| a.source.label()))
    .bind(description.backdrop.as_ref().map(|a| a.value.as_str()))
    .bind(description.backdrop.as_ref().map(|a| a.source.label()))
    .bind(description.source.label())
    .bind(now)
    .bind(now)
    .bind(item_id.to_string())
    .execute(pool)
    .await?;
    Ok(())
}

/// Record who describes a title, without fetching anything.
///
/// The operator's choice, which the next refresh reads.
///
/// # Errors
///
/// Returns [`AppError::Database`] on SQL failure.
pub async fn set_descriptive_source(
    pool: &Pool,
    item_id: Uuid,
    source: MetadataSource,
) -> Result<(), AppError> {
    sqlx::query("UPDATE library_items SET descriptive_source = ? WHERE id = ?")
        .bind(source.label())
        .bind(item_id.to_string())
        .execute(pool)
        .await?;
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

/// The entry a given external id names, whichever source issued it.
///
/// **This is what "already in the library?" is.** A lookup on one axis
/// could not answer it: a series added through TMDB and met again on the
/// \*arr's TVDB axis read as absent and was catalogued a second time.
/// Asking by any known id makes the two one row.
///
/// The join goes through `library_item_ids`, so a title carries as many
/// answers as it has ids and every one of them finds it.
///
/// # Errors
///
/// Returns [`AppError::NotFound`] when no item carries that id, and
/// [`AppError::Database`] on SQL failure.
pub async fn get_by_external(
    pool: &Pool,
    media_type: MediaType,
    id: &ExternalId,
) -> Result<LibraryItem, AppError> {
    let row = sqlx::query(&format!(
        "SELECT {} FROM library_items i \
         JOIN library_item_ids x ON x.item_id = i.id \
         WHERE x.source = ? AND x.media_type = ? AND x.external_id = ?",
        ITEM_COLUMNS
            .split(", ")
            .map(|c| format!("i.{c}"))
            .collect::<Vec<_>>()
            .join(", ")
    ))
    .bind(id.source().label())
    .bind(media_type.label())
    .bind(id.value())
    .fetch_optional(pool)
    .await?;
    match row {
        Some(r) => row_to_item(&r),
        None => Err(AppError::NotFound(format!(
            "library_item {}/{id}",
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
#[cfg(test)]
#[derive(Debug, Clone, Copy)]
struct StoredSeason {
    id: Uuid,
    monitored: bool,
}

/// An episode row as it stands before a sync.
#[cfg(test)]
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
#[cfg(test)]
struct StoredTree {
    /// By season number.
    seasons: HashMap<i32, StoredSeason>,
    /// By `(season_number, episode_number)`.
    episodes: HashMap<(i32, i32), StoredEpisode>,
}

#[cfg(test)]
impl StoredTree {
    /// The row this payload episode belongs to, if the tree already has
    /// it.
    ///
    /// Coordinates only, which is all this door can offer: `NewSeason`
    /// carries no identity. Production tree writes go through
    /// [`crate::structure::apply`], which pairs on the owning source's
    /// own episode id and reaches a coordinate only when there is none.
    fn resolve(&self, season: i32, number: i32) -> Option<StoredEpisode> {
        self.episodes.get(&(season, number)).copied()
    }
}

/// Read the tree the operator already has, ids included.
///
/// Split out of [`sync_seasons`] to keep that function under the line
/// limit, and because "what is stored right now" is a question worth
/// being able to ask on its own.
#[cfg(test)]
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
        "SELECT id, season_number, episode_number, monitored \
         FROM library_episodes WHERE item_id = ?",
    )
    .bind(item_id.to_string())
    .fetch_all(pool)
    .await?;
    let mut episodes = HashMap::with_capacity(existing_eps.len());
    for row in &existing_eps {
        let s: i64 = row.try_get("season_number")?;
        let e: i64 = row.try_get("episode_number")?;
        let flag: i64 = row.try_get("monitored")?;
        episodes.insert(
            (i32::try_from(s).unwrap_or(0), i32::try_from(e).unwrap_or(0)),
            StoredEpisode {
                id: uuid_at(row, "id")?,
                monitored: flag != 0,
            },
        );
    }
    Ok(StoredTree { seasons, episodes })
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

/// Seed a tree from a `NewSeason` payload. **Tests only.**
///
/// Production writes go through [`crate::structure::apply`], which asks
/// who owns the shape before rewriting it and computes the pairing with
/// evidence it can report. This door exists because a hundred-odd unit
/// tests want a tree in the database and do not want to state an identity
/// for every episode to get one; it resolves rows the way the writer did
/// before [`crate::structure::pair`] existed — the owning id, then the
/// `(season, episode)` pair — and stamps no identity, so it cannot
/// pretend a source produced rows no source did.
///
/// It is `#[cfg(test)]` rather than merely private because "no production
/// caller" is the property this phase bought, and a compiler error is a
/// better way to keep it than a review.
///
/// # Errors
///
/// Returns [`AppError::Database`] on SQL failure, or
/// [`AppError::InvalidInput`] when the write would orphan an acquisition.
#[cfg(test)]
pub(crate) async fn sync_seasons(
    pool: &Pool,
    item_id: Uuid,
    seasons: &[NewSeason],
) -> Result<(), AppError> {
    let stored = existing_tree(pool, item_id).await?;
    let policy = FlagPolicy::read(pool, item_id, seasons.iter().map(|s| s.season_number)).await?;

    let mut decided = Vec::with_capacity(seasons.len());
    for season in seasons {
        let known = stored.seasons.get(&season.season_number);
        let episodes = season
            .episodes
            .iter()
            .map(|episode| {
                let known = stored.resolve(season.season_number, episode.episode_number);
                DecidedEpisode {
                    id: known.map_or_else(Uuid::new_v4, |e| e.id),
                    number: episode.episode_number,
                    title: episode.title.clone(),
                    air_date: episode.air_date,
                    // The compatibility door writes no identity: it is
                    // fed `NewSeason`, which has nowhere to carry one.
                    // Only `structure::apply` stamps the neutral columns.
                    source: None,
                    external_id: None,
                    absolute_number: None,
                    monitored: policy.for_row(
                        season.season_number,
                        episode.air_date,
                        known.map(|e| e.monitored),
                    ),
                }
            })
            .collect();

        decided.push(DecidedSeason {
            id: known.map_or_else(Uuid::new_v4, |s| s.id),
            number: season.season_number,
            episode_count: season.episode_count,
            air_date: season.air_date,
            monitored: policy.for_row(
                season.season_number,
                season.air_date,
                known.map(|s| s.monitored),
            ),
            episodes,
        });
    }

    write_tree(pool, item_id, &decided).await
}

/// One season as the writer will store it, with its row id already
/// decided.
///
/// The type exists so that *deciding* which stored row an incoming
/// episode is — the part [`crate::structure::pair`] owns and the part
/// that can be got wrong — happens before the transaction opens and can
/// be inspected without writing anything.
#[derive(Debug, Clone)]
pub(crate) struct DecidedSeason {
    /// Row id to write under: a stored one when the season survives.
    pub id: Uuid,
    /// Season number.
    pub number: i32,
    /// Episode count to record.
    pub episode_count: i32,
    /// First air date.
    pub air_date: Option<OffsetDateTime>,
    /// Resolved monitoring flag.
    pub monitored: bool,
    /// Episodes belonging to it.
    pub episodes: Vec<DecidedEpisode>,
}

/// One episode as the writer will store it.
#[derive(Debug, Clone)]
pub(crate) struct DecidedEpisode {
    /// Row id to write under: a stored one when the episode survives.
    pub id: Uuid,
    /// Episode number within the season.
    pub number: i32,
    /// Episode title.
    pub title: Option<String>,
    /// Air date.
    pub air_date: Option<OffsetDateTime>,
    /// TMDB's own episode id, where the payload carried one.
    /// Who numbered this row.
    pub source: Option<MetadataSource>,
    /// The owning source's episode id, as text.
    pub external_id: Option<String>,
    /// Position in the series as a whole.
    pub absolute_number: Option<i32>,
    /// Resolved monitoring flag.
    pub monitored: bool,
}

/// How a row this writer has never seen gets its monitoring flag.
///
/// Rows it *has* seen keep whatever the operator set: the scope decides
/// defaults, it never overwrites a choice.
pub(crate) struct FlagPolicy {
    scope: MonitorScope,
    first_season: i32,
    now: OffsetDateTime,
}

impl FlagPolicy {
    /// Read the item's scope and fix the reference points.
    ///
    /// # Errors
    ///
    /// Returns [`AppError::Database`] on SQL failure.
    pub(crate) async fn read(
        pool: &Pool,
        item_id: Uuid,
        seasons: impl Iterator<Item = i32>,
    ) -> Result<Self, AppError> {
        Ok(Self {
            scope: item_scope(pool, item_id).await?,
            // Season 0 is TMDB's specials bucket and never counts as
            // "the first season" — The Boys carries 76 specials against
            // 40 real episodes.
            first_season: seasons.filter(|n| *n > 0).min().unwrap_or(1),
            now: OffsetDateTime::now_utc(),
        })
    }

    /// The flag for one row. `known` is its stored flag, when it has one.
    pub(crate) fn for_row(
        &self,
        season: i32,
        air_date: Option<OffsetDateTime>,
        known: Option<bool>,
    ) -> bool {
        // No air date counts as "not aired": TMDB leaves it blank until
        // it schedules one, and calling that aired would mark the episode
        // unmonitored under `future`, where it would stay forever — the
        // tree preserves flags by number, so it could never come back on
        // its own.
        let aired = air_date.is_some_and(|d| d <= self.now);
        known.unwrap_or_else(|| self.scope.wants_new_row(season, self.first_season, aired))
    }
}

/// Write a decided tree, and refuse to leave a file unlinked.
///
/// **The one place in brarr that rebuilds a season tree.** Everything
/// else reaches it through [`crate::structure::apply`], which is what
/// makes "who owns this shape?" a question that gets asked before the
/// shape is rewritten rather than after.
///
/// One transaction: a failure halfway used to leave the item with a
/// partially rebuilt tree, because the `DELETE` had already committed.
///
/// **The net.** `grabs.episode_id` is `ON DELETE SET NULL`, so every
/// prune silently unlinks whatever pointed at the row — no error, no log,
/// and the resulting `(scope='episode', episode_id=NULL)` covers nothing
/// while the screen used to render the series *complete*. So the count of
/// this item's orphaned episode grabs is taken before and after, inside
/// the same transaction, and a rise rolls the whole thing back. It is one
/// query, and it is the entire safety net, because the damage it guards
/// has no other symptom.
///
/// # Errors
///
/// Returns [`AppError::Database`] on SQL failure, or
/// [`AppError::InvalidInput`] when the write would orphan an acquisition.
pub(crate) async fn write_tree(
    pool: &Pool,
    item_id: Uuid,
    seasons: &[DecidedSeason],
) -> Result<(), AppError> {
    let mut tx = pool.begin().await?;

    // Every read from here on goes through `&mut *tx`, never the pool:
    // `open_memory` runs with `max_connections(1)`, so a pool query
    // issued while this transaction is checked out waits for the
    // connection the transaction itself holds, and the test hangs
    // instead of failing.
    let before = crate::db::grabs::orphan_episode_count(&mut tx, item_id).await?;
    let stored_ids = tree_row_ids(&mut tx, item_id).await?;

    park(&mut tx, item_id).await?;

    let mut claimed: HashSet<Uuid> = HashSet::new();
    for season in seasons {
        claimed.insert(season.id);
        upsert_season_row(&mut tx, item_id, season).await?;
        for episode in &season.episodes {
            let ids = EpisodeIds {
                id: episode.id,
                item_id,
                season_id: season.id,
            };
            claimed.insert(episode.id);
            upsert_episode_row(&mut tx, ids, season.number, episode).await?;
        }
    }

    prune_rows(&mut tx, &stored_ids, &claimed).await?;

    let after = crate::db::grabs::orphan_episode_count(&mut tx, item_id).await?;
    if after > before {
        // Dropping the transaction rolls it back. Returning the numbers
        // rather than a bare refusal is what makes the failure legible in
        // a log line that nobody was watching for.
        return Err(AppError::InvalidInput(format!(
            "recusado: a escrita da árvore deixaria {} aquisição(ões) sem episódio (antes {before}, depois {after})",
            after - before
        )));
    }

    tx.commit().await?;
    Ok(())
}

/// Every season and episode row id this item currently has.
///
/// Read inside the transaction, unlike the snapshot [`sync_seasons`]
/// diffs against: it decides what gets deleted, so it must see the same
/// state the deletes will run against.
async fn tree_row_ids(
    conn: &mut sqlx::SqliteConnection,
    item_id: Uuid,
) -> Result<Vec<(Uuid, bool)>, AppError> {
    let mut out = Vec::new();
    let seasons = sqlx::query("SELECT id FROM library_seasons WHERE item_id = ?")
        .bind(item_id.to_string())
        .fetch_all(&mut *conn)
        .await?;
    for row in &seasons {
        out.push((uuid_at(row, "id")?, false));
    }
    let episodes = sqlx::query("SELECT id FROM library_episodes WHERE item_id = ?")
        .bind(item_id.to_string())
        .fetch_all(&mut *conn)
        .await?;
    for row in &episodes {
        out.push((uuid_at(row, "id")?, true));
    }
    Ok(out)
}

/// Drop the rows the payload did not claim.
///
/// Episodes go first so the reason each row died is the one stated here,
/// not an incidental CASCADE from its season.
async fn prune_rows(
    conn: &mut sqlx::SqliteConnection,
    stored: &[(Uuid, bool)],
    claimed: &HashSet<Uuid>,
) -> Result<(), AppError> {
    let doomed = || stored.iter().filter(|(id, _)| !claimed.contains(id));
    for (id, _) in doomed().filter(|(_, is_episode)| *is_episode) {
        sqlx::query("DELETE FROM library_episodes WHERE id = ?")
            .bind(id.to_string())
            .execute(&mut *conn)
            .await?;
    }
    for (id, _) in doomed().filter(|(_, is_episode)| !*is_episode) {
        sqlx::query("DELETE FROM library_seasons WHERE id = ?")
            .bind(id.to_string())
            .execute(&mut *conn)
            .await?;
    }
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
async fn upsert_season_row(
    conn: &mut sqlx::SqliteConnection,
    item_id: Uuid,
    season: &DecidedSeason,
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
    .bind(season.id.to_string())
    .bind(item_id.to_string())
    .bind(i64::from(season.number))
    .bind(i64::from(season.episode_count))
    .bind(season.air_date.map(OffsetDateTime::unix_timestamp))
    .bind(i64::from(season.monitored))
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
/// `COALESCE` on every identity column, never a bare `excluded`.
///
/// An id can be set but not cleared. The compatibility door
/// ([`sync_seasons`]) carries no identity at all, so an unconditional
/// write from it would blank the very column the next refresh pairs on —
/// which is the fastest route back to delete-and-reinsert and to 7328
/// unlinked files.
async fn upsert_episode_row(
    conn: &mut sqlx::SqliteConnection,
    ids: EpisodeIds,
    season_number: i32,
    episode: &DecidedEpisode,
) -> Result<(), AppError> {
    sqlx::query(
        "INSERT INTO library_episodes \
            (id, item_id, season_id, season_number, episode_number, title, air_date, \
             monitored, source, external_id, absolute_number) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?) \
         ON CONFLICT(id) DO UPDATE SET \
            season_id       = excluded.season_id, \
            season_number   = excluded.season_number, \
            episode_number  = excluded.episode_number, \
            title           = excluded.title, \
            air_date        = excluded.air_date, \
            monitored       = excluded.monitored, \
            source          = COALESCE(excluded.source, source), \
            external_id     = COALESCE(excluded.external_id, external_id), \
            absolute_number = COALESCE(excluded.absolute_number, absolute_number)",
    )
    .bind(ids.id.to_string())
    .bind(ids.item_id.to_string())
    .bind(ids.season_id.to_string())
    .bind(i64::from(season_number))
    .bind(i64::from(episode.number))
    .bind(episode.title.as_deref())
    .bind(episode.air_date.map(OffsetDateTime::unix_timestamp))
    .bind(i64::from(episode.monitored))
    .bind(episode.source.map(MetadataSource::label))
    .bind(episode.external_id.as_deref())
    .bind(episode.absolute_number.map(i64::from))
    .execute(conn)
    .await?;
    Ok(())
}

/// Who owns a series' shape today.
///
/// Lives here rather than in [`crate::structure`] because it is the shape
/// of a row; the module that consumes it re-exports the name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StructureOwner {
    /// Source recorded on the item.
    ///
    /// `None` means **unclaimed**, not "anyone may write". Every series
    /// catalogued since the identity migration reads this way, because
    /// [`upsert`] does not write the column — so the first tree write
    /// adopts the item, and only a *disagreement* is refused. Note
    /// [`source_of`] turns an unregistered label into `None` too; that is
    /// the right call for rendering, and it means a typo in this column
    /// reads as unclaimed rather than as a hard stop.
    pub source: Option<MetadataSource>,
    /// brarr's word for the ordering in force.
    pub family: Option<OrderingFamily>,
    /// The owning source's opaque handle for it.
    pub handle: Option<String>,
    /// The cut the operator declared, as the *recipe* and not only as
    /// its result.
    ///
    /// A hand-declared 12/13 is applied once and then erased by the next
    /// refresh unless there is somewhere to re-apply it from — the one
    /// thing the translation table got right by leaving its rows in
    /// place. `None` for every ordering but [`OrderingFamily::Manual`].
    pub recipe: Option<String>,
    /// Whether the operator froze the choice.
    pub pinned: bool,
}

/// Read who owns a series' shape.
///
/// # Errors
///
/// Returns [`AppError::NotFound`] for an unknown item, or
/// [`AppError::Database`] on SQL failure.
pub async fn structure_owner(pool: &Pool, item_id: Uuid) -> Result<StructureOwner, AppError> {
    let row = sqlx::query(
        "SELECT structure_source, structure_family, structure_handle, structure_recipe, \
                structure_pinned \
         FROM library_items WHERE id = ?",
    )
    .bind(item_id.to_string())
    .fetch_optional(pool)
    .await?
    .ok_or_else(|| AppError::NotFound(format!("library item {item_id}")))?;

    let family: Option<String> = row.try_get("structure_family")?;
    let pinned: i64 = row.try_get("structure_pinned")?;
    Ok(StructureOwner {
        source: source_of(&row, "structure_source")?,
        family: family.as_deref().and_then(OrderingFamily::parse),
        handle: row.try_get("structure_handle")?,
        recipe: row.try_get("structure_recipe")?,
        pinned: pinned != 0,
    })
}

/// Record the ordering an **operator** chose, pin included.
///
/// Separate from [`record_structure`], which is what a sweep writes after
/// a refresh, and the difference is which columns each may touch. A sweep
/// reports where the tree came from; it must never move the pin or erase
/// a recipe, or the next cycle would walk back a decision. This writes
/// all five, because all five *are* the decision.
///
/// # Errors
///
/// Returns [`AppError::Database`] on SQL failure.
pub async fn set_structure_choice(
    pool: &Pool,
    item_id: Uuid,
    source: MetadataSource,
    ordering: &Ordering,
    recipe: Option<&str>,
    pinned: bool,
) -> Result<(), AppError> {
    sqlx::query(
        "UPDATE library_items SET \
            structure_source = ?, \
            structure_family = ?, \
            structure_handle = ?, \
            structure_recipe = ?, \
            structure_pinned = ?, \
            structure_refreshed_at = ? \
         WHERE id = ?",
    )
    .bind(source.label())
    .bind(ordering.family().label())
    .bind(ordering.handle())
    .bind(recipe)
    .bind(i64::from(pinned))
    .bind(OffsetDateTime::now_utc().unix_timestamp())
    .bind(item_id.to_string())
    .execute(pool)
    .await?;
    Ok(())
}

/// Record who owns the shape, after a tree write was accepted.
///
/// Deliberately **not** part of [`write_tree`]'s transaction. The tree is
/// the thing that must be all-or-nothing; this is a label describing what
/// was just committed, and a failure here leaves the column reading what
/// it read before — stale, but never claiming an ownership the rows do
/// not have.
///
/// `structure_pinned` is never touched: it is the operator's, and a sweep
/// that cleared it would be the one-way door the flag exists to avoid.
///
/// # Errors
///
/// Returns [`AppError::Database`] on SQL failure.
pub async fn record_structure(
    pool: &Pool,
    item_id: Uuid,
    source: MetadataSource,
    ordering: &Ordering,
) -> Result<(), AppError> {
    sqlx::query(
        "UPDATE library_items SET \
            structure_source       = ?, \
            structure_family       = ?, \
            structure_handle       = ?, \
            structure_refreshed_at = ? \
         WHERE id = ?",
    )
    .bind(source.label())
    .bind(ordering.family().label())
    .bind(ordering.handle())
    .bind(OffsetDateTime::now_utc().unix_timestamp())
    .bind(item_id.to_string())
    .execute(pool)
    .await?;
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
        "SELECT id, item_id, season_id, season_number, episode_number, title, air_date, monitored, \
                source, external_id, absolute_number \
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
                source: source_of(row, "source")?,
                external_id: row.try_get("external_id")?,
                absolute_number: opt_i32_at(row, "absolute_number")?,
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

    /// The net under every gate: a write that would unlink a file rolls
    /// back, even when nothing upstream saw it coming.
    ///
    /// `structure::plan` refuses an orphan before the transaction opens,
    /// so this reaches [`write_tree`] directly — which is the point. The
    /// count exists for the pairing that goes wrong in a way the gates
    /// did *not* predict, and the only honest way to test a backstop is
    /// to hand it the case the front stops would have caught.
    #[tokio::test]
    async fn a_rise_in_orphan_grabs_rolls_back() {
        let pool = open_memory().await.unwrap();
        let item = upsert(&pool, &Seed::series(1, "A Series").build())
            .await
            .unwrap();
        sync_seasons(
            &pool,
            item.id,
            &[NewSeason {
                season_number: 1,
                episode_count: 2,
                air_date: None,
                episodes: vec![crate::db::seed::episode(1), crate::db::seed::episode(2)],
            }],
        )
        .await
        .unwrap();

        let rows = episodes(&pool, item.id).await.unwrap();
        let doomed = rows.iter().find(|e| e.episode_number == 2).unwrap();

        let base_url = url::Url::parse("https://capybarabr.com/").unwrap();
        let provider = crate::db::providers::insert(
            &pool,
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
        crate::db::grabs::reserve(
            &pool,
            &crate::db::grabs::NewGrab {
                item_id: item.id,
                episode_id: Some(doomed.id),
                season_number: None,
                decision_id: None,
                provider_id: provider.id,
                provider_name: "capybara",
                release_id_remote: "rel",
                release_name: "rel",
                download_url: None,
                protocol: crate::db::grabs::Protocol::Torrent,
            },
        )
        .await
        .unwrap()
        .expect("first reservation");

        // A decided tree that simply does not mention episode 2. Nothing
        // here is malformed; it is what a bad pairing produces.
        let kept = rows.iter().find(|e| e.episode_number == 1).unwrap();
        let season_id = kept.season_id;
        let truncated = vec![DecidedSeason {
            id: season_id,
            number: 1,
            episode_count: 1,
            air_date: None,
            monitored: true,
            episodes: vec![DecidedEpisode {
                id: kept.id,
                number: 1,
                title: None,
                air_date: None,
                source: None,
                external_id: None,
                absolute_number: None,
                monitored: true,
            }],
        }];

        let err = write_tree(&pool, item.id, &truncated)
            .await
            .expect_err("unlinking a file must roll the write back");
        assert!(err.to_string().contains("sem episódio"), "{err}");

        // And the rollback is real: the row is still there, still linked.
        let after = episodes(&pool, item.id).await.unwrap();
        assert_eq!(after.len(), 2, "the pruned episode came back");
        let still_linked: i64 = sqlx::query(
            "SELECT count(*) AS n FROM grabs WHERE item_id = ? AND episode_id IS NOT NULL",
        )
        .bind(item.id.to_string())
        .fetch_one(&pool)
        .await
        .unwrap()
        .try_get("n")
        .unwrap();
        assert_eq!(still_linked, 1, "and the file kept its episode");
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
        assert_eq!(chased[0].title, "B");
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

    /// **The double-catalogue bug, as a test.**
    ///
    /// `get_by_tmdb` can only answer on one axis, so a series added
    /// through TMDB and met again on the \*arr's TVDB axis read as absent
    /// and was catalogued a second time. Every id a title carries has to
    /// find the same row.
    #[tokio::test]
    async fn a_title_is_found_by_every_id_it_carries() {
        use brarr_core::{ExternalId, MetadataSource};

        let pool = open_memory().await.unwrap();
        let stored = upsert(
            &pool,
            &Seed::series(76_479, "The Boys")
                .imdb("tt1190634")
                .tvdb(355_567)
                .build(),
        )
        .await
        .unwrap();

        for (source, raw) in [
            (MetadataSource::Tmdb, "76479"),
            (MetadataSource::Imdb, "tt1190634"),
            (MetadataSource::Tvdb, "355567"),
        ] {
            let id = ExternalId::new(source, raw).unwrap();
            let found = get_by_external(&pool, MediaType::Tv, &id).await;
            assert!(found.is_ok(), "{source} did not find the title");
            assert_eq!(found.map(|f| f.id).ok(), Some(stored.id));
        }

        // Whichever IMDb convention the caller holds, since the id
        // canonicalises before it reaches the query.
        let loose = ExternalId::new(MetadataSource::Imdb, "1190634").unwrap();
        assert_eq!(
            get_by_external(&pool, MediaType::Tv, &loose)
                .await
                .unwrap()
                .id,
            stored.id
        );

        // And a film sharing the number is a different title.
        let tmdb = ExternalId::new(MetadataSource::Tmdb, "76479").unwrap();
        assert!(matches!(
            get_by_external(&pool, MediaType::Movie, &tmdb).await,
            Err(AppError::NotFound(_))
        ));
    }

    #[tokio::test]
    async fn missing_media_type_is_rejected() {
        let pool = open_memory().await.unwrap();
        let err = upsert(
            &pool,
            &NewLibraryItem {
                media_type: None,
                ..Seed::movie(1, "X").build()
            },
        )
        .await
        .unwrap_err();
        assert!(matches!(err, AppError::InvalidInput(_)));
    }

    /// **A row nothing can name is one nothing can find again.** Identity
    /// used to be a NOT NULL column, so this state was unrepresentable;
    /// as a set it is a `Vec` somebody can leave empty, and the row it
    /// would create is invisible to `get_by_external` — so every sweep
    /// would make another one.
    #[tokio::test]
    async fn an_item_with_no_identity_is_rejected() {
        let pool = open_memory().await.unwrap();
        let err = upsert(
            &pool,
            &NewLibraryItem {
                ids: Vec::new(),
                ..Seed::movie(1, "X").build()
            },
        )
        .await
        .unwrap_err();
        assert!(matches!(err, AppError::InvalidInput(_)), "{err:?}");
        assert_eq!(list(&pool).await.unwrap().len(), 0, "and nothing landed");
    }

    /// **Any id finds it.** A series added through TMDB and met again on
    /// the \*arr's TVDB axis is one row, not two — the reason identity
    /// became a set at all.
    #[tokio::test]
    async fn a_second_upsert_under_another_id_refreshes_the_same_row() {
        let pool = open_memory().await.unwrap();
        let first = upsert(
            &pool,
            &Seed::series(1_396, "Breaking Bad").tvdb(81_189).build(),
        )
        .await
        .unwrap();

        // Met again knowing only the TheTVDB id, under a new title.
        let mut only_tvdb = Seed::series(1_396, "Breaking Bad (renomeado)").build();
        only_tvdb
            .ids
            .retain(|id| id.source() == MetadataSource::Tvdb);
        assert!(only_tvdb.ids.is_empty(), "the seed carries no TVDB id yet");
        only_tvdb
            .ids
            .push(ExternalId::new(MetadataSource::Tvdb, "81189").unwrap());

        let second = upsert(&pool, &only_tvdb).await.unwrap();
        assert_eq!(second.id, first.id, "one row, found by the other axis");
        assert_eq!(second.title, "Breaking Bad (renomeado)");
        assert_eq!(list(&pool).await.unwrap().len(), 1);
    }

    /// The operator's columns survive a metadata refresh — the rule this
    /// module has had since it existed, restated because the mechanism
    /// under it changed from `ON CONFLICT` to a read-then-write.
    #[tokio::test]
    async fn an_upsert_leaves_what_the_operator_set() {
        let pool = open_memory().await.unwrap();
        let item = upsert(&pool, &Seed::movie(603, "The Matrix").build())
            .await
            .unwrap();
        set_monitored(&pool, item.id, false).await.unwrap();
        set_placement(&pool, item.id, None, Some("/midias/filmes"))
            .await
            .unwrap();

        let refreshed = upsert(&pool, &Seed::movie(603, "Matrix").build())
            .await
            .unwrap();
        assert_eq!(refreshed.id, item.id);
        assert_eq!(refreshed.title, "Matrix", "the metadata did refresh");
        assert!(!refreshed.monitored, "and the operator's flag did not");
        assert_eq!(refreshed.root_folder.as_deref(), Some("/midias/filmes"));
        assert_eq!(refreshed.added_at, item.added_at);
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
