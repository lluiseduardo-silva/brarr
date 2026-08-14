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

use std::collections::HashMap;
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
use crate::db::episode_numbering;
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

/// How long a finished manual sweep stays readable by the badge that
/// asked for it.
///
/// Long enough that the operator can leave the tab and come back; short
/// enough that this stays a mailbox rather than a job history. The
/// durable record of what a sweep did is `grabs`.
pub const SCAN_RESULT_TTL: Duration = Duration::from_secs(10 * 60);

/// What a manual "buscar agora" sweep is doing, as far as the badge on
/// the detail page can tell.
///
/// The sweep is spawned and its `JoinHandle` dropped once the handler
/// stops waiting, so a scan that outran the wait used to be
/// unrecoverable — the handler's only honest answer was "recarregue a
/// página", and the page never learned the sweep had finished. This is
/// the mailbox it writes into instead. It is deliberately not a job
/// store: entries expire, and nothing depends on one being there.
#[derive(Debug, Clone)]
pub enum ScanProgress {
    /// Still sweeping.
    Running,
    /// Finished. Carries what it did, so the badge reads the same
    /// whether the sweep beat the wait or not.
    Done(ScanSummary),
    /// The sweep itself failed.
    Failed(String),
}

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
    /// Episodes in scope that the operator paused.
    ///
    /// Only interesting on a narrowed sweep: a paused episode among
    /// forty is not news, but "I clicked buscar on this episode and
    /// nothing happened" has exactly two honest answers, and this is
    /// one of them.
    pub skipped_unmonitored: usize,
    /// Episodes in scope, monitored, that have not aired. The other
    /// honest answer.
    pub skipped_unaired: usize,
    /// The item carries no id the search axis can use, so no target was
    /// buildable at all.
    ///
    /// A third way for a sweep to do nothing, and it used to read as
    /// "nada encontrado" — which is a lie in the direction that costs
    /// most: a series imported without a TVDB id can never be swept, and
    /// the badge said the trackers had nothing.
    pub no_search_axis: bool,
    /// The operator switched brarr off. A **fourth** way to do nothing,
    /// and the one most likely to be forgotten — which is exactly why it
    /// gets its own answer instead of joining "nada encontrado".
    pub paused: bool,
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
        self.skipped_unmonitored += other.skipped_unmonitored;
        self.skipped_unaired += other.skipped_unaired;
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
    // Nothing is searched, reserved or delivered while paused.
    if crate::db::settings::is_paused(state.pool()).await {
        return;
    }
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
        let one = scan_item(state, item, &mut budget, Scope::Item).await?;
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
    if crate::db::settings::is_paused(state.pool()).await {
        return Ok(ScanSummary {
            paused: true,
            ..ScanSummary::default()
        });
    }
    let mut budget = usize::MAX;
    scan_item(state, item, &mut budget, Scope::Item).await
}

/// Which slice of an item a sweep should look at.
///
/// The scheduler always runs [`Scope::Item`]. The narrower two exist for
/// the buttons on the detail screen: the operator pointing at one season
/// or one episode is a different ask from "sweep this title", and firing
/// the whole item because they wanted episode 7 is both slow and a lot of
/// tracker traffic for nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Scope {
    /// Everything the item offers.
    #[default]
    Item,
    /// One season, by its number.
    Season(i32),
    /// One episode, by season and episode number.
    Episode(i32, i32),
}

impl Scope {
    /// Whether this scope wants the given episode.
    const fn covers(self, season: i32, episode: i32) -> bool {
        match self {
            Self::Item => true,
            Self::Season(s) => s == season,
            Self::Episode(s, e) => s == season && e == episode,
        }
    }

    /// Whether this scope names a series slice at all. A movie has no
    /// seasons, so a narrowed sweep of one is a contradiction rather
    /// than an empty result.
    const fn is_narrowed(self) -> bool {
        !matches!(self, Self::Item)
    }
}

