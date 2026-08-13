//! Bringing a Sonarr/Radarr catalogue into brarr's own library.
//!
//! The block that makes brarr a viable replacement rather than a second
//! opinion: 468 titles on this operator's stack, and re-registering them
//! by hand is not a migration path. It also solves the hardest problem
//! the disk importer left open, for free — **Sonarr already knows which
//! file is which episode**, including the absolute-numbered anime that no
//! marker regex reads (545 real files here).
//!
//! ```text
//!   *arr catalogue ──► tmdb_id ──► library_items   (metadata via TMDB)
//!         │                            │
//!         │                            ▼
//!         │                     library_seasons/episodes
//!         ▼
//!   file paths ──► remote_path::translate ──► grabs (protocol = local)
//! ```
//!
//! ## Three things this module must never do
//!
//! - **Write to the filesystem.** Not a link, not a copy, not a rename.
//!   The files are already in the operator's library, on the same share
//!   brarr mounts; the only thing missing is brarr's record of them.
//!   There is no destination anywhere in this module, which is the same
//!   guarantee `adopt::AdoptAction::InPlace` gets from carrying no path.
//! - **Enable monitoring on the passive path.** All three of the
//!   operator's *arr still have indexers and download clients aimed at
//!   the same qBittorrent and SABnzbd brarr uses. A title synced as
//!   monitored puts two agents on it, which is the exact double agency
//!   that motivated taking the *arr out of the loop. [`sync_one`]
//!   expresses that by **not accepting a monitoring parameter at all**.
//! - **Overwrite operator state.** Monitoring, profile and root folder
//!   are written only for titles this run *creates*. That is
//!   `db::library`'s own doctrine — metadata is a cache, monitoring is
//!   state — extended to placement, because a passive sweep runs forever
//!   and would otherwise walk back every choice made by hand.
//!
//! ## Why the root is mapped, and not the path
//!
//! Every path an *arr reports is in its own namespace: Sonarr answers
//! `/data/Series/9-1-1/…` while brarr mounts the same share at
//! `/midias/Series`. That is the class of defect that once marked five
//! finished downloads as `failed` with the files intact on disk. An *arr
//! has one or two roots and thousands of files under them, so the
//! operator maps the root — two or three choices for the whole migration
//! — and every path below follows. The matching itself is
//! [`crate::remote_path`]'s, shared with the download clients.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use brarr_arr::{ArrClient, ArrEpisode, ArrError, ArrFile, ArrKind, ArrMovie, ArrSeries};
use brarr_tmdb::TmdbClient;
use time::OffsetDateTime;
use tokio::task::JoinHandle;
use tokio::time::sleep;
use tracing::{debug, info, warn};
use uuid::Uuid;

use crate::db::arr_instances::{self, ArrInstanceRow};
use crate::db::arr_root_mappings::{self, ArrRootMapping};
use crate::db::library::{self, LibraryItem, MediaType, MonitorScope};
use crate::db::{Pool, episode_numbering, grabs};
use crate::episode_match::EpisodeMatcher;
use crate::remote_path::{self, PrefixRule};
use crate::tmdb_sync;
use crate::{AppError, AppState};

/// How many per-title failures a report keeps verbatim. Past that the
/// counter still rises: a migration that fails three hundred times has
/// one cause, and three hundred identical lines do not help find it.
const MAX_FAILURES_KEPT: usize = 20;

// ---------------------------------------------------------------------
// What an *arr says
// ---------------------------------------------------------------------

/// One title as an \*arr has it, reduced to what brarr stores.
#[derive(Debug, Clone)]
pub struct ArrTitle {
    /// \*arr-side id, needed to fetch a series' episodes.
    pub arr_id: u64,
    /// Movie or series.
    pub media_type: MediaType,
    /// TMDB id. `0` means the \*arr never linked one, and nothing can be
    /// done with the title until it does — brarr's library is keyed on
    /// this.
    pub tmdb_id: i64,
    /// TVDb id, series only. The bridge out of a `tmdb_id` of zero.
    pub tvdb_id: i64,
    /// Title as the \*arr shows it, for the preview only. brarr takes the
    /// name from TMDB, never from here.
    pub title: String,
    /// Release / first-air year, same caveat.
    pub year: Option<i32>,
    /// Whether the \*arr is chasing it.
    pub monitored: bool,
    /// Its folder, **in the \*arr's namespace**.
    pub path: String,
    /// The root that folder sits under, same namespace.
    pub root_folder_path: String,
    /// Per-season monitoring, series only.
    pub seasons: Vec<(i32, bool)>,
    /// The movie's file, when Radarr inlined one. A series' files need a
    /// second call per title and only the commit pays for it — see
    /// [`series_detail`].
    pub files: Vec<ArrFileRef>,
}

/// One file the \*arr already matched to a target.
///
/// Deliberately without the size the \*arr reports. brarr stats the file
/// itself once translated, and a second number nothing reads is a number
/// free to disagree with the disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArrFileRef {
    /// Absolute path in the \*arr's namespace.
    pub path: String,
    /// `None` for a movie, `Some((season, episode))` for an episode.
    ///
    /// These are the **\*arr's** coordinates, which are `TheTVDB`'s. They
    /// are not always TMDB's, and [`crate::episode_match`] is what
    /// reconciles the two.
    pub episode: Option<(i32, i32)>,
    /// The \*arr's absolute episode number, when it has one.
    pub absolute: Option<i32>,
}

/// The second read of one series: the file→episode pairing, and the
/// per-episode monitoring, both out of the same two calls.
#[derive(Debug, Clone, Default)]
pub struct SeriesDetail {
    /// Files, one per episode Sonarr has a file for.
    pub files: Vec<ArrFileRef>,
    /// `(season, episode, monitored)` for every episode, file or not —
    /// which is what [`MonitorChoice::Mirror`] copies.
    pub monitoring: Vec<(i32, i32, bool)>,
    /// Every episode's coordinates in the \*arr's numbering, file or not.
    ///
    /// This is the **scene's** numbering: Sonarr is `TheTVDB`-numbered and
    /// releases follow `TheTVDB`. Carried for every episode, not just the
    /// ones with files, because the numbering exists to find what is
    /// *missing*.
    pub numbering: Vec<ArrNumber>,
}

/// One episode's coordinates in the \*arr's numbering.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ArrNumber {
    /// Season the \*arr puts it in.
    pub season: i32,
    /// Number within that season.
    pub episode: i32,
    /// Position in the series as a whole, when the \*arr has one.
    pub absolute: Option<i32>,
}

/// Radarr row → title. The file comes inline, so a movie never needs a
/// second call.
fn movie_to_title(movie: &ArrMovie) -> ArrTitle {
    ArrTitle {
        arr_id: movie.id,
        media_type: MediaType::Movie,
        tmdb_id: i64::from(movie.tmdb_id),
        tvdb_id: 0,
        title: movie.title.clone(),
        year: (movie.year > 0).then_some(movie.year),
        monitored: movie.monitored,
        path: movie.path.clone(),
        root_folder_path: movie.root_folder_path.clone(),
        seasons: Vec::new(),
        files: movie
            .movie_file
            .iter()
            .filter(|f| !f.path.is_empty())
            .map(|f| ArrFileRef {
                path: f.path.clone(),
                episode: None,
                absolute: None,
            })
            .collect(),
    }
}

/// Sonarr row → title. Season 0 is dropped here as it is everywhere else
/// in brarr: TMDB's specials bucket carries 76 entries against The Boys'
/// 40 real episodes, and counting them makes every number wrong.
fn series_to_title(series: &ArrSeries) -> ArrTitle {
    ArrTitle {
        arr_id: series.id,
        media_type: MediaType::Tv,
        tmdb_id: i64::from(series.tmdb_id),
        tvdb_id: i64::from(series.tvdb_id),
        title: series.title.clone(),
        year: (series.year > 0).then_some(series.year),
        monitored: series.monitored,
        path: series.path.clone(),
        root_folder_path: series.root_folder_path.clone(),
        seasons: series
            .seasons
            .iter()
            .filter(|s| s.season_number > 0)
            .map(|s| (s.season_number, s.monitored))
            .collect(),
        files: Vec::new(),
    }
}

