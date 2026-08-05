//! The monitored sweep — brarr looking for what it does not have yet.
//!
//! This is the half of Radarr/Sonarr that decides *when* to search.
//! [`crate::poll`] asked an \*arr what was missing; this asks brarr's own
//! library, which is what makes brarr the only agent in the loop.
//!
//! One pass, per target:
//!
//! ```text
//!   library::monitored()        ── movies, and aired monitored episodes
//!         │
//!         ├─ grabs::blocking_for()  ── already taken care of? skip
//!         ▼
//!   search::run_search()        ── the same fan-out the UI uses
//!         │
//!         ▼
//!   pick_candidates()           ── score under the item's profile,
//!         │                        best first, threshold applied
//!         ▼
//!   grabs::reserve()            ── the barrier, BEFORE any network
//!         │
//!         ▼
//!   deliver::deliver()          ── hand to the download client
//! ```
//!
//! ## "Do I already have this?"
//!
//! Inferred from `grabs`, per the operator's decision: an item with a
//! grab in any state other than `failed`/`rejected` is taken care of and
//! drops out of the sweep (see [`crate::db::grabs::GrabStatus::blocks_search`]).
//! The accepted lie is a file deleted outside brarr — reconciling
//! against the disk arrives with the import phase.
//!
//! ## Why the candidate loop keeps going
//!
//! The barrier can refuse a release the scanner just picked: a previous
//! cycle may have tried it and marked it `failed`, which keeps its key
//! occupied on purpose. So the scanner walks candidates in score order
//! until one reservation succeeds, rather than giving up on the item.
//! It stops early on a *retryable* delivery failure, because that means
//! the client or the network is down and the next candidate would fail
//! identically.
//!
//! ## TV is per-episode, and conservative about what matches
//!
//! Each monitored, aired episode is its own target on the TVDB axis, and
//! a candidate is only accepted when its title carries the matching
//! `S01E02` marker. A release that cannot be positively tied to the
//! episode is left alone: `grabs.season_number` exists for season packs,
//! but recognising one means parsing which episodes it covers, which is
//! the import phase's problem. Not grabbing is the safe failure here.

use std::sync::Arc;
use std::time::Duration;

use brarr_core::{ImdbId, TmdbId, TvdbId};
use time::OffsetDateTime;
use tokio::task::JoinHandle;
// `tokio::time::sleep` by name rather than the module: importing
// `tokio::time` would shadow the `time` crate this module also uses.
use tokio::time::sleep;
use tracing::{debug, info, warn};

use crate::db::decisions::DecisionRow;
use crate::db::grabs::{self, NewGrab, Protocol};
use crate::db::library::{self, Episode, LibraryItem, MediaType};
use crate::db::quality_profiles::{self, QualityProfileRow};
use crate::db::{download_clients, searches};
use crate::deliver::{self, DeliveryOutcome};
use crate::search::{self, SearchKeys};
use crate::{AppError, AppState};

/// Ceiling on searches per sweep.
///
/// A library adopted all at once can have hundreds of wanted episodes,
/// and firing one search each would hammer every configured tracker in a
/// burst that looks exactly like abuse. What does not fit this cycle is
/// picked up by the next one — and is reported in
/// [`ScanSummary::skipped_over_cap`] rather than silently dropped.
pub const MAX_SEARCHES_PER_CYCLE: usize = 25;

/// Threshold applied to an item with no quality profile attached.
/// Same default as an \*arr instance without one: roughly "PT-BR audio
/// confirmed plus one quality bonus".
pub const DEFAULT_PUSH_THRESHOLD: u32 = 150;

/// Delay before the first sweep so it doesn't pile onto the startup
/// burst (migrations, the poller's first cycle, both servers binding).
const STARTUP_DELAY: Duration = Duration::from_secs(90);