/// Run the same sweep against one season or one episode.
///
/// Same barrier, same profile, same delivery — only the target list is
/// narrower. **Monitoring is still respected**, deliberately: pausing a
/// title is the operator's standing decision and a one-click shortcut
/// should not quietly overrule it. What changes is that the summary now
/// says *why* nothing happened, so the button reports "episódio pausado"
/// instead of the "nada encontrado" it would otherwise render — and the
/// magnifier right beside it is the manual override that ignores every
/// flag.
///
/// # Errors
///
/// Same as [`run_once_for_item`].
pub async fn run_once_for_target(
    state: &AppState,
    item: &LibraryItem,
    scope: Scope,
) -> Result<ScanSummary, AppError> {
    if crate::db::settings::is_paused(state.pool()).await {
        return Ok(ScanSummary {
            paused: true,
            ..ScanSummary::default()
        });
    }
    let mut budget = usize::MAX;
    scan_item(state, item, &mut budget, scope).await
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

impl Target {
    /// What the barrier is being asked about. An episode carries its
    /// season so a pack of *another* season does not answer for it.
    fn grab_target(&self) -> grabs::GrabTarget {
        match &self.episode {
            Some(e) => grabs::GrabTarget::episode(e.id, e.season_number),
            None => grabs::GrabTarget::item(),
        }
    }
}

async fn scan_item(
    state: &AppState,
    item: &LibraryItem,
    budget: &mut usize,
    scope: Scope,
) -> Result<ScanSummary, AppError> {
    let mut summary = ScanSummary::default();
    let profile = resolve_profile(state, item).await?;
    let plan = build_targets(state, item, scope).await?;
    summary.skipped_unmonitored = plan.skipped_unmonitored;
    summary.skipped_unaired = plan.skipped_unaired;
    summary.no_search_axis = plan.no_search_axis;

    for target in plan.targets {
        summary.targets += 1;
        let covered = grabs::blocking_for(state.pool(), item.id, target.grab_target()).await?;
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

/// Everything worth searching for on one item, within `scope`.
async fn build_targets(
    state: &AppState,
    item: &LibraryItem,
    scope: Scope,
) -> Result<TargetPlan, AppError> {
    match item.media_type {
        // A movie has no seasons, so a narrowed sweep of one is a
        // contradiction rather than an empty result — refuse it instead
        // of quietly searching the whole film.
        MediaType::Movie if scope.is_narrowed() => Ok(TargetPlan::default()),
        MediaType::Movie => Ok(TargetPlan {
            targets: movie_target(item).into_iter().collect(),
            ..TargetPlan::default()
        }),
        MediaType::Tv => {
            let Some(tvdb) = item.tvdb_id.and_then(|v| u32::try_from(v).ok()) else {
                debug!(
                    target: "brarr_orchestrator::scan",
                    item = %item.title,
                );
                return Ok(TargetPlan {
                    no_search_axis: true,
                    ..TargetPlan::default()
                });
            };
            let Ok(tvdb) = TvdbId::new(tvdb) else {
                return Ok(TargetPlan {
                    no_search_axis: true,
                    ..TargetPlan::default()
                });
            };
            let episodes = library::episodes(state.pool(), item.id).await?;
            // Empty for every title until an operator applies an episode
            // group, so "no entry" is the fallback rather than a flag to
            // check first.
            let numbering = episode_numbering::for_item(state.pool(), item.id).await?;
            Ok(episode_targets(
                item,
                &episodes,
                tvdb,
                OffsetDateTime::now_utc(),
                scope,
                &numbering,
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
/// traffic that gets an account banned.
///
/// Season 0 — TMDB's specials bucket — is **not** excluded, since
/// v0.10.1. The `monitored` flag is what keeps it out of the way, and it
/// does so on its own: specials arrive unmonitored and stay that way
/// unless somebody says otherwise. Excluding the season on top of that
/// meant [`crate::coverage`] counted a monitored special the sweep then
/// refused to chase, which is a lever that moves the number and nothing
/// else. A special is matchable, too: `S00E01` parses (only the `0x10`
/// spelling is refused, and that one is not real).
///
/// `numbering` translates the catalogue's coordinates into the ones
/// releases actually use. It is empty for almost every title — the
/// canonical numbering *is* the scene's — and where it is not, it is the
/// difference between finding an episode and refusing every candidate:
/// TMDB models Jujutsu Kaisen as one season of 59, so brarr asked for
/// `S01E35` while the release was named `S02E23`. **Only the query and
/// the marker are translated.** `Target::episode` stays canonical,
/// because it is what the grab, the file name and Sonarr are keyed on.
fn episode_targets(
    item: &LibraryItem,
    episodes: &[Episode],
    tvdb: TvdbId,
    now: OffsetDateTime,
    scope: Scope,
    numbering: &HashMap<(i32, i32), episode_numbering::Numbering>,
) -> TargetPlan {
    let mut plan = TargetPlan::default();
    for e in episodes {
        // Season 0 (TMDB's specials bucket) used to be excluded here, and
        // that was the one asymmetry left in the library screens: since
        // v0.7.1 `crate::coverage` *counts* a monitored special, so one
        // without a file read as "faltando" while the sweep quietly
        // refused to go after it.
        //
        // The exclusion was also redundant. The `monitored` filter below
        // is what actually keeps the bucket out: specials arrive
        // unmonitored and stay that way unless somebody says otherwise.
        // Measured on this operator's catalogue — 914 specials, exactly
        // one monitored — so honouring the flag adds one target, not
        // nine hundred. Monitoring is the operator's lever, and a lever
        // that moves the count but not the sweep is a broken one.
        //
        // A negative season number is not a thing TMDB emits, but it is
        // not a target either.
        if e.season_number < 0 || !scope.covers(e.season_number, e.episode_number) {
            continue;
        }
        // Counted, not silently dropped: on a narrowed sweep these two
        // are the whole answer to "I clicked and nothing happened".
        if !e.monitored {
            plan.skipped_unmonitored += 1;
            continue;
        }
        if e.air_date.is_none_or(|d| d > now) {
            plan.skipped_unaired += 1;
            continue;
        }
        let asked = numbering
            .get(&(e.season_number, e.episode_number))
            .map_or((e.season_number, e.episode_number), |n| {
                (n.season, n.episode)
            });
        let (Ok(season), Ok(number)) = (u16::try_from(asked.0), u16::try_from(asked.1)) else {
            continue;
        };
        plan.targets.push(Target {
            label: format!("{} S{season:02}E{number:02}", item.title),
            // Canonical, deliberately: this is the episode the grab is
            // recorded against and the name the importer will write.
            episode: Some(e.clone()),
            keys: SearchKeys::from_tvdb(tvdb, Some(season), Some(number)),
            episode_marker: Some((season, number)),
        });
    }
    plan
}

/// What [`episode_targets`] resolved to, including the two reasons an
/// episode was left out.
///
/// The counts exist for the narrowed sweep. On a full item sweep nobody
/// reads them — a paused episode among forty is not news. On "buscar
/// este episódio" they are the entire report.
#[derive(Default)]
struct TargetPlan {
    /// Episodes that will actually be searched.
    targets: Vec<Target>,
    /// In scope, but the operator paused it.
    skipped_unmonitored: usize,
    /// In scope and monitored, but it has not aired.
    skipped_unaired: usize,
    /// No usable search id on the item at all.
    no_search_axis: bool,
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
/// Shared with [`crate::import`], which faces the same question from the
/// other end — which file inside a finished download is episode 7 — and
/// must answer it the same way the scanner did when it chose the release.
///
/// Accepts the two spellings that appear in the wild — `S01E02` and
/// `1x02` — case-insensitively. A title with no recognisable marker is
/// **not** accepted: it is most likely a season pack, and grabbing one
/// while recording it against a single episode would leave the rest of
/// the season looking acquired when it is not.
///
/// Padding is a spelling, not a meaning: `S01E02`, `S1E2` and `S001E002`
/// name the same episode, so the marker is **parsed**, never rendered and
/// compared. Building `format!("s{season:02}e{episode:02}")` and asking
/// whether the title contains it missed `S011E09` — a three-digit season,
/// which is every one of the 24 unmatched `Series` files the operator has
/// — and matched `1920x1080` as season 1920, episode 1080.
pub(crate) fn title_matches_episode(title: &str, season: u16, episode: u16) -> bool {
    episode_markers(title)
        .iter()
        .any(|m| m.season == season && m.episode == episode)
}

/// One episode marker read out of a name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Marker {
    /// Season it names.
    pub season: u16,
    /// Episode it names.
    pub episode: u16,
    /// A second episode group followed immediately — `S01E01E02`.
    ///
    /// Only the first is reported, which is what the sweep has always
    /// done. [`crate::adopt`] refuses such a file outright: recording a
    /// two-episode file against one episode leaves the other looking
    /// unacquired, and against both would need one file to hold two
    /// barrier keys.
    pub chained: bool,
}

/// Every episode marker the name carries, in order.
///
/// Extraction lives here, next to the verification built on it, so the
/// two can never disagree about what a marker is.
pub(crate) fn episode_markers(title: &str) -> Vec<Marker> {
    let lowered = title.to_ascii_lowercase();
    let bytes = lowered.as_bytes();
    (0..bytes.len())
        .filter_map(|at| marker_at(bytes, at))
        .collect()
}

/// The episode marker starting at `at`, in either spelling.
fn marker_at(bytes: &[u8], at: usize) -> Option<Marker> {
    if bytes[at] == b's' {
        return season_episode_marker(bytes, at);
    }
    cross_marker(bytes, at)
}

/// `s<digits>e<digits>`, any padding.
fn season_episode_marker(bytes: &[u8], at: usize) -> Option<Marker> {
    // Glued to a word, it is part of that word: `Seasons01e02` is not a
    // marker, and reading it as one would adopt a file against an episode
    // nobody named.
    if at > 0 && bytes[at - 1].is_ascii_alphanumeric() {
        return None;
    }
    let (season, after_season) = digits(bytes, at + 1, usize::MAX)?;
    if bytes.get(after_season) != Some(&b'e') {
        return None;
    }
    let (episode, end) = digits(bytes, after_season + 1, usize::MAX)?;
    let chained = bytes.get(end) == Some(&b'e') && digits(bytes, end + 1, usize::MAX).is_some();
    Some(Marker {
        season,
        episode,
        chained,
    })
}

/// `<digits>x<digits>`, the spelling `1x02` uses.
///
/// Both runs are capped at two digits and the marker must end on a
/// boundary, because the same shape spells a resolution: `1920x1080` is
/// not season 19 episode 20, and `4x070p` is not episode 7. Season 0 is
/// refused outright — it is TMDB's specials bucket, it never appears in a
/// release name as `0x10`, and accepting it would invent an episode.
fn cross_marker(bytes: &[u8], at: usize) -> Option<Marker> {
    if !bytes[at].is_ascii_digit() {
        return None;
    }
    if at > 0 && bytes[at - 1].is_ascii_alphanumeric() {
        return None;
    }
    let (season, after_season) = digits(bytes, at, 2)?;
    if season == 0 || bytes.get(after_season) != Some(&b'x') {
        return None;
    }
    let (episode, end) = digits(bytes, after_season + 1, 2)?;
    if bytes.get(end).is_some_and(u8::is_ascii_alphanumeric) {
        return None;
    }
    Some(Marker {
        season,
        episode,
        chained: false,
    })
}

/// The run of ASCII digits at `from`: its value and the index just past
/// it. `None` when there are none, when the run is longer than `max_len`,
/// or when it does not fit a `u16`.
fn digits(bytes: &[u8], from: usize, max_len: usize) -> Option<(u16, usize)> {
    let mut end = from;
    while end < bytes.len() && bytes[end].is_ascii_digit() {
        end += 1;
    }
    if end == from || end - from > max_len {
        return None;
    }
    let value = std::str::from_utf8(&bytes[from..end]).ok()?.parse().ok()?;
    Some((value, end))
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
            poster_source: None,
            backdrop_source: None,
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
            monitor_scope: crate::db::library::MonitorScope::All,
            added_at: OffsetDateTime::now_utc(),
            metadata_refreshed_at: OffsetDateTime::now_utc(),
        }
    }

    fn episode(season: i32, number: i32, monitored: bool, air: Option<i64>) -> Episode {
        Episode {
            id: Uuid::new_v4(),
            tmdb_episode_id: None,
            item_id: Uuid::new_v4(),
            season_id: Uuid::new_v4(),
            season_number: season,
            episode_number: number,
            title: None,
            air_date: air.map(|t| OffsetDateTime::from_unix_timestamp(t).unwrap()),
            monitored,
            source: None,
            external_id: None,
            absolute_number: None,
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
    fn an_applied_numbering_changes_the_query_and_nothing_else() {
        // TMDB models Jujutsu Kaisen as one season of 59; every release
        // is named `S02E23`. brarr asked for `S01E47` and the marker
        // filter refused every candidate. The translation fixes the two
        // things that talk to the network — and only those.
        let series = item(MediaType::Tv);
        let now = OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap();
        let episodes = vec![episode(1, 47, true, Some(1_600_000_000))];
        let tvdb = TvdbId::new(70_726).unwrap();

        let mut numbering = HashMap::new();
        numbering.insert(
            (1, 47),
            episode_numbering::Numbering {
                season: 2,
                episode: 23,
            },
        );

        let plan = episode_targets(&series, &episodes, tvdb, now, Scope::Item, &numbering);
        let target = &plan.targets[0];

        assert_eq!(target.keys.season, Some(2), "the indexer is asked for S02");
        assert_eq!(target.keys.episode, Some(23));
        assert_eq!(
            target.episode_marker,
            Some((2, 23)),
            "and the title must carry the marker the release actually uses"
        );
        assert_eq!(target.label, "The Matrix S02E23");

        // The identity does not move: this is still canonical S01E47,
        // which is what the grab is recorded against, what the importer
        // writes to disk, and what Sonarr and `relink` pair on.
        let held = target.episode.as_ref().unwrap();
        assert_eq!(held.season_number, 1);
        assert_eq!(held.episode_number, 47);
    }

    #[test]
    fn an_episode_the_group_does_not_cover_keeps_the_canonical_numbering() {
        // A group may cover fewer episodes than the catalogue holds —
        // Jujutsu Kaisen's "Story Arcs" lists 48 of 59. The rest must
        // keep working rather than fall off the sweep.
        let series = item(MediaType::Tv);
        let now = OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap();
        let episodes = vec![episode(1, 59, true, Some(1_600_000_000))];
        let tvdb = TvdbId::new(70_726).unwrap();

        let mut numbering = HashMap::new();
        numbering.insert(
            (1, 47),
            episode_numbering::Numbering {
                season: 2,
                episode: 23,
            },
        );

        let plan = episode_targets(&series, &episodes, tvdb, now, Scope::Item, &numbering);
        assert_eq!(plan.targets[0].episode_marker, Some((1, 59)));
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
        ];
        let tvdb = TvdbId::new(70_726).unwrap();
        let targets =
            episode_targets(&series, &episodes, tvdb, now, Scope::Item, &HashMap::new()).targets;
        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].label, "The Matrix S01E01");
        assert_eq!(targets[0].keys.season, Some(1));
        assert_eq!(targets[0].keys.episode, Some(1));
        assert_eq!(targets[0].episode_marker, Some((1, 1)));
    }

    /// Season 0 is no longer excluded, and the flag is what decides.
    ///
    /// `crate::coverage` has counted a monitored special since v0.7.1, so
    /// leaving the sweep blind to it meant an episode could read
    /// "faltando" forever with nothing ever going after it. The bucket
    /// stays out of the way on its own: specials arrive unmonitored.
    #[test]
    fn a_monitored_special_is_targeted_and_an_unmonitored_one_is_not() {
        let series = item(MediaType::Tv);
        let now = OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap();
        let episodes = vec![
            episode(0, 1, true, Some(1_600_000_000)), // monitored special
            episode(0, 2, false, Some(1_600_000_000)), // the usual case
        ];
        let tvdb = TvdbId::new(70_726).unwrap();
        let plan = episode_targets(&series, &episodes, tvdb, now, Scope::Item, &HashMap::new());

        assert_eq!(plan.targets.len(), 1, "only the monitored one");
        assert_eq!(plan.targets[0].episode_marker, Some((0, 1)));
        assert_eq!(plan.targets[0].keys.season, Some(0));
        assert_eq!(plan.skipped_unmonitored, 1);
    }

    /// Targeting a special only helps if a special can also be *matched*.
    /// `S00E01` is a real spelling and parses; `0x10` is not and does not
    /// — that refusal lives in `cross_marker` and stays.
    #[test]
    fn a_special_is_matchable_by_its_sxxexx_marker() {
        let found = episode_markers("Show.S00E01.Making.Of.1080p.WEB-DL");
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].season, 0);
        assert_eq!(found[0].episode, 1);

        // The other spelling would invent an episode out of a number
        // pair, so it is still refused.
        assert!(episode_markers("Show.0x10.1080p").is_empty());
    }

    /// And the narrow scope reaches it too, so the button on the specials
    /// row is not decorative.
    #[test]
    fn the_specials_season_can_be_swept_on_its_own() {
        let series = item(MediaType::Tv);
        let now = OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap();
        let episodes = vec![
            episode(0, 1, true, Some(1_600_000_000)),
            episode(1, 1, true, Some(1_600_000_000)),
        ];
        let tvdb = TvdbId::new(70_726).unwrap();
        let plan = episode_targets(
            &series,
            &episodes,
            tvdb,
            now,
            Scope::Season(0),
            &HashMap::new(),
        );

        assert_eq!(plan.targets.len(), 1);
        assert_eq!(plan.targets[0].keys.season, Some(0));
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
        let target = &episode_targets(&series, &episodes, tvdb, now, Scope::Item, &HashMap::new())
            .targets[0];

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

    /// Padding is a spelling, not a meaning. Measured over the operator's
    /// 7 215 files: every one of the 24 `Series` misses was
    /// `The Big Bang Theory - S011E09` — a three-digit season, which a
    /// fixed `{:02}` cannot see. `S1E9` escapes the same way.
    #[test]
    fn episode_markers_accept_any_padding() {
        assert!(title_matches_episode(
            "The Big Bang Theory - S011E09",
            11,
            9
        ));
        assert!(title_matches_episode("Show.S1E9.1080p", 1, 9));
        assert!(title_matches_episode("Show.s001e009.1080p", 1, 9));
        assert!(title_matches_episode("Show 1x2 1080p", 1, 2));
    }

    /// Resolutions and codecs are digits next to letters, and a loose
    /// reading of the cross form turns them into episodes.
    #[test]
    fn episode_markers_reject_resolutions_and_codecs() {
        assert!(!title_matches_episode("Movie.1920x1080.x264", 19, 10));
        assert!(!title_matches_episode("Movie.1920x1080.x264", 1920, 1080));
        assert!(!title_matches_episode("Movie.2160p.x265", 21, 60));
        // Season 0 is TMDB's specials bucket and never appears as `0x10`
        // in the wild; reading it here would invent an episode.
        assert!(!title_matches_episode("Some.Release.0x10.mkv", 0, 10));
    }

    /// A marker glued to a word is part of that word, not a marker.
    #[test]
    fn episode_markers_need_a_boundary() {
        assert!(!title_matches_episode("Seasons01e02", 1, 2));
        assert!(!title_matches_episode("Show.4x070p", 4, 7));
        assert!(title_matches_episode("[Group] Show - S04E07 [1080p]", 4, 7));
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