/// Pair each episode with the file Sonarr matched to it.
///
/// **This is the whole reason to import from Sonarr rather than from
/// disk.** The pairing is Sonarr's and already made, so brarr reads no
/// name and applies no regex — which is what rescues absolute-numbered
/// anime, 545 files on this collection that no marker parser reads.
///
/// A file covering two episodes appears twice, deliberately — a 40-minute
/// `S05E33E34` is two episodes to Sonarr, to Plex and to the operator,
/// and it is two rows here. The local barrier carries the episode in its
/// key so both are recorded; see
/// `migrations/20260813120000_multi_episode_files.sql`.
fn join_episode_files(episodes: &[ArrEpisode], files: &[ArrFile]) -> Vec<ArrFileRef> {
    let by_id: HashMap<u64, &ArrFile> = files.iter().map(|f| (f.id, f)).collect();
    episodes
        .iter()
        .filter(|e| e.season_number > 0 && e.episode_file_id > 0)
        .filter_map(|e| {
            let file = by_id.get(&e.episode_file_id)?;
            (!file.path.is_empty()).then(|| ArrFileRef {
                path: file.path.clone(),
                episode: Some((e.season_number, e.episode_number)),
                absolute: e.absolute_episode_number,
            })
        })
        .collect()
}

/// Every episode's coordinates in the \*arr's numbering.
fn arr_numbering(episodes: &[ArrEpisode]) -> Vec<ArrNumber> {
    episodes
        .iter()
        .filter(|e| e.season_number > 0 && e.episode_number > 0)
        .map(|e| ArrNumber {
            season: e.season_number,
            episode: e.episode_number,
            absolute: e.absolute_episode_number,
        })
        .collect()
}

/// Store the \*arr's numbering as this title's search numbering.
///
/// **The point of the whole feature.** `20260808130000` gave brarr a
/// translation table and a screen to pick a TMDB episode group; two
/// titles got one in a week, out of the fifteen that need it, because
/// picking an ordering by hand per title — after discovering it exists
/// and which of eleven is right — is work nobody does. The \*arr already
/// holds the answer and brarr already reads it every pass.
///
/// The matcher here is built with an **empty** reverse map on purpose:
/// deriving the translation from a translation this function itself
/// wrote would be self-referential, and a wrong row would then keep
/// re-deriving itself. Only the canonical coordinate and the absolute
/// axis get a vote.
///
/// Best-effort by design — a title whose numbering cannot be derived is
/// left on the canonical one, which is where it already was.
async fn sync_search_numbering(
    pool: &Pool,
    item: &LibraryItem,
    detail: &SeriesDetail,
) -> Result<(), AppError> {
    if matches!(episode_numbering::source(pool, item.id).await?, Some(s) if !s.is_automatic()) {
        return Ok(());
    }
    let episodes = library::episodes(pool, item.id).await?;
    let coordinates: HashMap<Uuid, (i32, i32)> = episodes
        .iter()
        .map(|e| (e.id, (e.season_number, e.episode_number)))
        .collect();
    let matcher = EpisodeMatcher::new(&episodes, HashMap::new());

    let mut rows = Vec::new();
    for n in &detail.numbering {
        let Some(found) = matcher.resolve(n.season, n.episode, n.absolute) else {
            continue;
        };
        let Some(&canonical) = coordinates.get(&found) else {
            continue;
        };
        // Absent *is* the encoding of "no translation", so an episode both
        // sides number the same contributes nothing. Without this the
        // Simpsons would carry 801 rows that say what their absence says.
        if canonical == (n.season, n.episode) {
            continue;
        }
        rows.push(episode_numbering::NumberingRow {
            part_order: n.season,
            part_name: None,
            group: episode_numbering::Numbering {
                season: n.season,
                episode: n.episode,
            },
            canonical: episode_numbering::Numbering {
                season: canonical.0,
                episode: canonical.1,
            },
            tmdb_episode_id: None,
        });
    }

    if episode_numbering::apply_from_arr(pool, item.id, &rows).await? && !rows.is_empty() {
        debug!(
            target: "brarr_orchestrator::arr_import",
            item = %item.id, title = %item.title, episodes = rows.len(),
            "derived the search numbering from the *arr"
        );
    }
    Ok(())
}

/// Per-episode monitoring, specials excluded like everywhere else.
fn episode_monitoring(episodes: &[ArrEpisode]) -> Vec<(i32, i32, bool)> {
    episodes
        .iter()
        .filter(|e| e.season_number > 0)
        .map(|e| (e.season_number, e.episode_number, e.monitored))
        .collect()
}

/// The whole catalogue of one instance, in one call.
///
/// Deliberately **one** call: the series' episode lists are fetched per
/// title by [`series_detail`], and only by the commit. A preview that
/// walked 176 series would make three hundred requests to answer a
/// question the operator has not asked yet.
///
/// # Errors
///
/// Propagates the \*arr client's transport, HTTP and decode failures.
pub async fn read_catalogue(client: &ArrClient) -> Result<Vec<ArrTitle>, ArrError> {
    match client.instance().kind {
        ArrKind::Radarr => Ok(client
            .catalogue_movies()
            .await?
            .iter()
            .map(movie_to_title)
            .collect()),
        ArrKind::Sonarr => Ok(client
            .catalogue_series()
            .await?
            .iter()
            .map(series_to_title)
            .collect()),
    }
}

/// The second read of one series — two small calls rather than one big
/// one.
///
/// `episodes` is fetched without `includeEpisodeFile`: that flag inlines
/// the file object with its media info and takes one series from a few KB
/// to ~300 KB. Over 176 series that is the difference between ~50 MB of
/// JSON and a few hundred.
///
/// # Errors
///
/// Propagates the \*arr client's failures.
pub async fn series_detail(client: &ArrClient, arr_id: u64) -> Result<SeriesDetail, ArrError> {
    let episodes = client.episodes(arr_id).await?;
    let files = client.episode_files(arr_id).await?;
    Ok(SeriesDetail {
        files: join_episode_files(&episodes, &files),
        monitoring: episode_monitoring(&episodes),
        numbering: arr_numbering(&episodes),
    })
}

/// Build a client for one configured instance.
///
/// # Errors
///
/// [`AppError::InvalidInput`] when the TLS backend cannot be built.
pub fn client_for(row: &ArrInstanceRow) -> Result<ArrClient, AppError> {
    ArrClient::new(row.to_arr_instance())
        .map_err(|e| AppError::InvalidInput(format!("{}: {e}", row.name)))
}

// ---------------------------------------------------------------------
// The preview
// ---------------------------------------------------------------------

/// One \*arr root folder, and what brarr makes of it.
#[derive(Debug, Clone)]
pub struct PlannedRoot {
    /// Path as the \*arr reports it.
    pub arr_path: String,
    /// Where a mapping sends it. `None` when none covers it.
    pub mapped_to: Option<PathBuf>,
    /// The mapping row that fired, so the screen can offer to remove it.
    /// Not always the row for this exact prefix — the longest rule wins,
    /// and a `/data` rule covers `/data/Series` too.
    pub mapping_id: Option<Uuid>,
    /// Whether brarr can open that directory. The check that catches a
    /// wrong mapping *before* seven thousand records, not after.
    pub reachable: bool,
    /// Titles the \*arr keeps under it.
    pub titles: usize,
}

/// What the import would do with one title.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TitleStatus {
    /// Not catalogued here yet. The import adds it.
    New,
    /// Already in the library. Metadata refreshes and files are still
    /// recorded; monitoring, profile and root folder are left alone.
    Known,
    /// Nothing can be done, and the sentence says why.
    Blocked(String),
}

impl TitleStatus {
    /// Label for the preview row.
    #[must_use]
    pub fn label(&self) -> &str {
        match self {
            Self::New => "novo",
            Self::Known => "já na biblioteca",
            Self::Blocked(why) => why,
        }
    }

    /// Whether the commit would touch this row at all.
    #[must_use]
    pub fn actionable(&self) -> bool {
        !matches!(self, Self::Blocked(_))
    }
}