/// What one sweep did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ScanSummary {
    /// Targets the sweep looked at (movies + episodes).
    pub targets: usize,
    /// Targets skipped because a grab already covers them.
    pub skipped_covered: usize,
    /// Targets skipped because the per-cycle cap was reached.
    pub skipped_over_cap: usize,
    /// Searches actually dispatched.
    pub searches: usize,
    /// Releases handed to a download client.
    pub grabbed: usize,
    /// Targets searched that produced nothing worth grabbing.
    pub no_candidate: usize,
    /// `(target, reason)` for everything that went wrong — a dead
    /// client, a provider error, a release the client refused.
    pub failures: Vec<(String, String)>,
}

impl ScanSummary {
    fn merge(&mut self, other: Self) {
        self.targets += other.targets;
        self.skipped_covered += other.skipped_covered;
        self.skipped_over_cap += other.skipped_over_cap;
        self.searches += other.searches;
        self.grabbed += other.grabbed;
        self.no_candidate += other.no_candidate;
        self.failures.extend(other.failures);
    }
}

/// Spawn the background sweep. Mirrors [`crate::poll::spawn`] and
/// [`crate::maintenance::spawn`]: dropping the handle aborts the task.
#[must_use]
pub fn spawn(state: AppState) -> JoinHandle<()> {
    let state = Arc::new(state);
    info!(
        target: "brarr_orchestrator::scan",
        interval_secs = state.poll_interval().as_secs(),
        cap = MAX_SEARCHES_PER_CYCLE,
        "starting the library scanner"
    );
    tokio::spawn(async move {
        sleep(STARTUP_DELAY).await;
        loop {
            run_one_cycle(&state).await;
            sleep(state.poll_interval()).await;
        }
    })
}

/// One scheduled sweep. Errors are logged, never propagated — a
/// transient DB or tracker hiccup must not kill the long-lived task.
async fn run_one_cycle(state: &AppState) {
    // Nothing to deliver to means every reservation would immediately be
    // released again. Skipping keeps brarr inert for anyone who has not
    // configured a client yet.
    match download_clients::list_enabled(state.pool()).await {
        Ok(clients) if clients.is_empty() => {
            debug!(
                target: "brarr_orchestrator::scan",
                "no download client enabled; skipping the sweep"
            );
            return;
        }
        Ok(_) => {}
        Err(e) => {
            warn!(target: "brarr_orchestrator::scan", error = %e, "could not read download clients");
            return;
        }
    }
    match run_once(state).await {
        Ok(summary) => info!(
            target: "brarr_orchestrator::scan",
            targets = summary.targets,
            searches = summary.searches,
            grabbed = summary.grabbed,
            covered = summary.skipped_covered,
            over_cap = summary.skipped_over_cap,
            no_candidate = summary.no_candidate,
            failures = summary.failures.len(),
            "sweep complete"
        ),
        Err(e) => warn!(target: "brarr_orchestrator::scan", error = %e, "sweep failed"),
    }
}

/// Sweep every monitored item.
///
/// # Errors
///
/// Returns [`AppError::Database`] when the library cannot be read.
/// Per-target failures are collected into [`ScanSummary::failures`]
/// instead of aborting the sweep.
pub async fn run_once(state: &AppState) -> Result<ScanSummary, AppError> {
    let items = library::monitored(state.pool()).await?;
    let mut summary = ScanSummary::default();
    let mut budget = MAX_SEARCHES_PER_CYCLE;
    for item in &items {
        let one = scan_item(state, item, &mut budget).await?;
        summary.merge(one);
    }
    Ok(summary)
}

/// Sweep one item, ignoring the per-cycle cap. This is the "buscar
/// agora" button: the operator asked for this one specifically.
///
/// # Errors
///
/// Returns [`AppError::Database`] on a DB failure.
pub async fn run_once_for_item(
    state: &AppState,
    item: &LibraryItem,
) -> Result<ScanSummary, AppError> {
    let mut budget = usize::MAX;
    scan_item(state, item, &mut budget).await
}