/// One row of the preview.
#[derive(Debug, Clone)]
pub struct PlannedTitle {
    /// \*arr-side id.
    pub arr_id: u64,
    /// Title as the \*arr shows it.
    pub title: String,
    /// Year, when the \*arr has one.
    pub year: Option<i32>,
    /// Movie or series.
    pub media_type: MediaType,
    /// TMDB id, `0` when the \*arr never linked one.
    pub tmdb_id: i64,
    /// Whether the \*arr is chasing it.
    pub monitored: bool,
    /// What the import would do.
    pub status: TitleStatus,
    /// Whether brarr can open the title's own folder after translation.
    /// This is what a wrong root mapping looks like: every title new,
    /// every folder unseen.
    pub folder_seen: bool,
}

/// What one instance would produce.
#[derive(Debug, Clone)]
pub struct ImportPlan {
    /// Instance the plan was built for.
    pub instance_id: Uuid,
    /// Its display name.
    pub instance_name: String,
    /// Sonarr or Radarr.
    pub kind: ArrKind,
    /// Roots the \*arr reports, with their mapping.
    pub roots: Vec<PlannedRoot>,
    /// One row per catalogued title.
    pub titles: Vec<PlannedTitle>,
}

impl ImportPlan {
    /// Titles the import would add.
    #[must_use]
    pub fn new_titles(&self) -> usize {
        self.titles
            .iter()
            .filter(|t| t.status == TitleStatus::New)
            .count()
    }

    /// Titles already catalogued here.
    #[must_use]
    pub fn known_titles(&self) -> usize {
        self.titles
            .iter()
            .filter(|t| t.status == TitleStatus::Known)
            .count()
    }

    /// Titles nothing can be done with.
    #[must_use]
    pub fn blocked_titles(&self) -> usize {
        self.titles
            .iter()
            .filter(|t| !t.status.actionable())
            .count()
    }

    /// Titles whose folder brarr can actually open.
    #[must_use]
    pub fn seen_folders(&self) -> usize {
        self.titles.iter().filter(|t| t.folder_seen).count()
    }

    /// Whether any root is still unmapped — the one thing the operator
    /// has to fix before the numbers mean anything.
    #[must_use]
    pub fn has_unmapped_root(&self) -> bool {
        self.roots.iter().any(|r| r.mapped_to.is_none())
    }
}

/// What one instance would import, without writing anything.
///
/// **Writes nothing** — no row, no file, no probe. One call to the \*arr
/// for the catalogue, and one `stat` per title to answer the only
/// question a preview exists for: does the root mapping actually reach
/// this operator's disk?
///
/// # Errors
///
/// [`AppError::NotFound`] when the instance is gone,
/// [`AppError::InvalidInput`] when the \*arr cannot be read,
/// [`AppError::Database`] on SQL failure.
pub async fn plan(state: &AppState, instance_id: Uuid) -> Result<ImportPlan, AppError> {
    let pool = state.pool();
    let row = arr_instances::get_by_id(pool, instance_id).await?;
    let mappings = arr_root_mappings::for_instance(pool, instance_id).await?;
    let rules = arr_root_mappings::rules(&mappings);

    let client = client_for(&row)?;
    let titles = read_catalogue(&client)
        .await
        .map_err(|e| AppError::InvalidInput(format!("{}: {e}", row.name)))?;

    let seen = folders_seen(&titles, &rules).await;
    let mut rows = Vec::with_capacity(titles.len());
    for (title, folder_seen) in titles.iter().zip(seen) {
        rows.push(PlannedTitle {
            arr_id: title.arr_id,
            title: title.title.clone(),
            year: title.year,
            media_type: title.media_type,
            tmdb_id: title.tmdb_id,
            monitored: title.monitored,
            status: status_of(pool, title).await?,
            folder_seen,
        });
    }

    Ok(ImportPlan {
        instance_id,
        instance_name: row.name,
        kind: row.kind,
        roots: plan_roots(&titles, &mappings),
        titles: rows,
    })
}

/// Whether this title is new, known, or beyond help.
async fn status_of(pool: &Pool, title: &ArrTitle) -> Result<TitleStatus, AppError> {
    if title.tmdb_id <= 0 && title.tvdb_id <= 0 {
        return Ok(TitleStatus::Blocked("sem id do TMDB no *arr".to_owned()));
    }
    if title.tmdb_id <= 0 {
        // A series the *arr linked only to TVDb. The commit bridges it
        // through TMDB's find endpoint, so it is not blocked — but the
        // preview cannot say whether it is new without spending a
        // request per title, and it says so.
        return Ok(TitleStatus::New);
    }
    match library::get_by_tmdb(pool, title.media_type, title.tmdb_id).await {
        Ok(_) => Ok(TitleStatus::Known),
        Err(AppError::NotFound(_)) => Ok(TitleStatus::New),
        Err(e) => Err(e),
    }
}

/// Group the titles by the root the \*arr keeps them under, and say what
/// each root translates to.
fn plan_roots(titles: &[ArrTitle], mappings: &[ArrRootMapping]) -> Vec<PlannedRoot> {
    let rules = arr_root_mappings::rules(mappings);
    let mut counts: HashMap<&str, usize> = HashMap::new();
    for title in titles {
        if !title.root_folder_path.is_empty() {
            *counts.entry(title.root_folder_path.as_str()).or_default() += 1;
        }
    }
    // A root the *arr declares but keeps nothing under still deserves a
    // row: it is a mapping the operator has to make before the next
    // title lands there.
    for mapping in mappings {
        counts.entry(mapping.arr_path.as_str()).or_default();
    }

    let mut roots: Vec<PlannedRoot> = counts
        .into_iter()
        .map(|(arr_path, titles)| {
            let translated = remote_path::translate(&rules, arr_path);
            let mapping_id = translated.applied.as_ref().map(|a| a.id);
            let mapped_to = mapping_id.map(|_| translated.local.clone());
            PlannedRoot {
                arr_path: arr_path.to_owned(),
                reachable: mapped_to.as_ref().is_some_and(|p| p.is_dir()),
                mapped_to,
                mapping_id,
                titles,
            }
        })
        .collect();
    roots.sort_by(|a, b| a.arr_path.cmp(&b.arr_path));
    roots
}

/// Which title folders brarr can actually open, after translation.
///
/// One `spawn_blocking` for the whole batch: a `stat` is fast right up
/// until the mount is a network share, and a stalled call on a runtime
/// worker takes the process with it.
async fn folders_seen(titles: &[ArrTitle], rules: &[PrefixRule]) -> Vec<bool> {
    let paths: Vec<Option<PathBuf>> = titles
        .iter()
        .map(|t| {
            if t.path.is_empty() {
                return None;
            }
            let translated = remote_path::translate(rules, &t.path);
            remote_path::is_usable(&translated).then_some(translated.local)
        })
        .collect();
    let count = paths.len();
    tokio::task::spawn_blocking(move || {
        paths
            .into_iter()
            .map(|p| p.is_some_and(|path| path.is_dir()))
            .collect()
    })
    .await
    .unwrap_or_else(|e| {
        warn!(target: "brarr_orchestrator::arr_import", error = %e, "folder check failed");
        vec![false; count]
    })
}

// ---------------------------------------------------------------------
// The commit
// ---------------------------------------------------------------------

/// What monitoring the titles this run *creates* start with.
///
/// It never applies to a title already in the library: monitoring is
/// operator state, and a sweep that rewrote it would walk back every
/// choice made by hand, once a minute, forever.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MonitorChoice {
    /// Catalogue everything, chase nothing. **The migration's answer**:
    /// 468 titles arriving monitored, against three \*arr that still have
    /// their own indexers and download clients, is two agents on every
    /// one of them.
    #[default]
    Paused,
    /// Copy the \*arr's flags — item, season and episode.
    Mirror,
}

impl MonitorChoice {
    /// Persisted/posted label.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Paused => "paused",
            Self::Mirror => "mirror",
        }
    }

    /// Parse from the form, defaulting to the safe answer. An
    /// unrecognised value must never turn into "chase everything".
    #[must_use]
    pub fn from_label(s: &str) -> Self {
        match s.trim() {
            "mirror" => Self::Mirror,
            _ => Self::Paused,
        }
    }

    /// The scope a created row starts with.
    ///
    /// Set **before** the season tree is built, because
    /// [`library::sync_seasons`] reads it to decide the default for every
    /// row it has never seen. Setting it afterwards would build the tree
    /// under the old scope and then quietly disagree with the screen.
    fn scope(self) -> MonitorScope {
        match self {
            Self::Paused => MonitorScope::Nothing,
            Self::Mirror => MonitorScope::All,
        }
    }
}

/// What the operator chose for one run.
#[derive(Debug, Clone, Copy, Default)]
pub struct ImportOptions {
    /// Monitoring for created titles.
    pub monitoring: MonitorChoice,
    /// Quality profile for created titles. `None` falls back to the
    /// instance's own — filmes and séries score under the dubbed-PT
    /// profile, animes under the JP one, and that lives on the instance.
    pub profile_id: Option<Uuid>,
}

/// Per-file outcome of one title.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FileCounts {
    /// Files now recorded as brarr's.
    pub adopted: usize,
    /// Files a live grab already covered.
    pub already: usize,
    /// Files already recorded that had **lost** their episode, and which
    /// the \*arr's own pairing put back. Separate from `already` because
    /// it is a repair, not a no-op: these are the rows a metadata
    /// refresh unlinked, and the ones no file name can identify —
    /// Sonarr knows an absolute-numbered anime file is S04E07, and the
    /// name does not say so. See [`crate::relink`].
    pub relinked: usize,
    /// Files whose translated path brarr cannot see.
    pub missing: usize,
    /// Files no mapping covers *and* that are not where the \*arr says.
    /// Split from `missing` because the fix is different: this one is a
    /// root mapping the operator has not made.
    pub unmapped: usize,
}

impl FileCounts {
    fn merge(&mut self, other: Self) {
        self.adopted += other.adopted;
        self.already += other.already;
        self.relinked += other.relinked;
        self.missing += other.missing;
        self.unmapped += other.unmapped;
    }
}

/// What a run did.
#[derive(Debug, Clone, Default)]
pub struct ImportReport {
    /// Titles added to the library.
    pub added: usize,
    /// Titles already there, whose metadata was refreshed.
    pub refreshed: usize,
    /// Titles nothing could be done with.
    pub blocked: usize,
    /// Files, across every title.
    pub files: FileCounts,
    /// Per-title failures, capped at [`MAX_FAILURES_KEPT`].
    pub failures: Vec<String>,
    /// How many failed in total, cap included.
    pub failed: usize,
}

impl ImportReport {
    fn fail(&mut self, title: &str, why: &str) {
        self.failed += 1;
        if self.failures.len() < MAX_FAILURES_KEPT {
            self.failures.push(format!("{title}: {why}"));
        }
    }
}

/// Import one instance's catalogue with the operator's choices.
///
/// # Errors
///
/// [`AppError::NotFound`] when the instance is gone,
/// [`AppError::InvalidInput`] when TMDB is not configured or the \*arr
/// cannot be read, [`AppError::Database`] on SQL failure. A failure on
/// one *title* is recorded in the report and the run continues — one bad
/// id must not abandon 467 good ones.
pub async fn run(
    state: &AppState,
    instance_id: Uuid,
    options: ImportOptions,
) -> Result<ImportReport, AppError> {
    let row = arr_instances::get_by_id(state.pool(), instance_id).await?;
    run_instance(state, &row, options).await
}

/// Read one instance as a passive source.
///
/// **Takes no monitoring parameter, and that is the design.** The
/// operator's colleagues request through Seerr, Seerr adds to
/// Sonarr/Radarr, and brarr records the wish — paused. Those \*arr still
/// have indexers and download clients pointed at the same clients brarr
/// uses, so a synced title arriving monitored would put two agents on it.
/// The profile still comes from the instance, so an anime lands under the
/// anime rules the day the operator does turn it on.
///
/// # Errors
///
/// Same as [`run`].
pub async fn sync_one(state: &AppState, row: &ArrInstanceRow) -> Result<ImportReport, AppError> {
    let report = run_instance(
        state,
        row,
        ImportOptions {
            monitoring: MonitorChoice::Paused,
            profile_id: None,
        },
    )
    .await?;
    arr_instances::mark_synced(state.pool(), row.id, OffsetDateTime::now_utc()).await?;
    Ok(report)
}

async fn run_instance(
    state: &AppState,
    row: &ArrInstanceRow,
    options: ImportOptions,
) -> Result<ImportReport, AppError> {
    let pool = state.pool();
    let tmdb = tmdb_sync::client(pool).await?;
    let mappings = arr_root_mappings::for_instance(pool, row.id).await?;
    let rules = arr_root_mappings::rules(&mappings);
    let profile = options.profile_id.or(row.profile_id);

    let client = client_for(row)?;
    let titles = read_catalogue(&client)
        .await
        .map_err(|e| AppError::InvalidInput(format!("{}: {e}", row.name)))?;
    info!(
        target: "brarr_orchestrator::arr_import",
        instance = %row.name,
        titles = titles.len(),
        monitoring = options.monitoring.label(),
        "importing an *arr catalogue"
    );

    let ctx = TitleContext {
        tmdb: &tmdb,
        client: &client,
        rules: &rules,
        mappings: &mappings,
        monitoring: options.monitoring,
        profile,
    };
    let mut report = ImportReport::default();
    for title in &titles {
        match import_title(state, title, &ctx).await {
            Ok(outcome) => outcome.fold_into(&mut report),
            Err(e) => report.fail(&title.title, &e.to_string()),
        }
    }
    info!(
        target: "brarr_orchestrator::arr_import",
        instance = %row.name,
        added = report.added,
        refreshed = report.refreshed,
        adopted = report.files.adopted,
        missing = report.files.missing,
        failed = report.failed,
        "import finished"
    );
    Ok(report)
}

/// Everything one title needs that does not come from the \*arr row.
///
/// A struct rather than six parameters because `clippy.toml` pins
/// `too-many-arguments-threshold = 6`.
struct TitleContext<'a> {
    tmdb: &'a TmdbClient,
    client: &'a ArrClient,
    rules: &'a [PrefixRule],
    mappings: &'a [ArrRootMapping],
    monitoring: MonitorChoice,
    profile: Option<Uuid>,
}

/// How one title ended up.
enum TitleOutcome {
    /// Nothing to do, and why.
    Blocked,
    /// Catalogued, with its files.
    Done { created: bool, files: FileCounts },
}

impl TitleOutcome {
    fn fold_into(self, report: &mut ImportReport) {
        match self {
            Self::Blocked => report.blocked += 1,
            Self::Done { created, files } => {
                if created {
                    report.added += 1;
                } else {
                    report.refreshed += 1;
                }
                report.files.merge(files);
            }
        }
    }
}

async fn import_title(
    state: &AppState,
    title: &ArrTitle,
    ctx: &TitleContext<'_>,
) -> Result<TitleOutcome, AppError> {
    let pool = state.pool();
    let Some(tmdb_id) = resolve_tmdb_id(ctx, title).await? else {
        return Ok(TitleOutcome::Blocked);
    };
    let created = library::get_by_tmdb(pool, title.media_type, tmdb_id)
        .await
        .is_err();

    // The scope has to be stored before the tree is built, and the row
    // has to exist before it can carry a scope — so the first upsert
    // comes first, then the scope, then the tree. `upsert` preserves
    // operator state on conflict, so this is safe for a known title.
    let item = upsert_metadata(pool, ctx.tmdb, title.media_type, tmdb_id).await?;
    if created {
        library::set_monitor_scope(pool, item.id, ctx.monitoring.scope()).await?;
        place(pool, &item, title, ctx).await?;
    }

    let detail = match title.media_type {
        MediaType::Movie => SeriesDetail::default(),
        MediaType::Tv => {
            sync_tree(pool, ctx.tmdb, &item, tmdb_id).await?;
            series_detail(ctx.client, title.arr_id)
                .await
                .map_err(|e| AppError::InvalidInput(e.to_string()))?
        }
    };
    if created && ctx.monitoring == MonitorChoice::Mirror {
        mirror_monitoring(pool, &item, title, &detail).await?;
    }
    if title.media_type == MediaType::Tv {
        // After the tree, because it reads it; before the files, because
        // a numbering is what makes the *next* sweep able to search for
        // what the files do not cover.
        sync_search_numbering(pool, &item, &detail).await?;
    }

    let files = if title.media_type == MediaType::Tv {
        &detail.files
    } else {
        &title.files
    };
    Ok(TitleOutcome::Done {
        created,
        files: adopt_files(pool, &item, files, ctx.rules).await?,
    })
}