/// One target: a movie, or one episode of a series.
struct Target {
    /// Label for logs and the summary.
    label: String,
    /// Episode this target stands for; `None` for a movie.
    episode: Option<Episode>,
    /// Search axis.
    keys: SearchKeys,
    /// `S01E02`-style marker a candidate title must carry; `None` for a
    /// movie, where the search axis alone identifies the release.
    episode_marker: Option<(u16, u16)>,
}

async fn scan_item(
    state: &AppState,
    item: &LibraryItem,
    budget: &mut usize,
) -> Result<ScanSummary, AppError> {
    let mut summary = ScanSummary::default();
    let profile = resolve_profile(state, item).await?;
    let targets = build_targets(state, item).await?;

    for target in targets {
        summary.targets += 1;
        let covered =
            grabs::blocking_for(state.pool(), item.id, target.episode.as_ref().map(|e| e.id))
                .await?;
        if !covered.is_empty() {
            summary.skipped_covered += 1;
            continue;
        }
        if *budget == 0 {
            summary.skipped_over_cap += 1;
            continue;
        }
        *budget -= 1;
        summary.searches += 1;

        let outcome = match search::run_search(state, target.keys.clone()).await {
            Ok(o) => o,
            Err(e) => {
                summary.failures.push((target.label.clone(), e.to_string()));
                continue;
            }
        };
        // The column exists for exactly this: an ad-hoc search leaves it
        // NULL, a sweep names the item that asked for it.
        if let Err(e) =
            searches::attach_library_item(state.pool(), outcome.search.id, item.id).await
        {
            warn!(target: "brarr_orchestrator::scan", error = %e, "could not tie the search to its item");
        }

        let candidates = pick_candidates(&outcome.decisions, profile.as_ref(), &target);
        if candidates.is_empty() {
            summary.no_candidate += 1;
            continue;
        }
        match take_first_available(state, item, &target, &candidates).await? {
            TargetOutcome::Grabbed => summary.grabbed += 1,
            TargetOutcome::Nothing => summary.no_candidate += 1,
            TargetOutcome::Failed(reason) => {
                summary.failures.push((target.label.clone(), reason));
            }
        }
    }
    Ok(summary)
}

/// What happened to one target after candidates were picked.
enum TargetOutcome {
    Grabbed,
    /// Every candidate was refused by the barrier — all of them already
    /// tried and failed in an earlier sweep.
    Nothing,
    Failed(String),
}

/// Walk candidates best-first until one is reserved and delivered.
async fn take_first_available(
    state: &AppState,
    item: &LibraryItem,
    target: &Target,
    candidates: &[&DecisionRow],
) -> Result<TargetOutcome, AppError> {
    for decision in candidates {
        let new = NewGrab {
            item_id: item.id,
            episode_id: target.episode.as_ref().map(|e| e.id),
            season_number: None,
            decision_id: Some(decision.id),
            provider_id: match decision.provider_id {
                Some(id) => id,
                // A decision whose provider was deleted mid-sweep has no
                // barrier key to speak of; skip rather than reserve
                // something that cannot be deduplicated.
                None => continue,
            },
            provider_name: &decision.provider_name,
            release_id_remote: &decision.stable_release_key(),
            release_name: &decision.release_name,
            download_url: decision.download_url.as_deref(),
            protocol: protocol_of(decision),
        };
        // The barrier. Nothing above this line touched the network on
        // this release's behalf, and nothing below runs without winning.
        let Some(grab) = grabs::reserve(state.pool(), &new).await? else {
            // Already reserved, sent, or failed for good in an earlier
            // sweep. Try the next-best release.
            continue;
        };
        match deliver::deliver(state, &grab).await? {
            DeliveryOutcome::Sent { .. } => return Ok(TargetOutcome::Grabbed),
            DeliveryOutcome::Permanent(_) => {
                // This release is out; the grab row keeps its key so it
                // is not tried again. Move to the next candidate.
            }
            DeliveryOutcome::Retryable(reason) => {
                // The client or the network is down — every remaining
                // candidate would fail the same way.
                return Ok(TargetOutcome::Failed(reason));
            }
        }
    }
    Ok(TargetOutcome::Nothing)
}