/// The TMDB id to catalogue under.
///
/// The \*arr's own is the common path — Radarr is keyed on it and Sonarr
/// v3 carries it. A zero means the title was never linked, and for a
/// series the TVDb id is a way back: one `find` call, and only for the
/// rows that need it.
async fn resolve_tmdb_id(
    ctx: &TitleContext<'_>,
    title: &ArrTitle,
) -> Result<Option<i64>, AppError> {
    if title.tmdb_id > 0 {
        return Ok(Some(title.tmdb_id));
    }
    if title.media_type != MediaType::Tv || title.tvdb_id <= 0 {
        return Ok(None);
    }
    let found = ctx.tmdb.find_by_tvdb(title.tvdb_id).await?;
    Ok(found.series.first().map(|s| s.tmdb_id))
}

/// Insert or refresh the catalogue row from TMDB.
async fn upsert_metadata(
    pool: &Pool,
    tmdb: &TmdbClient,
    media_type: MediaType,
    tmdb_id: i64,
) -> Result<LibraryItem, AppError> {
    match media_type {
        MediaType::Movie => {
            let details = tmdb.movie(tmdb_id).await?;
            Ok(library::upsert(pool, &tmdb_sync::movie_to_item(&details)).await?)
        }
        MediaType::Tv => {
            let details = tmdb.tv(tmdb_id).await?;
            Ok(library::upsert(pool, &tmdb_sync::tv_to_item(&details)).await?)
        }
    }
}

/// Rebuild a series' season tree from TMDB.
async fn sync_tree(
    pool: &Pool,
    tmdb: &TmdbClient,
    item: &LibraryItem,
    tmdb_id: i64,
) -> Result<(), AppError> {
    let details = tmdb.tv(tmdb_id).await?;
    let mut seasons = Vec::with_capacity(details.seasons.len());
    for summary in &details.seasons {
        let season = tmdb.season(tmdb_id, summary.season_number).await?;
        seasons.push(tmdb_sync::season_to_new(&season));
    }
    library::sync_seasons(pool, item.id, &seasons).await
}

/// Give a created title its profile and its root folder.
///
/// The root is the one the \*arr's own root maps to, which is the whole
/// point of the mapping: the title stays exactly where it is, and brarr
/// knows where "there" is. Only ever called for a created row —
/// [`library::set_placement`] writes both columns unconditionally, so
/// calling it for a known title would erase a hand-set profile.
async fn place(
    pool: &Pool,
    item: &LibraryItem,
    title: &ArrTitle,
    ctx: &TitleContext<'_>,
) -> Result<(), AppError> {
    let root = ctx
        .mappings
        .iter()
        .find(|m| m.arr_path == title.root_folder_path)
        .map(|m| m.root_path.to_string_lossy().into_owned());
    if ctx.profile.is_none() && root.is_none() {
        return Ok(());
    }
    library::set_placement(pool, item.id, ctx.profile, root.as_deref()).await
}

/// Copy the \*arr's monitoring onto a created title.
///
/// Season first, then episode: [`library::set_season_monitored`] cascades
/// to its episodes, so doing it the other way round would erase the
/// per-episode flags it just wrote. Only differing episodes are touched,
/// which on a fully-monitored series is no writes at all rather than one
/// per episode.
async fn mirror_monitoring(
    pool: &Pool,
    item: &LibraryItem,
    title: &ArrTitle,
    detail: &SeriesDetail,
) -> Result<(), AppError> {
    library::set_monitored(pool, item.id, title.monitored).await?;
    if item.media_type != MediaType::Tv {
        return Ok(());
    }

    let wanted_seasons: HashMap<i32, bool> = title.seasons.iter().copied().collect();
    for season in library::seasons(pool, item.id).await? {
        let Some(&flag) = wanted_seasons.get(&season.season_number) else {
            continue;
        };
        if flag != season.monitored {
            library::set_season_monitored(pool, season.id, flag).await?;
        }
    }

    let wanted: HashMap<(i32, i32), bool> = detail
        .monitoring
        .iter()
        .map(|(s, e, m)| ((*s, *e), *m))
        .collect();
    for episode in library::episodes(pool, item.id).await? {
        let key = (episode.season_number, episode.episode_number);
        if let Some(&flag) = wanted.get(&key)
            && flag != episode.monitored
        {
            library::set_episode_monitored(pool, episode.id, flag).await?;
        }
    }
    Ok(())
}

/// Record every file the \*arr already matched, where it stands.
///
/// **Nothing is written to disk.** The file is in the library already —
/// that is what the root mapping asserts — so the reservation is taken
/// and immediately marked imported at the same path, which is exactly the
/// shape `adopt` gives an in-place adoption and what makes undo a single
/// row delete.
async fn adopt_files(
    pool: &Pool,
    item: &LibraryItem,
    files: &[ArrFileRef],
    rules: &[PrefixRule],
) -> Result<FileCounts, AppError> {
    if files.is_empty() {
        return Ok(FileCounts::default());
    }
    let matcher = if item.media_type == MediaType::Tv {
        let episodes = library::episodes(pool, item.id).await?;
        let reverse = episode_numbering::reverse_for_item(pool, item.id).await?;
        EpisodeMatcher::new(&episodes, reverse)
    } else {
        EpisodeMatcher::default()
    };

    let mut counts = FileCounts::default();
    for file in files {
        let local = locate(file, rules).await;
        if !local.present {
            // Fail-open first: an install where brarr and the *arr see
            // the same filesystem needs no mapping and must keep working,
            // so "not there" is only *about* the mapping when no rule
            // fired. The two get different counters because the fix is
            // different — one is a rule to write, the other a file that
            // moved or a rule that is wrong.
            if local.mapped {
                counts.missing += 1;
            } else {
                counts.unmapped += 1;
            }
            continue;
        }
        let mut episode_id = None;
        if let Some((season, episode)) = file.episode {
            // TMDB does not always number a series the way Sonarr does —
            // they disagree on 29 of this operator's 176 series, and on
            // anime almost always. `EpisodeMatcher` tries the canonical
            // coordinate, then the absolute axis, then whatever ordering
            // the operator applied. A file none of them place is still
            // dropped: recording it against the item instead would make
            // one file look like the whole series to
            // `grabs::blocking_for`.
            let Some(found) = matcher.resolve(season, episode, file.absolute) else {
                counts.missing += 1;
                continue;
            };
            episode_id = Some(found);
        }
        match record(pool, item.id, episode_id, &local.path, &file.path).await? {
            Recorded::Adopted => counts.adopted += 1,
            Recorded::Already => counts.already += 1,
            Recorded::Relinked => counts.relinked += 1,
        }
    }
    Ok(counts)
}

/// What [`record`] concluded about one file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Recorded {
    /// A new in-place adoption.
    Adopted,
    /// Already recorded, and already correct.
    Already,
    /// Already recorded, but the row had lost its episode. Repaired from
    /// the pairing the \*arr had done.
    Relinked,
}

/// Reserve, then mark imported.
///
/// The barrier refusing is not always "nothing to do": `idx_grabs_unique_local`
/// is keyed on `(item_id, release_id_remote)` — the file path — so a row
/// whose `episode_id` a metadata refresh nulled refuses the re-adoption
/// that would have fixed it, and the run counts it `already` forever.
/// **This is the only place in brarr that can repair those**: the file
/// name of an absolute-numbered anime says nothing, and Sonarr has
/// already paired it. `relink_episode` only ever fills a blank, so a
/// grab pointing somewhere is never moved.
async fn record(
    pool: &Pool,
    item_id: Uuid,
    episode_id: Option<Uuid>,
    local: &std::path::Path,
    reported: &str,
) -> Result<Recorded, AppError> {
    let path = local.to_string_lossy().into_owned();
    let name = local
        .file_name()
        .map_or_else(|| reported.to_owned(), |n| n.to_string_lossy().into_owned());
    let reserved = grabs::reserve_local(
        pool,
        &grabs::LocalGrab {
            item_id,
            episode_id,
            source_path: &path,
            release_name: &name,
        },
    )
    .await?;
    let Some(grab) = reserved else {
        return repair(pool, item_id, episode_id, &path).await;
    };
    // Source and destination are the same path, which is what
    // `grabs::is_in_place` reads to know undo has nothing to remove.
    grabs::mark_imported(pool, grab.id, &path).await?;
    debug!(
        target: "brarr_orchestrator::arr_import",
        %item_id, path = %path, "adopted a file the *arr already had"
    );
    Ok(Recorded::Adopted)
}

/// The barrier refused: decide whether the stored row is fine or is an
/// orphan this run can put back.
async fn repair(
    pool: &Pool,
    item_id: Uuid,
    episode_id: Option<Uuid>,
    path: &str,
) -> Result<Recorded, AppError> {
    let (Some(episode_id), Some(stored)) =
        (episode_id, grabs::local_by_path(pool, item_id, path).await?)
    else {
        return Ok(Recorded::Already);
    };
    if stored.episode_id.is_some() {
        return Ok(Recorded::Already);
    }
    match grabs::relink_episode(pool, stored.id, episode_id).await? {
        grabs::Relink::Linked => {
            debug!(
                target: "brarr_orchestrator::arr_import",
                %item_id, path, "re-pointed an orphaned file at the episode the *arr paired it with"
            );
            Ok(Recorded::Relinked)
        }
        grabs::Relink::AlreadyLinked | grabs::Relink::Occupied => Ok(Recorded::Already),
    }
}

/// One file's path after translation.
struct LocalFile {
    /// Where to look on this machine.
    path: PathBuf,
    /// A mapping actually fired. When it did not, the path came through
    /// untouched — which is right for an install where brarr and the
    /// \*arr see the same filesystem, and is a missing mapping otherwise.
    mapped: bool,
    /// brarr can see a file there.
    present: bool,
}

/// Translate one path and look for it.
///
/// A path in a namespace this machine cannot open — a POSIX path on a
/// Windows brarr, say — is `present: false` **without a syscall**. It is
/// certain to fail, and saying so before asking the filesystem is what
/// turns "does not exist" into "no mapping covers this".
///
/// `tokio::fs` rather than `std`: the stat runs on the blocking pool, so
/// a network mount that stops answering costs a pool thread rather than a
/// runtime worker.
async fn locate(file: &ArrFileRef, rules: &[PrefixRule]) -> LocalFile {
    let translated = remote_path::translate(rules, &file.path);
    let mapped = translated.applied.is_some();
    if !remote_path::is_usable(&translated) {
        return LocalFile {
            path: translated.local,
            mapped,
            present: false,
        };
    }
    let present = tokio::fs::metadata(&translated.local)
        .await
        .is_ok_and(|m| m.is_file());
    LocalFile {
        path: translated.local,
        mapped,
        present,
    }
}

/// Default cadence of the passive sweep.
///
/// Half an hour, and it could be longer: the wish list only changes when
/// a colleague asks Seerr for something, and nothing downstream is
/// waiting on it — the titles land paused. Reading three catalogues is
/// cheap, but every *new* title costs a TMDB call, so a tight loop buys
/// nothing and spends someone else's rate limit.
pub const DEFAULT_SYNC_INTERVAL: Duration = Duration::from_secs(30 * 60);

/// Floor for the configured cadence. A one-second sweep would hammer
/// both the \*arr and TMDB for a list that changes a few times a day.
const MIN_SYNC_INTERVAL: Duration = Duration::from_secs(60);

/// Delay before the first sweep, so it does not pile onto the startup
/// burst. Deliberately clear of the other tasks' openings — the poller
/// starts immediately, the scanner at 90s, the importer at 120s.
const STARTUP_DELAY: Duration = Duration::from_secs(150);

/// Spawn the passive sweep.
///
/// Returns the [`JoinHandle`] so the caller can keep it alive — dropping
/// it aborts the task.
#[must_use]
pub fn spawn(state: AppState) -> JoinHandle<()> {
    let state = Arc::new(state);
    info!(
        target: "brarr_orchestrator::arr_import",
        default_interval_secs = DEFAULT_SYNC_INTERVAL.as_secs(),
        "starting the passive *arr sweep (cadence is hot-reloadable via /settings)"
    );
    tokio::spawn(async move {
        sleep(STARTUP_DELAY).await;
        loop {
            run_cycle(&state).await;
            sleep(configured_interval(state.pool()).await).await;
        }
    })
}

/// The cadence as configured, floored. Read fresh every cycle so an edit
/// in `/settings` lands on the next tick rather than at the next restart
/// — the same hot-reload contract the poller and the maintenance task
/// have.
async fn configured_interval(pool: &Pool) -> Duration {
    let stored =
        match crate::db::settings::get(pool, crate::db::settings::KEY_ARR_SYNC_INTERVAL_SECS).await
        {
            Ok(Some(row)) => row.value.trim().parse::<u64>().ok(),
            // A blank, missing or unreadable setting is the default, not a
            // stall: this task must keep running while the DB hiccups.
            _ => None,
        };
    stored.map_or(DEFAULT_SYNC_INTERVAL, |secs| {
        Duration::from_secs(secs).max(MIN_SYNC_INTERVAL)
    })
}

/// One sweep. Errors are logged, never propagated — a transient failure
/// must not kill the long-lived task.
async fn run_cycle(state: &AppState) {
    // Checked before the instance list so an install with no TMDB
    // credential logs one debug line instead of an error per instance
    // every half hour.
    match tmdb_sync::load_config(state.pool()).await {
        Ok(cfg) if !cfg.is_configured() => {
            debug!(
                target: "brarr_orchestrator::arr_import",
                "TMDB not configured; skipping the passive sweep"
            );
            return;
        }
        Ok(_) => {}
        Err(e) => {
            warn!(target: "brarr_orchestrator::arr_import", error = %e, "could not read the TMDB config");
            return;
        }
    }
    match sync_all(state).await {
        Ok(report) if report.added > 0 || report.files.adopted > 0 || report.failed > 0 => info!(
            target: "brarr_orchestrator::arr_import",
            added = report.added,
            refreshed = report.refreshed,
            adopted = report.files.adopted,
            failed = report.failed,
            "passive sweep complete"
        ),
        Ok(_) => {
            debug!(target: "brarr_orchestrator::arr_import", "passive sweep found nothing new");
        }
        Err(e) => {
            warn!(target: "brarr_orchestrator::arr_import", error = %e, "passive sweep failed");
        }
    }
}

/// Every instance marked as a sync source, read in turn.
///
/// Sequential on purpose: three instances against one TMDB credential,
/// and a burst of parallel metadata calls buys seconds while looking
/// exactly like abuse.
///
/// # Errors
///
/// Returns [`AppError::Database`] when the instance list cannot be read.
/// A failure on one *instance* is logged and the sweep continues.
pub async fn sync_all(state: &AppState) -> Result<ImportReport, AppError> {
    let sources = arr_instances::list_sync_sources(state.pool()).await?;
    let mut total = ImportReport::default();
    let mut seen: HashSet<Uuid> = HashSet::new();
    for row in sources {
        if !seen.insert(row.id) {
            continue;
        }
        match sync_one(state, &row).await {
            Ok(report) => {
                total.added += report.added;
                total.refreshed += report.refreshed;
                total.blocked += report.blocked;
                total.failed += report.failed;
                total.files.merge(report.files);
            }
            Err(e) => {
                warn!(
                    target: "brarr_orchestrator::arr_import",
                    instance = %row.name,
                    error = %e,
                    "passive sync failed"
                );
                total.fail(&row.name, &e.to_string());
            }
        }
    }
    Ok(total)
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "tests assert on happy paths"
)]
mod tests {
    use super::*;
    use crate::db::library::{NewEpisode, NewLibraryItem, NewSeason};
    use crate::db::open_memory;

    fn file(id: u64, path: &str) -> ArrFile {
        ArrFile {
            id,
            path: path.to_owned(),
            size: 1024,
        }
    }

    fn episode(id: u64, season: i32, number: i32, file_id: u64) -> ArrEpisode {
        ArrEpisode {
            id,
            season_number: season,
            episode_number: number,
            monitored: true,
            has_file: file_id > 0,
            episode_file_id: file_id,
            absolute_episode_number: None,
        }
    }