/// Quality profile attached to the item, if any.
async fn resolve_profile(
    state: &AppState,
    item: &LibraryItem,
) -> Result<Option<QualityProfileRow>, AppError> {
    match item.profile_id {
        Some(id) => match quality_profiles::get_by_id(state.pool(), id).await {
            Ok(p) => Ok(Some(p)),
            // `ON DELETE SET NULL` makes this unlikely, but a stale id
            // must degrade to the default rather than stall the sweep.
            Err(AppError::NotFound(_)) => Ok(None),
            Err(e) => Err(e),
        },
        None => Ok(None),
    }
}

/// Everything worth searching for on one item.
async fn build_targets(state: &AppState, item: &LibraryItem) -> Result<Vec<Target>, AppError> {
    match item.media_type {
        MediaType::Movie => Ok(movie_target(item).into_iter().collect()),
        MediaType::Tv => {
            let Some(tvdb) = item.tvdb_id.and_then(|v| u32::try_from(v).ok()) else {
                debug!(
                    target: "brarr_orchestrator::scan",
                    item = %item.title,
                    "series has no TVDB id; the episode search axis needs one"
                );
                return Ok(Vec::new());
            };
            let Ok(tvdb) = TvdbId::new(tvdb) else {
                return Ok(Vec::new());
            };
            let episodes = library::episodes(state.pool(), item.id).await?;
            Ok(episode_targets(
                item,
                &episodes,
                tvdb,
                OffsetDateTime::now_utc(),
            ))
        }
    }
}

/// Search axis for a movie. `None` when the item carries no usable id,
/// which cannot happen through the TMDB add path but can through a
/// hand-edited row.
fn movie_target(item: &LibraryItem) -> Option<Target> {
    let tmdb = u32::try_from(item.tmdb_id)
        .ok()
        .and_then(|v| TmdbId::new(v).ok());
    let imdb = item.imdb_id.as_deref().and_then(parse_imdb);
    if tmdb.is_none() && imdb.is_none() {
        return None;
    }
    Some(Target {
        label: item.title.clone(),
        episode: None,
        keys: SearchKeys {
            tmdb,
            imdb,
            ..SearchKeys::default()
        },
        episode_marker: None,
    })
}

/// One target per monitored episode that has already aired.
///
/// Unaired episodes are skipped rather than searched: nothing exists to
/// find, and asking every tracker about them each cycle is the kind of
/// traffic that gets an account banned. Season 0 (TMDB's specials
/// bucket) is excluded for the same reason it is excluded from the tree
/// counts — it is not what the operator means by "the show".
fn episode_targets(
    item: &LibraryItem,
    episodes: &[Episode],
    tvdb: TvdbId,
    now: OffsetDateTime,
) -> Vec<Target> {
    episodes
        .iter()
        .filter(|e| e.monitored && e.season_number > 0)
        .filter(|e| e.air_date.is_some_and(|d| d <= now))
        .filter_map(|e| {
            let season = u16::try_from(e.season_number).ok()?;
            let number = u16::try_from(e.episode_number).ok()?;
            Some(Target {
                label: format!("{} S{season:02}E{number:02}", item.title),
                episode: Some(e.clone()),
                keys: SearchKeys::from_tvdb(tvdb, Some(season), Some(number)),
                episode_marker: Some((season, number)),
            })
        })
        .collect()
}

/// Parse the library's canonical `ttNNNNNNN` into the numeric id the
/// search axis wants. The two conventions are reconciled in code — see
/// the note on `library_items.imdb_id`.
fn parse_imdb(raw: &str) -> Option<ImdbId> {
    let digits = raw.trim().trim_start_matches("tt");
    digits.parse::<u32>().ok().and_then(|v| ImdbId::new(v).ok())
}