    /// The whole reason to import from Sonarr: it already decided which
    /// file is which episode.
    ///
    /// Taken verbatim from the operator's `sonarr-animes` on 2026-08-07 —
    /// 224 files, 224 paired, not one `SxxEyy` marker among them. A name
    /// like this is what `adopt::parse_marker` refuses (correctly: it
    /// would have to guess), and what Sonarr answers without guessing.
    #[test]
    fn sonarr_pairs_an_absolute_numbered_file_with_its_episode() {
        const PATH: &str = "/data/Animes/Yu-Gi-Oh! Duel Monsters/Season 01/\
             Yu-Gi-Oh! Duel Monsters - 001 - O Terrivel Blue-Eyes White Dragon.mkv";
        let paired = join_episode_files(&[episode(1, 1, 1, 7)], &[file(7, PATH)]);
        assert_eq!(paired.len(), 1);
        assert_eq!(paired[0].episode, Some((1, 1)));
        assert_eq!(paired[0].path, PATH);
        // The name brarr would have had to read on its own.
        assert!(
            crate::adopt::parse_marker(PATH).is_err(),
            "if a marker parser could read this, the *arr import would be a convenience \
             rather than the only way to get these 545 files right"
        );
    }

    #[test]
    fn an_episode_without_a_file_is_not_a_pairing() {
        let paired = join_episode_files(&[episode(1, 1, 1, 0)], &[]);
        assert!(paired.is_empty());
    }

    /// Season 0 is TMDB's specials bucket, excluded by the scanner, the
    /// episode picker and the tree summary. Letting it in here would
    /// adopt files against episodes nothing else will ever look at.
    #[test]
    fn specials_are_excluded() {
        let files = vec![file(3, "/data/Series/X/Especial.mkv")];
        let episodes = vec![episode(5, 0, 1, 3)];
        assert!(join_episode_files(&episodes, &files).is_empty());
        assert!(episode_monitoring(&episodes).is_empty());
    }

    /// A file covering two episodes appears twice on purpose, and since
    /// `20260813120000` both rows survive the barrier — a 40-minute
    /// `S05E33E34` really is two episodes.
    #[test]
    fn a_multi_episode_file_appears_once_per_episode() {
        let files = vec![file(9, "/data/Series/X/S01E01E02.mkv")];
        let episodes = vec![episode(1, 1, 1, 9), episode(2, 1, 2, 9)];
        let paired = join_episode_files(&episodes, &files);
        assert_eq!(paired.len(), 2);
        assert_eq!(paired[0].path, paired[1].path);
    }

    #[test]
    fn a_radarr_row_carries_its_file_inline() {
        let movie = ArrMovie {
            id: 12,
            title: "Duna: Parte Dois".to_owned(),
            year: 2024,
            tmdb_id: 693_134,
            imdb_id: "tt15239678".to_owned(),
            monitored: true,
            has_file: true,
            path: "/data/Filmes/Duna Parte Dois (2024)".to_owned(),
            root_folder_path: "/data/Filmes".to_owned(),
            movie_file: Some(file(4, "/data/Filmes/Duna Parte Dois (2024)/duna.mkv")),
        };
        let title = movie_to_title(&movie);
        assert_eq!(title.media_type, MediaType::Movie);
        assert_eq!(title.tmdb_id, 693_134);
        assert_eq!(title.year, Some(2024));
        assert_eq!(title.files.len(), 1);
        assert_eq!(title.files[0].episode, None);
    }

    /// A zero year is Radarr's "I do not know", not the year zero.
    #[test]
    fn a_zero_year_is_absent_rather_than_zero() {
        let movie = ArrMovie {
            id: 1,
            title: "x".to_owned(),
            year: 0,
            tmdb_id: 1,
            imdb_id: String::new(),
            monitored: false,
            has_file: false,
            path: String::new(),
            root_folder_path: String::new(),
            movie_file: None,
        };
        assert_eq!(movie_to_title(&movie).year, None);
    }

    #[test]
    fn monitoring_choice_defaults_to_paused_on_anything_unknown() {
        // An unrecognised form value must never become "chase 468 titles
        // while three *arr are still chasing them too".
        assert_eq!(MonitorChoice::from_label("mirror"), MonitorChoice::Mirror);
        assert_eq!(MonitorChoice::from_label("paused"), MonitorChoice::Paused);
        assert_eq!(MonitorChoice::from_label("sim"), MonitorChoice::Paused);
        assert_eq!(MonitorChoice::from_label(""), MonitorChoice::Paused);
        assert_eq!(MonitorChoice::default(), MonitorChoice::Paused);
    }

    #[test]
    fn paused_creates_an_unmonitored_tree() {
        assert_eq!(MonitorChoice::Paused.scope(), MonitorScope::Nothing);
        assert!(!MonitorScope::Nothing.monitors_item());
        assert!(!MonitorScope::Nothing.wants_new_row(1, 1, true));
    }