/// Releases worth grabbing for one target, best first.
///
/// Score is read under the item's profile — the same
/// [`quality_profiles::effective_score`] the Torznab feed's `?profile=`
/// filter uses, so the pull path and the sweep can never judge the same
/// release differently.
fn pick_candidates<'a>(
    decisions: &'a [DecisionRow],
    profile: Option<&QualityProfileRow>,
    target: &Target,
) -> Vec<&'a DecisionRow> {
    let threshold = profile.map_or(DEFAULT_PUSH_THRESHOLD, |p| p.push_threshold);
    let mut kept: Vec<(&DecisionRow, u32)> = decisions
        .iter()
        .filter(|d| !d.rejected)
        // Nothing to hand a client without one.
        .filter(|d| d.download_url.is_some())
        .filter(|d| match target.episode_marker {
            Some((season, episode)) => title_matches_episode(&d.release_name, season, episode),
            None => true,
        })
        .map(|d| (d, score_for(d, profile)))
        .filter(|(_, score)| *score >= threshold)
        .collect();
    // Score first, then seeders — a tie between two equally-scored
    // releases is best broken by the one more likely to finish.
    kept.sort_by(|a, b| b.1.cmp(&a.1).then(b.0.seeders.cmp(&a.0.seeders)));
    kept.into_iter().map(|(d, _)| d).collect()
}

fn score_for(decision: &DecisionRow, profile: Option<&QualityProfileRow>) -> u32 {
    match profile {
        Some(p) => {
            quality_profiles::effective_score(&decision.profile_scores, decision.score, p.id)
        }
        None => decision.score,
    }
}

/// Transport a decision travels over, from its provider kind.
fn protocol_of(decision: &DecisionRow) -> Protocol {
    match decision.provider_kind.as_deref() {
        Some(k) if k.eq_ignore_ascii_case("newznab") => Protocol::Usenet,
        _ => Protocol::Torrent,
    }
}