    async fn seed_series(pool: &Pool) -> LibraryItem {
        let item = library::upsert(
            pool,
            &NewLibraryItem {
                media_type: Some(MediaType::Tv),
                tmdb_id: 76_479,
                title: "The Boys".to_owned(),
                ..NewLibraryItem::default()
            },
        )
        .await
        .unwrap();
        library::sync_seasons(
            pool,
            item.id,
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
        item
    }

    #[tokio::test]
    async fn mirror_copies_the_arr_flags_down_the_tree() {
        let pool = open_memory().await.unwrap();
        let item = seed_series(&pool).await;
        let title = ArrTitle {
            arr_id: 1,
            media_type: MediaType::Tv,
            tmdb_id: 76_479,
            tvdb_id: 0,
            title: "The Boys".to_owned(),
            year: None,
            monitored: true,
            path: String::new(),
            root_folder_path: String::new(),
            seasons: vec![(1, true)],
            files: Vec::new(),
        };
        let detail = SeriesDetail {
            files: Vec::new(),
            // Sonarr has episode 2 switched off; brarr must end up
            // agreeing with it.
            monitoring: vec![(1, 1, true), (1, 2, false)],
            numbering: Vec::new(),
        };
        mirror_monitoring(&pool, &item, &title, &detail)
            .await
            .unwrap();

        let episodes = library::episodes(&pool, item.id).await.unwrap();
        let flags: Vec<bool> = episodes.iter().map(|e| e.monitored).collect();
        assert_eq!(flags, vec![true, false]);
    }

    /// The season cascade wipes per-episode flags, so it has to run
    /// first. This is the test that fails if the order is swapped.
    #[tokio::test]
    async fn an_unmonitored_season_takes_its_episodes_with_it() {
        let pool = open_memory().await.unwrap();
        let item = seed_series(&pool).await;
        let title = ArrTitle {
            arr_id: 1,
            media_type: MediaType::Tv,
            tmdb_id: 76_479,
            tvdb_id: 0,
            title: "The Boys".to_owned(),
            year: None,
            monitored: true,
            path: String::new(),
            root_folder_path: String::new(),
            seasons: vec![(1, false)],
            files: Vec::new(),
        };
        let detail = SeriesDetail {
            files: Vec::new(),
            monitoring: vec![(1, 1, false), (1, 2, false)],
            numbering: Vec::new(),
        };
        mirror_monitoring(&pool, &item, &title, &detail)
            .await
            .unwrap();

        let episodes = library::episodes(&pool, item.id).await.unwrap();
        assert!(episodes.iter().all(|e| !e.monitored));
    }

    #[tokio::test]
    async fn the_sweep_cadence_defaults_and_floors() {
        use crate::db::settings;
        let pool = open_memory().await.unwrap();
        assert_eq!(
            configured_interval(&pool).await,
            DEFAULT_SYNC_INTERVAL,
            "nothing stored means the default"
        );

        settings::set(&pool, settings::KEY_ARR_SYNC_INTERVAL_SECS, "900")
            .await
            .unwrap();
        assert_eq!(configured_interval(&pool).await, Duration::from_secs(900));

        // A blanked setting reads as "use the default", the same contract
        // every other hot-reloadable value has.
        settings::set(&pool, settings::KEY_ARR_SYNC_INTERVAL_SECS, "")
            .await
            .unwrap();
        assert_eq!(configured_interval(&pool).await, DEFAULT_SYNC_INTERVAL);

        // A one-second sweep would hammer both the *arr and TMDB for a
        // list that changes a few times a day.
        settings::set(&pool, settings::KEY_ARR_SYNC_INTERVAL_SECS, "1")
            .await
            .unwrap();
        assert_eq!(configured_interval(&pool).await, MIN_SYNC_INTERVAL);
    }

    #[tokio::test]
    async fn a_file_the_arr_reports_is_recorded_where_it_stands() {
        let pool = open_memory().await.unwrap();
        let item = seed_series(&pool).await;
        let dir = std::env::temp_dir().join(format!("brarr-arrimport-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let on_disk = dir.join("The Boys - S01E01.mkv");
        std::fs::write(&on_disk, b"x").unwrap();

        // Sonarr's namespace on the left, brarr's on the right — the
        // production shape.
        let rules = vec![PrefixRule {
            id: Uuid::nil(),
            remote_prefix: "/data/Series".to_owned(),
            local_prefix: dir.clone(),
        }];
        let files = vec![ArrFileRef {
            path: "/data/Series/The Boys - S01E01.mkv".to_owned(),
            episode: Some((1, 1)),
            absolute: None,
        }];

        let counts = adopt_files(&pool, &item, &files, &rules).await.unwrap();
        assert_eq!(counts.adopted, 1, "{counts:?}");

        let live = grabs::live_for_item(&pool, item.id).await.unwrap();
        assert_eq!(live.len(), 1);
        assert!(
            grabs::is_in_place(&live[0]),
            "an *arr import never writes, so source and destination are one path"
        );
        assert_eq!(live[0].imported_path.as_deref(), on_disk.to_str());

        // Idempotent: running it again records nothing new, which is what
        // makes the passive sweep safe on a timer.
        let again = adopt_files(&pool, &item, &files, &rules).await.unwrap();
        assert_eq!(again.adopted, 0);
        assert_eq!(again.already, 1);

        std::fs::remove_dir_all(&dir).ok();
    }

    /// The failure mode the root mapping exists to prevent: brarr looks
    /// where the *arr said, finds nothing, and must say so instead of
    /// recording a library it cannot see.
    #[tokio::test]
    async fn a_file_brarr_cannot_see_is_reported_not_recorded() {
        let pool = open_memory().await.unwrap();
        let item = seed_series(&pool).await;
        let files = vec![ArrFileRef {
            path: "/data/Series/The Boys - S01E01.mkv".to_owned(),
            episode: Some((1, 1)),
            absolute: None,
        }];
        let counts = adopt_files(&pool, &item, &files, &[]).await.unwrap();
        assert_eq!(counts.adopted, 0);
        assert_eq!(
            counts.unmapped, 1,
            "no rule covered it, so the fix is a mapping — not a lost file"
        );
        assert!(
            grabs::live_for_item(&pool, item.id)
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn an_episode_the_catalogue_does_not_have_is_not_recorded() {
        let pool = open_memory().await.unwrap();
        let item = seed_series(&pool).await;
        let dir = std::env::temp_dir().join(format!("brarr-arrimport-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("x.mkv"), b"x").unwrap();
        let rules = vec![PrefixRule {
            id: Uuid::nil(),
            remote_prefix: "/data/Series".to_owned(),
            local_prefix: dir.clone(),
        }];
        // TMDB has two episodes; Sonarr claims a ninth.
        let files = vec![ArrFileRef {
            path: "/data/Series/x.mkv".to_owned(),
            episode: Some((1, 9)),
            absolute: None,
        }];
        let counts = adopt_files(&pool, &item, &files, &rules).await.unwrap();
        assert_eq!(counts.adopted, 0);
        assert_eq!(counts.missing, 1);
        assert!(
            grabs::live_for_item(&pool, item.id)
                .await
                .unwrap()
                .is_empty()
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    /// **The Dragon Ball Super regression.** TMDB flattens the series
    /// into one season of 131; `TheTVDB`, Sonarr and the operator's disk
    /// split the same episodes into five arcs. Sonarr reports the file as
    /// S02E01, TMDB calls it episode 15, and before
    /// [`crate::episode_match`] this fell straight through to
    /// `counts.missing` — 117 of the 131 files, present on disk, recorded
    /// as absent.
    ///
    /// Run against the previous code this fails with `missing: 1`.
    #[tokio::test]
    async fn a_file_the_arr_numbers_differently_still_finds_its_episode() {
        let pool = open_memory().await.unwrap();
        let item = seed_flat_series(&pool, 131).await;
        let dir = std::env::temp_dir().join(format!("brarr-arrimport-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("dbs-s02e01.mkv"), b"x").unwrap();
        let rules = vec![PrefixRule {
            id: Uuid::nil(),
            remote_prefix: "/data/Animes".to_owned(),
            local_prefix: dir.clone(),
        }];
        let files = vec![ArrFileRef {
            path: "/data/Animes/dbs-s02e01.mkv".to_owned(),
            episode: Some((2, 1)),
            // Arc 2 episode 1 is the fifteenth of the series, and Sonarr
            // says so on every anime episode it holds.
            absolute: Some(15),
        }];

        let counts = adopt_files(&pool, &item, &files, &rules).await.unwrap();
        assert_eq!(counts.adopted, 1, "{counts:?}");

        let live = grabs::live_for_item(&pool, item.id).await.unwrap();
        assert_eq!(live.len(), 1);
        let episodes = library::episodes(&pool, item.id).await.unwrap();
        let fifteenth = episodes
            .iter()
            .find(|e| e.season_number == 1 && e.episode_number == 15)
            .unwrap();
        assert_eq!(
            live[0].episode_id,
            Some(fifteenth.id),
            "S02E01 is the catalogue's episode 15, not its episode 1"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    /// One 40-minute file, two episodes — `S05E33E34` on this operator's
    /// disk, `S33E06E07` on their Simpsons. The local barrier used to be
    /// keyed on the path alone, so the second episode stayed uncovered
    /// forever and the scanner could never close the gap: every release
    /// it found was refused by the same key.
    #[tokio::test]
    async fn one_file_can_cover_two_episodes() {
        let pool = open_memory().await.unwrap();
        let item = seed_flat_series(&pool, 4).await;
        let dir = std::env::temp_dir().join(format!("brarr-arrimport-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("double.mkv"), b"x").unwrap();
        let rules = vec![PrefixRule {
            id: Uuid::nil(),
            remote_prefix: "/data/Animes".to_owned(),
            local_prefix: dir.clone(),
        }];
        // What `join_episode_files` emits for a two-episode file: the same
        // path once per episode.
        let files = vec![
            ArrFileRef {
                path: "/data/Animes/double.mkv".to_owned(),
                episode: Some((1, 2)),
                absolute: None,
            },
            ArrFileRef {
                path: "/data/Animes/double.mkv".to_owned(),
                episode: Some((1, 3)),
                absolute: None,
            },
        ];

        let counts = adopt_files(&pool, &item, &files, &rules).await.unwrap();
        assert_eq!(counts.adopted, 2, "{counts:?}");

        let live = grabs::live_for_item(&pool, item.id).await.unwrap();
        let covered: HashSet<Option<Uuid>> = live.iter().map(|g| g.episode_id).collect();
        assert_eq!(
            covered.len(),
            2,
            "both episodes are covered by the one file"
        );

        // Still idempotent — the key gained the episode, it did not stop
        // being a key.
        let again = adopt_files(&pool, &item, &files, &rules).await.unwrap();
        assert_eq!(again.adopted, 0);
        assert_eq!(again.already, 2);

        std::fs::remove_dir_all(&dir).ok();
    }

    /// A series TMDB flattens into a single season of `count` episodes —
    /// Dragon Ball Super's real shape, and Jujutsu Kaisen's, and that of
    /// thirteen more titles in this operator's catalogue.
    async fn seed_flat_series(pool: &Pool, count: i32) -> LibraryItem {
        let item = library::upsert(
            pool,
            &NewLibraryItem {
                media_type: Some(MediaType::Tv),
                tmdb_id: 62_715,
                title: "Dragon Ball Super".to_owned(),
                ..NewLibraryItem::default()
            },
        )
        .await
        .unwrap();
        library::sync_seasons(
            pool,
            item.id,
            &[NewSeason {
                season_number: 1,
                episode_count: count,
                air_date: None,
                episodes: (1..=count)
                    .map(|episode_number| NewEpisode {
                        tmdb_episode_id: None,
                        episode_number,
                        title: None,
                        air_date: None,
                    })
                    .collect(),
            }],
        )
        .await
        .unwrap();
        item
    }
}