/// `true` when the title positively identifies this episode.
///
/// Accepts the two spellings that appear in the wild — `S01E02` and
/// `1x02` — case-insensitively. A title with no recognisable marker is
/// **not** accepted: it is most likely a season pack, and grabbing one
/// while recording it against a single episode would leave the rest of
/// the season looking acquired when it is not.
fn title_matches_episode(title: &str, season: u16, episode: u16) -> bool {
    let lowered = title.to_ascii_lowercase();
    let s_e = format!("s{season:02}e{episode:02}");
    if lowered.contains(&s_e) {
        return true;
    }
    let cross = format!("{season}x{episode:02}");
    lowered.contains(&cross)
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "tests assert on happy paths"
)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use uuid::Uuid;

    fn item(media_type: MediaType) -> LibraryItem {
        LibraryItem {
            id: Uuid::new_v4(),
            media_type,
            tmdb_id: 603,
            imdb_id: Some("tt0133093".to_owned()),
            tvdb_id: Some(70_726),
            title: "The Matrix".to_owned(),
            original_title: None,
            year: Some(1999),
            overview: None,
            poster_path: None,
            backdrop_path: None,
            tmdb_status: None,
            runtime_minutes: None,
            next_air_date: None,
            digital_release_at: None,
            physical_release_at: None,
            monitored: true,
            profile_id: None,
            root_folder: None,
            added_at: OffsetDateTime::now_utc(),
            metadata_refreshed_at: OffsetDateTime::now_utc(),
        }
    }

    fn episode(season: i32, number: i32, monitored: bool, air: Option<i64>) -> Episode {
        Episode {
            id: Uuid::new_v4(),
            item_id: Uuid::new_v4(),
            season_id: Uuid::new_v4(),
            season_number: season,
            episode_number: number,
            title: None,
            air_date: air.map(|t| OffsetDateTime::from_unix_timestamp(t).unwrap()),
            monitored,
        }
    }

    fn decision(name: &str, score: u32, seeders: u32) -> DecisionRow {
        DecisionRow {
            id: Uuid::new_v4(),
            search_id: Uuid::new_v4(),
            provider_id: Some(Uuid::new_v4()),
            provider_name: "capybara".to_owned(),
            release_name: name.to_owned(),
            release_id_remote: 1,
            release_guid: Some("abc".to_owned()),
            score,
            rejected: false,
            tags: Vec::new(),
            matched_rules: Vec::new(),
            seeders,
            leechers: 0,
            size_bytes: 1,
            resolution: "1080p".to_owned(),
            kind: "web-dl".to_owned(),
            download_url: Some("https://tracker/download/1".to_owned()),
            details_url: None,
            provider_kind: Some("unit3d".to_owned()),
            published_at: None,
            audio_languages: Vec::new(),
            subtitle_languages: Vec::new(),
            profile_scores: HashMap::new(),
            decided_at: OffsetDateTime::now_utc(),
        }
    }

    fn movie_target_for(item: &LibraryItem) -> Target {
        movie_target(item).expect("the fixture carries both ids")
    }

    #[test]
    fn a_movie_searches_on_both_axes() {
        let target = movie_target_for(&item(MediaType::Movie));
        assert_eq!(target.keys.tmdb.map(TmdbId::get), Some(603));
        assert_eq!(target.keys.imdb.map(ImdbId::get), Some(133_093));
        assert!(target.episode_marker.is_none());
    }

    #[test]
    fn the_imdb_prefix_is_stripped_for_the_search_axis() {
        // `library_items.imdb_id` keeps the `tt`; `searches.imdb_id`
        // keeps the bare number. This is where the two meet.
        assert_eq!(parse_imdb("tt0133093").map(ImdbId::get), Some(133_093));
        assert_eq!(parse_imdb("133093").map(ImdbId::get), Some(133_093));
        assert!(parse_imdb("").is_none());
        assert!(parse_imdb("tt0").is_none(), "zero is not a valid id");
    }

    #[test]
    fn episodes_are_targeted_only_when_monitored_and_aired() {
        let series = item(MediaType::Tv);
        let now = OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap();
        let episodes = vec![
            episode(1, 1, true, Some(1_600_000_000)), // aired, monitored
            episode(1, 2, false, Some(1_600_000_000)), // unmonitored
            episode(1, 3, true, Some(1_900_000_000)), // not out yet
            episode(1, 4, true, None),                // no date at all
            episode(0, 1, true, Some(1_600_000_000)), // specials bucket
        ];
        let tvdb = TvdbId::new(70_726).unwrap();
        let targets = episode_targets(&series, &episodes, tvdb, now);
        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].label, "The Matrix S01E01");
        assert_eq!(targets[0].keys.season, Some(1));
        assert_eq!(targets[0].keys.episode, Some(1));
        assert_eq!(targets[0].episode_marker, Some((1, 1)));
    }

    #[test]
    fn candidates_are_ordered_by_score_then_seeders() {
        let target = movie_target_for(&item(MediaType::Movie));
        let rows = vec![
            decision("mid", 200, 5),
            decision("best", 300, 1),
            decision("tie-low-seeds", 200, 1),
        ];
        let picked = pick_candidates(&rows, None, &target);
        let names: Vec<&str> = picked.iter().map(|d| d.release_name.as_str()).collect();
        assert_eq!(names, vec!["best", "mid", "tie-low-seeds"]);
    }

    #[test]
    fn candidates_below_the_threshold_are_not_grabbed() {
        let target = movie_target_for(&item(MediaType::Movie));
        let rows = vec![decision("weak", DEFAULT_PUSH_THRESHOLD - 1, 100)];
        assert!(pick_candidates(&rows, None, &target).is_empty());

        let rows = vec![decision("exact", DEFAULT_PUSH_THRESHOLD, 0)];
        assert_eq!(
            pick_candidates(&rows, None, &target).len(),
            1,
            "the threshold is inclusive, matching the push path"
        );
    }

    #[test]
    fn a_rejected_release_or_one_without_a_url_is_never_a_candidate() {
        let target = movie_target_for(&item(MediaType::Movie));
        let mut rejected = decision("rejected", 900, 10);
        rejected.rejected = true;
        let mut no_url = decision("no url", 900, 10);
        no_url.download_url = None;
        assert!(pick_candidates(&[rejected, no_url], None, &target).is_empty());
    }

    #[test]
    fn the_items_profile_score_wins_over_the_baseline() {
        let profile = QualityProfileRow {
            id: Uuid::new_v4(),
            name: "anime jp".to_owned(),
            description: None,
            push_threshold: 500,
            is_preset: false,
            rules: brarr_decision_service::RuleSet::default(),
            created_at: OffsetDateTime::now_utc(),
        };
        let target = movie_target_for(&item(MediaType::Movie));
        // Baseline says 100 — far below the profile's threshold — but the
        // profile itself scored it 700.
        let mut row = decision("dub", 100, 1);
        row.profile_scores.insert(profile.id, 700);
        let picked = pick_candidates(std::slice::from_ref(&row), Some(&profile), &target);
        assert_eq!(picked.len(), 1, "the profile's own verdict is what counts");

        // And a release the profile never scored falls back to baseline,
        // which here is nowhere near the bar.
        let plain = decision("plain", 100, 1);
        assert!(pick_candidates(&[plain], Some(&profile), &target).is_empty());
    }

    #[test]
    fn an_episode_target_only_accepts_a_title_naming_that_episode() {
        let series = item(MediaType::Tv);
        let tvdb = TvdbId::new(70_726).unwrap();
        let episodes = vec![episode(4, 7, true, Some(1_600_000_000))];
        let now = OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap();
        let target = &episode_targets(&series, &episodes, tvdb, now)[0];

        let rows = vec![
            decision("The.Boys.S04E07.1080p.WEB-DL", 300, 10),
            decision("The.Boys.S04E08.1080p.WEB-DL", 900, 99),
            decision("The.Boys.S04.COMPLETE.1080p.WEB-DL", 900, 99),
            decision("The.Boys.4x07.1080p.WEB-DL", 250, 1),
        ];
        let picked = pick_candidates(&rows, None, target);
        let names: Vec<&str> = picked.iter().map(|d| d.release_name.as_str()).collect();
        assert_eq!(
            names,
            vec!["The.Boys.S04E07.1080p.WEB-DL", "The.Boys.4x07.1080p.WEB-DL"],
            "a higher-scoring wrong episode and a season pack must both be left alone"
        );
    }

    #[test]
    fn episode_markers_are_matched_case_insensitively() {
        assert!(title_matches_episode("the.boys.s04e07.web", 4, 7));
        assert!(title_matches_episode("The Boys 4x07 WEB", 4, 7));
        assert!(!title_matches_episode("The Boys S04E70 WEB", 4, 7));
        assert!(!title_matches_episode("The Boys Season 4 WEB", 4, 7));
    }

    #[test]
    fn the_protocol_follows_the_provider_kind() {
        let mut usenet = decision("x", 1, 1);
        usenet.provider_kind = Some("newznab".to_owned());
        assert_eq!(protocol_of(&usenet), Protocol::Usenet);

        let torznab = decision("x", 1, 1);
        assert_eq!(protocol_of(&torznab), Protocol::Torrent);

        // A legacy row with no kind recorded is a torrent — every
        // provider family except newznab is.
        let mut legacy = decision("x", 1, 1);
        legacy.provider_kind = None;
        assert_eq!(protocol_of(&legacy), Protocol::Torrent);
    }

    #[test]
    fn the_summary_adds_up_across_items() {
        let mut a = ScanSummary {
            targets: 2,
            grabbed: 1,
            ..ScanSummary::default()
        };
        a.merge(ScanSummary {
            targets: 3,
            skipped_covered: 3,
            failures: vec![("x".to_owned(), "y".to_owned())],
            ..ScanSummary::default()
        });
        assert_eq!(a.targets, 5);
        assert_eq!(a.skipped_covered, 3);
        assert_eq!(a.grabbed, 1);
        assert_eq!(a.failures.len(), 1);
    }
}
