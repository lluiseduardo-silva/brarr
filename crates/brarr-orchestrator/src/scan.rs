//! The monitored sweep — brarr looking for what it does not have yet.
//!
//! This is the half of Radarr/Sonarr that decides *when* to search.
//! [`crate::poll`] asked an \*arr what was missing; this asks brarr's own
//! library, which is what makes brarr the only agent in the loop.
//!
//! One pass, per target:
//!
//! ```text
//!   library::monitored()        ── released movies, aired monitored episodes
//!         │
//!         ├─ grabs::live_coverage()  ── already taken care of? skip
//!         ▼
//!   sort_queue()                ── fresh first, then least recently searched
//!         │                        (only the first N fit this cycle)
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
//! ## The queue rotates, and that is what makes the ceiling honest
//!
//! A cycle dispatches at most [`DEFAULT_SEARCHES_PER_CYCLE`] searches, so
//! a catalogue with hundreds of gaps is swept over many cycles rather
//! than in one burst that looks like abuse. That budget used to be spent
//! from a fixed head — items by `metadata_refreshed_at`, then season and
//! episode — and nothing ever moved it, so the same targets were searched
//! every cycle forever while the rest were never searched at all.
//!
//! The head of that list is the worst possible place to spend a budget:
//! a target is wanted *because* nothing was found for it, so whatever
//! sits at the front is precisely what keeps finding nothing. Measured on
//! this operator's catalogue — 294 wanted targets against a ceiling of
//! 25, of which 269 had never been searched once. One title contributed
//! twelve of the twenty-five: a season whose files exist under a
//! different cut of the show, so those gaps can never close, and they
//! were holding the budget for the whole library.
//!
//! [`sort_queue`] spends it least-recently-searched first, within two
//! tiers: what came out inside [`FRESH_WINDOW`], and everything else.
//! That is brarr's version of the split Sonarr draws between its RSS pass
//! and its search for missing episodes — two tiers of one queue rather
//! than two schedules to keep in agreement.
//!
//! The tiers **divide** the cycle ([`split_budget`]) instead of ordering
//! it, and that distinction was measured rather than guessed: the same
//! catalogue carries 30 targets inside the window, which alone exceeds
//! the ceiling. Strict priority would have spent every cycle on the fresh
//! tier and left the 249 others at zero — the identical starvation, one
//! tier down, and harder to spot because the sweep would look busy.
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

use brarr_core::TvdbId;
use time::OffsetDateTime;
use tokio::task::JoinHandle;
// `tokio::time::sleep` by name rather than the module: importing
// `tokio::time` would shadow the `time` crate this module also uses.
use tokio::time::sleep;
use tracing::{debug, info, warn};
use uuid::Uuid;

use crate::db::decisions::DecisionRow;
use crate::db::grabs::{self, NewGrab, Protocol};
use crate::db::library::{self, Episode, LibraryItem, MediaType, ProductionStatus};
use crate::db::quality_profiles::{self, QualityProfileRow};
use crate::db::{download_clients, item_ids, scan_attempts, searches};
use crate::deliver::{self, DeliveryOutcome};
use crate::metadata::axis;
use crate::search::{self, SearchKeys};
use crate::{AppError, AppState};

/// Ceiling on searches per sweep, when `/settings` names none.
///
/// A library adopted all at once can have hundreds of wanted episodes,
/// and firing one search each would hammer every configured tracker in a
/// burst that looks exactly like abuse. What does not fit this cycle is
/// picked up by the next one — and is reported in
/// [`ScanSummary::skipped_over_cap`] rather than silently dropped.
///
/// **The ceiling is only honest because the queue rotates.** It used to
/// be spent from a fixed head — items by `metadata_refreshed_at`, then
/// season and episode — and nothing ever moved, so the same targets were
/// searched every cycle forever. Those are by definition the ones that
/// never find anything: a target is wanted precisely because nothing was
/// found for it. Measured on this operator's catalogue: 294 wanted
/// targets against this ceiling, so 269 of them had never been searched
/// once, including every episode that had just aired. See [`sort_queue`].
pub const DEFAULT_SEARCHES_PER_CYCLE: usize = 25;

/// Floor for the configured ceiling.
///
/// Zero is a paused scanner spelled a second way, and
/// [`crate::db::settings::KEY_PAUSED`] is the one that says so — an
/// operator who reaches for this box wants fewer searches, not a sweep
/// that silently stops sweeping.
pub const MIN_SEARCHES_PER_CYCLE: usize = 1;

/// How recently a target has to have come out to jump the backlog.
///
/// Rotation alone puts a newly aired episode behind whatever backlog
/// precedes it — on this operator's catalogue a full pass is roughly six
/// hours, and the episode that aired this afternoon is the one being
/// waited for. This is brarr's version of the split Sonarr draws between
/// its RSS pass and its search for missing episodes: one sweep, two
/// tiers, rather than two schedules to keep in agreement.
const FRESH_WINDOW: time::Duration = time::Duration::days(14);

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
    /// Targets searched whose results held nothing worth grabbing —
    /// nothing found, or nothing that cleared the profile's threshold.
    pub no_candidate: usize,
    /// Targets whose candidates were **all refused by the barrier**:
    /// every release worth grabbing had already been tried and marked
    /// `failed` on an earlier sweep.
    ///
    /// Counted apart from [`Self::no_candidate`] because the two say
    /// opposite things and the badge used to say the wrong one. This is
    /// the shape the operator reported: the automatic search answered
    /// "nada encontrado — nenhuma release passou do threshold" while the
    /// interactive search on the same episode listed nine releases, most
    /// of them above the line. Both halves of that sentence were false —
    /// the releases were found, and they passed. They were simply all
    /// spent, which is a story about `grabs`, not about the trackers.
    pub exhausted: usize,
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
    /// Films that are not out yet.
    ///
    /// Kept apart from [`Self::skipped_unaired`] because the two have
    /// different fixes: an unaired episode is a date to wait for, and a
    /// film with no digital release may well be on a shelf somewhere
    /// under a status the operator can check.
    pub skipped_unreleased: usize,
    /// The item carries no id the search axis can use, so no target was
    /// buildable at all.
    ///
    /// A third way for a sweep to do nothing, and it used to read as
    /// "nada encontrado" — which is a lie in the direction that costs
    /// most: a series imported without a TVDB id can never be swept, and
    /// the badge said the trackers had nothing.
    pub no_search_axis: bool,
    /// Ids brarr holds and cannot search with, in the operator's words.
    ///
    /// The difference between "sem id de busca" and *which* id, and it
    /// matters most for the case that produced it: an id that is
    /// perfectly good in its own catalogue and simply not something any
    /// indexer accepts. That is a title to resolve, not a tracker to
    /// check.
    pub axis_rejections: Vec<String>,
    /// The operator switched brarr off. A **fourth** way to do nothing,
    /// and the one most likely to be forgotten — which is exactly why it
    /// gets its own answer instead of joining "nada encontrado".
    pub paused: bool,
    /// `(target, reason)` for everything that went wrong — a dead
    /// client, a provider error, a release the client refused.
    pub failures: Vec<(String, String)>,
}

impl ScanSummary {
    /// Take everything [`build_targets`] decided *not* to search.
    ///
    /// The counters are additive because the scheduled sweep folds every
    /// item into one summary; the narrowed sweep folds exactly one, and
    /// reads them as its whole report.
    fn absorb(&mut self, plan: &TargetPlan) {
        self.skipped_unmonitored += plan.skipped_unmonitored;
        self.skipped_unaired += plan.skipped_unaired;
        self.skipped_unreleased += plan.skipped_unreleased;
        self.no_search_axis |= plan.no_search_axis;
        self.axis_rejections.extend(
            plan.rejected
                .iter()
                .map(crate::metadata::axis::AxisRejection::message),
        );
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
        default_cap = DEFAULT_SEARCHES_PER_CYCLE,
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
    // Two bulk reads instead of one query per target. The sweep needs the
    // whole wanted list in hand before it can order it, and asking per
    // target was already thousands of queries a cycle — a cost that scaled
    // with the catalogue rather than with the budget actually spent.
    let coverage = grabs::live_coverage(state.pool()).await?;
    let attempts = scan_attempts::last_searched(state.pool()).await?;
    let now = OffsetDateTime::now_utc();

    let mut summary = ScanSummary::default();
    let mut queue: Vec<Wanted<'_>> = Vec::new();
    for item in &items {
        let plan = build_targets(state, item, Scope::Item).await?;
        summary.absorb(&plan);
        for target in plan.targets {
            summary.targets += 1;
            if is_covered(&coverage, item.id, target.grab_target()) {
                summary.skipped_covered += 1;
                continue;
            }
            let key = (item.id, target.episode.as_ref().map(|e| e.id));
            queue.push(Wanted {
                tier: tier_of(item, target.episode.as_ref(), now),
                last_searched: attempts.get(&key).copied(),
                item,
                target,
            });
        }
    }

    // Two tiers, each rotating on its own, each with a guaranteed share
    // of the cycle — see `split_budget` for why this is not a priority
    // order.
    let (mut fresh, mut backlog): (Vec<_>, Vec<_>) =
        queue.into_iter().partition(|w| w.tier == Tier::Fresh);
    sort_queue(&mut fresh);
    sort_queue(&mut backlog);

    let budget = configured_budget(state.pool()).await;
    let (take_fresh, take_backlog) = split_budget(budget, fresh.len(), backlog.len());
    summary.skipped_over_cap = (fresh.len() - take_fresh) + (backlog.len() - take_backlog);
    fresh.truncate(take_fresh);
    backlog.truncate(take_backlog);
    debug!(
        target: "brarr_orchestrator::scan",
        fresh = take_fresh,
        backlog = take_backlog,
        over_cap = summary.skipped_over_cap,
        "queue for this cycle"
    );
    let queue: Vec<Wanted<'_>> = fresh.into_iter().chain(backlog).collect();

    // Resolved once per item rather than once per target: a series
    // contributing eight episodes to one cycle would otherwise read the
    // same profile eight times.
    let mut profiles: HashMap<Uuid, Option<QualityProfileRow>> = HashMap::new();
    for wanted in &queue {
        // Not `entry()`: the lookup has to happen before the `await` that
        // fills it, and holding the entry across one would borrow the map
        // for the whole call.
        if let std::collections::hash_map::Entry::Vacant(slot) = profiles.entry(wanted.item.id) {
            let profile = resolve_profile(state, wanted.item).await?;
            slot.insert(profile);
        }
        let profile = profiles.get(&wanted.item.id).and_then(Option::as_ref);
        summary.searches += 1;
        run_target(state, wanted.item, &wanted.target, profile, &mut summary).await?;
    }
    Ok(summary)
}

/// One target waiting for a slot, with everything that decides its place
/// in line.
struct Wanted<'a> {
    /// Which half of the queue it sits in.
    tier: Tier,
    /// When this exact target was last searched. `None` — never — sorts
    /// ahead of every searched one, which is what drains a fresh
    /// catalogue instead of circling its first page.
    last_searched: Option<OffsetDateTime>,
    /// Borrowed from the caller's list: the queue holds a few hundred of
    /// these and a `LibraryItem` is not a cheap clone.
    item: &'a LibraryItem,
    /// What to search for.
    target: Target,
}

/// Which half of the queue a target belongs to.
///
/// Derived `Ord` puts [`Self::Fresh`] first, and the declaration order is
/// the ordering — there is no second place saying so.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Tier {
    /// Out within [`FRESH_WINDOW`].
    Fresh,
    /// Everything else.
    Backlog,
}

/// Where a target sits, from the date it became findable.
///
/// Only reached for targets that already passed the release gate, so the
/// date is in the past and the question is only how far.
fn tier_of(item: &LibraryItem, episode: Option<&Episode>, now: OffsetDateTime) -> Tier {
    let out_at = match episode {
        Some(e) => e.air_date,
        None => item.digital_release_at,
    };
    match out_at {
        Some(date) if date > now - FRESH_WINDOW => Tier::Fresh,
        // Older than the window — or a film with no digital date at all,
        // which is most of a back catalogue and is never news.
        _ => Tier::Backlog,
    }
}

/// The order the budget is spent in.
///
/// Fresh first, then least-recently-searched, and never-searched ahead of
/// both — which `Option`'s own ordering already says, so it is not
/// written twice. The label breaks what is left, so a cycle is
/// reproducible and a test can assert on one.
///
/// This function is the whole fix for the starvation: with a fixed order
/// the ceiling meant the first N wanted targets were searched forever and
/// target N+1 never once, and the head of that list is the part that
/// never finds anything.
fn sort_queue(queue: &mut [Wanted<'_>]) {
    queue.sort_by(|a, b| {
        a.tier
            .cmp(&b.tier)
            .then(a.last_searched.cmp(&b.last_searched))
            .then_with(|| a.target.label.cmp(&b.target.label))
    });
}

/// Share of a cycle the fresh tier is guaranteed, as `NUM / DEN`.
///
/// Expressed as two integers rather than a float because a budget is
/// counted in searches, and a rounding rule is easier to read than a cast.
const FRESH_SHARE_NUM: usize = 7;
/// Denominator of [`FRESH_SHARE_NUM`].
const FRESH_SHARE_DEN: usize = 10;

/// How one cycle's budget is divided between the two tiers.
///
/// **This is a split, not a priority order, and the measurement is why.**
/// "Fresh first, then whatever is left" reads like the obvious rule and
/// is wrong on real data: on this operator's catalogue the fresh tier is
/// 30 targets against a ceiling of 25, so strict priority would spend
/// every cycle inside the two-week window and the 249 backlog targets
/// would get exactly zero searches — the same starvation this rotation
/// exists to end, moved one tier down and harder to see, because the
/// sweep would look busy.
///
/// **Neither share is a cap when the other tier cannot fill its own.** A
/// share is a floor for the tier that needs it; a quiet week has no fresh
/// episodes and the backlog should get the whole cycle, not 30% of it.
///
/// The fresh tier wins a budget of one, because a single slot spent on
/// what aired yesterday beats one spent on a gap that has been open for
/// years.
fn split_budget(budget: usize, fresh: usize, backlog: usize) -> (usize, usize) {
    let share = (budget * FRESH_SHARE_NUM / FRESH_SHARE_DEN).max(1);
    let take_fresh = fresh.min(share.max(budget.saturating_sub(backlog)));
    let take_backlog = backlog.min(budget - take_fresh);
    (take_fresh, take_backlog)
}

/// Whether any live grab answers for this target.
///
/// [`grabs::live_coverage`] plus [`grabs::Coverage::covers`] in Rust,
/// rather than [`grabs::blocking_for`] per target. The two are the same
/// rule — a test confronts the SQL predicate with the Rust one — and this
/// side is the one that does not cost a round trip per episode.
fn is_covered(coverage: &[grabs::Coverage], item_id: Uuid, target: grabs::GrabTarget) -> bool {
    coverage
        .iter()
        .any(|row| row.item_id == item_id && row.covers(target))
}

/// The ceiling as configured, floored.
///
/// Read fresh every cycle so an edit in `/settings` lands on the next
/// tick rather than at the next restart — the same hot-reload contract
/// the poller and the \*arr sweep have. A blank, missing or unreadable
/// setting is the default, not a stall: this task has to keep sweeping
/// while the DB hiccups.
async fn configured_budget(pool: &crate::db::Pool) -> usize {
    let stored = match crate::db::settings::get(
        pool,
        crate::db::settings::KEY_SCAN_SEARCHES_PER_CYCLE,
    )
    .await
    {
        Ok(Some(row)) => row.value.trim().parse::<usize>().ok(),
        Ok(None) => None,
        Err(e) => {
            warn!(
                target: "brarr_orchestrator::scan",
                error = %e,
                "could not read the search ceiling; using the default"
            );
            None
        }
    };
    stored
        .unwrap_or(DEFAULT_SEARCHES_PER_CYCLE)
        .max(MIN_SEARCHES_PER_CYCLE)
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
    scan_item(state, item, Scope::Item).await
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
    scan_item(state, item, scope).await
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

/// Sweep one item, ignoring the per-cycle ceiling and the rotation.
///
/// Both exist to share a bounded budget fairly across a whole catalogue,
/// and neither has anything to say when the operator has pointed at one
/// title: there is nothing to be fair to.
async fn scan_item(
    state: &AppState,
    item: &LibraryItem,
    scope: Scope,
) -> Result<ScanSummary, AppError> {
    let mut summary = ScanSummary::default();
    let profile = resolve_profile(state, item).await?;
    let coverage = grabs::live_coverage_for_item(state.pool(), item.id).await?;
    let plan = build_targets(state, item, scope).await?;
    summary.absorb(&plan);

    for target in &plan.targets {
        summary.targets += 1;
        if is_covered(&coverage, item.id, target.grab_target()) {
            summary.skipped_covered += 1;
            continue;
        }
        summary.searches += 1;
        run_target(state, item, target, profile.as_ref(), &mut summary).await?;
    }
    Ok(summary)
}

/// Search one target and act on what came back.
///
/// Shared by the scheduled sweep and the buttons, so the two can never
/// judge the same release differently — the caller has already decided
/// this target is worth a slot and counted the search.
async fn run_target(
    state: &AppState,
    item: &LibraryItem,
    target: &Target,
    profile: Option<&QualityProfileRow>,
    summary: &mut ScanSummary,
) -> Result<(), AppError> {
    // Stamped at dispatch rather than on success, and that is the whole
    // point: a target whose search errors, or finds nothing, has to move
    // down the queue too. Recording only successes would leave exactly
    // the un-findable targets at the head forever, which is the defect
    // this rotation exists to close.
    //
    // Best-effort, like `record_provider_metric`: bookkeeping does not
    // get to fail a sweep.
    if let Err(e) = scan_attempts::record(
        state.pool(),
        item.id,
        target.episode.as_ref().map(|e| e.id),
        OffsetDateTime::now_utc(),
    )
    .await
    {
        warn!(target: "brarr_orchestrator::scan", error = %e, "could not record the search attempt");
    }

    let outcome = match search::run_search(state, target.keys.clone()).await {
        Ok(o) => o,
        Err(e) => {
            summary.failures.push((target.label.clone(), e.to_string()));
            return Ok(());
        }
    };
    // The column exists for exactly this: an ad-hoc search leaves it
    // NULL, a sweep names the item that asked for it.
    if let Err(e) = searches::attach_library_item(state.pool(), outcome.search.id, item.id).await {
        warn!(target: "brarr_orchestrator::scan", error = %e, "could not tie the search to its item");
    }

    let candidates = pick_candidates(&outcome.decisions, profile, target);
    if candidates.is_empty() {
        summary.no_candidate += 1;
        return Ok(());
    }
    match take_first_available(state, item, target, &candidates).await? {
        TargetOutcome::Grabbed => summary.grabbed += 1,
        TargetOutcome::Nothing => summary.exhausted += 1,
        TargetOutcome::Failed(reason) => {
            summary.failures.push((target.label.clone(), reason));
        }
    }
    Ok(())
}

/// What happened to one target after candidates were picked.
#[derive(Debug)]
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
///
/// **The axis comes from the identity set, through the one resolver that
/// reports what it could not use.** Reading the three columns here meant
/// a film whose id would not convert left the sweep via
/// `movie_target(item).into_iter().collect()` — no counter, no log, and
/// a screen that went on saying "faltando" while the badge blamed the
/// trackers. `metadata::axis::resolve` hands back both halves, and a
/// caller that wants to ignore a rejection has to say so.
async fn build_targets(
    state: &AppState,
    item: &LibraryItem,
    scope: Scope,
) -> Result<TargetPlan, AppError> {
    let ids = item_ids::for_item(state.pool(), item.id).await?;
    let (axis, rejected) = axis::resolve(&ids, item.media_type);

    match item.media_type {
        // A movie has no seasons, so a narrowed sweep of one is a
        // contradiction rather than an empty result — refuse it instead
        // of quietly searching the whole film.
        MediaType::Movie if scope.is_narrowed() => Ok(TargetPlan::default()),
        MediaType::Movie => {
            // A film that is not out is not missing, and asking every
            // tracker about it each cycle is the same traffic
            // `episode_targets` refuses to generate for an unaired
            // episode. `coverage::movie_progress` has painted these "não
            // estreou" on the shelf since v0.7.1 while the sweep chased
            // them every half hour — the season 0 asymmetry again,
            // pointing the other way: there the screen counted what the
            // sweep would not touch, here the sweep chases what the
            // screen already refuses to call a gap.
            if !movie_is_out(item, OffsetDateTime::now_utc()) {
                return Ok(TargetPlan {
                    skipped_unreleased: 1,
                    rejected,
                    ..TargetPlan::default()
                });
            }
            let Some(target) = movie_target(item, &axis) else {
                return Ok(TargetPlan {
                    no_search_axis: true,
                    rejected,
                    ..TargetPlan::default()
                });
            };
            Ok(TargetPlan {
                targets: vec![target],
                ..TargetPlan::default()
            })
        }
        MediaType::Tv => {
            // TVDB is the only per-episode axis any indexer speaks, so a
            // series holding a TMDB id and nothing else is searchable as
            // a title and not as an episode — which is why this asks
            // `can_search_episodes` rather than `is_empty`.
            let Some(tvdb) = axis.tvdb else {
                debug!(
                    target: "brarr_orchestrator::scan",
                    item = %item.title,
                    rejected = rejected.len(),
                    "no per-episode search axis"
                );
                return Ok(TargetPlan {
                    no_search_axis: true,
                    rejected,
                    ..TargetPlan::default()
                });
            };
            let episodes = library::episodes(state.pool(), item.id).await?;
            Ok(episode_targets(
                item,
                &episodes,
                tvdb,
                OffsetDateTime::now_utc(),
                scope,
            ))
        }
    }
}

/// Whether a film exists to be found yet.
///
/// Two questions, and the order matters. **Production status first**: a
/// film still being made carries no release date of any kind, so a date
/// test alone reads "no date ⇒ out" and sends the sweep after a 2027
/// sequel every half hour — measured on this operator's catalogue, two of
/// the seventeen wanted films. Then the digital date, which is when a
/// film becomes findable at all; the theatrical window is exactly what
/// the detail page already warns about.
///
/// **A missing date on a released film still counts as out**, which is
/// the default [`crate::coverage::movie_progress`] documents: TMDB
/// carries no digital date for most older films, and the opposite
/// default would take half a back catalogue out of the sweep.
///
/// The `match` is exhaustive rather than a two-arm test, so a seventh
/// production status has to be placed here instead of quietly joining
/// whichever side the wildcard fell on.
fn movie_is_out(item: &LibraryItem, now: OffsetDateTime) -> bool {
    if let Some(status) = item.status {
        match status {
            // Announced, or being made. There is nothing to find, and
            // there is no date to reason about either.
            ProductionStatus::InProduction | ProductionStatus::Announced => return false,
            // `Released` is the film answer. The three series values
            // cannot describe a film at all, so a row carrying one is a
            // mis-mapped provider — which is a reason to look at the
            // metadata, not a reason to stop searching for the film.
            ProductionStatus::Released
            | ProductionStatus::Returning
            | ProductionStatus::Ended
            | ProductionStatus::Cancelled => {}
        }
    }
    item.digital_release_at.is_none_or(|date| date <= now)
}

/// Search axis for a movie. `None` when the item carries no usable id,
/// which cannot happen through the TMDB add path but can through a
/// hand-edited row or an id from a source no indexer accepts.
fn movie_target(item: &LibraryItem, axis: &axis::SearchAxis) -> Option<Target> {
    if axis.tmdb.is_none() && axis.imdb.is_none() {
        return None;
    }
    Some(Target {
        label: item.title.clone(),
        episode: None,
        keys: SearchKeys {
            tmdb: axis.tmdb,
            imdb: axis.imdb,
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
/// **The stored coordinate is the one asked for.** It used to be
/// translated on the way out — TMDB models Jujutsu Kaisen as one season
/// of 59, so brarr asked for `S01E35` while every release was named
/// `S02E23` — and the translation is gone because the tree is now built
/// by whoever numbers it the way releases do. A series still born under
/// TMDB is one TheTVDB does not have, which is also one whose two
/// numberings do not disagree.
fn episode_targets(
    item: &LibraryItem,
    episodes: &[Episode],
    tvdb: TvdbId,
    now: OffsetDateTime,
    scope: Scope,
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
        let (Ok(season), Ok(number)) = (
            u16::try_from(e.season_number),
            u16::try_from(e.episode_number),
        ) else {
            continue;
        };
        plan.targets.push(Target {
            label: format!("{} S{season:02}E{number:02}", item.title),
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
    /// A film that is not out yet. Always 0 or 1 — a film is one target.
    skipped_unreleased: usize,
    /// No usable search id on the item at all.
    no_search_axis: bool,
    /// Ids brarr holds and cannot search with, so the badge can name the
    /// cause instead of blaming the trackers.
    rejected: Vec<axis::AxisRejection>,
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
    use brarr_core::{ImdbId, MetadataSource, TmdbId};
    use std::collections::HashMap;
    use uuid::Uuid;

    fn item(media_type: MediaType) -> LibraryItem {
        LibraryItem {
            id: Uuid::new_v4(),
            media_type,
            title: "The Matrix".to_owned(),
            original_title: None,
            year: Some(1999),
            overview: None,
            poster_source: None,
            backdrop_source: None,
            descriptive_source: None,
            poster_path: None,
            backdrop_path: None,
            status: None,
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

    /// The axis a film's identity resolves to, built the way
    /// `build_targets` builds it.
    fn stored(source: MetadataSource, value: &str) -> item_ids::StoredId {
        item_ids::StoredId {
            id: brarr_core::ExternalId::new(source, value).unwrap(),
            verification: item_ids::Verification::Asserted,
        }
    }

    fn movie_target_for(item: &LibraryItem) -> Target {
        let ids = [
            stored(MetadataSource::Tmdb, "603"),
            stored(MetadataSource::Imdb, "tt0133093"),
        ];
        let (axis, rejected) = axis::resolve(&ids, MediaType::Movie);
        assert!(rejected.is_empty(), "the fixture carries usable ids");
        movie_target(item, &axis).expect("the fixture carries both ids")
    }

    #[test]
    fn a_movie_searches_on_both_axes() {
        let target = movie_target_for(&item(MediaType::Movie));
        assert_eq!(target.keys.tmdb.map(TmdbId::get), Some(603));
        assert_eq!(target.keys.imdb.map(ImdbId::get), Some(133_093));
        assert!(target.episode_marker.is_none());
    }

    /// **A film with no usable id leaves a counter behind.** It used to
    /// leave nothing: `movie_target(item).into_iter().collect()` dropped
    /// the `None`, so the title rendered "faltando" forever and the badge
    /// blamed the trackers for a problem in the catalogue.
    #[test]
    fn a_film_with_no_usable_id_is_refused_and_the_reason_is_nameable() {
        let (axis, rejected) = axis::resolve(&[], MediaType::Movie);
        assert!(movie_target(&item(MediaType::Movie), &axis).is_none());
        assert!(rejected.is_empty(), "nothing held is nothing to report");

        // And one brarr *does* hold and cannot use says which.
        let held = [stored(MetadataSource::Tvdb, "355567")];
        let (axis, rejected) = axis::resolve(&held, MediaType::Tv);
        assert!(axis.can_search_episodes(), "a series can use a TVDB id");
        assert!(rejected.is_empty());
    }

    /// **The query, the marker and the identity are one coordinate.**
    /// They used to be two: the tree was TMDB's and the search had to be
    /// translated out of it, which is the arrangement Jujutsu Kaisen
    /// broke — one season of 59 on TMDB, `S02E23` on every release,
    /// `S01E47` asked for, every candidate refused. A tree built by
    /// whoever numbers it the way releases do has nothing to translate,
    /// and this test is what would fail if a translation came back.
    #[test]
    fn the_stored_coordinate_is_the_one_asked_for() {
        let series = item(MediaType::Tv);
        let now = OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap();
        let episodes = vec![episode(2, 23, true, Some(1_600_000_000))];
        let tvdb = TvdbId::new(70_726).unwrap();

        let plan = episode_targets(&series, &episodes, tvdb, now, Scope::Item);
        let target = &plan.targets[0];

        assert_eq!(target.keys.season, Some(2), "the indexer is asked for S02");
        assert_eq!(target.keys.episode, Some(23));
        assert_eq!(
            target.episode_marker,
            Some((2, 23)),
            "and the title must carry the marker the release actually uses"
        );
        assert_eq!(target.label, "The Matrix S02E23");

        let held = target.episode.as_ref().unwrap();
        assert_eq!(held.season_number, 2);
        assert_eq!(held.episode_number, 23);
    }

    /// The two IMDb conventions meet in `ExternalId`'s constructor now,
    /// not in a helper each caller reimplemented. `library_items.imdb_id`
    /// kept the `tt` and `searches.imdb_id` keeps the bare number.
    #[test]
    fn the_imdb_prefix_is_stripped_for_the_search_axis() {
        for written in ["tt0133093", "133093", "tt133093"] {
            let (axis, rejected) =
                axis::resolve(&[stored(MetadataSource::Imdb, written)], MediaType::Movie);
            assert!(rejected.is_empty(), "{written}");
            assert_eq!(axis.imdb.map(ImdbId::get), Some(133_093), "{written}");
        }
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
        let targets = episode_targets(&series, &episodes, tvdb, now, Scope::Item).targets;
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
        let plan = episode_targets(&series, &episodes, tvdb, now, Scope::Item);

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
        let plan = episode_targets(&series, &episodes, tvdb, now, Scope::Season(0));

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
        let target = &episode_targets(&series, &episodes, tvdb, now, Scope::Item).targets[0];

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
        let mut summary = ScanSummary {
            targets: 2,
            skipped_unaired: 1,
            ..ScanSummary::default()
        };
        summary.absorb(&TargetPlan {
            skipped_unmonitored: 3,
            skipped_unaired: 4,
            skipped_unreleased: 1,
            no_search_axis: true,
            ..TargetPlan::default()
        });
        assert_eq!(summary.targets, 2, "absorb only takes the skip counters");
        assert_eq!(summary.skipped_unmonitored, 3);
        assert_eq!(summary.skipped_unaired, 5, "additive across items");
        assert_eq!(summary.skipped_unreleased, 1);
        assert!(summary.no_search_axis);
    }

    // ---- the release gate on the film path -----------------------------

    fn film(status: Option<ProductionStatus>, digital: Option<OffsetDateTime>) -> LibraryItem {
        LibraryItem {
            status,
            digital_release_at: digital,
            ..item(MediaType::Movie)
        }
    }

    /// The traffic `episode_targets` has always refused to generate, on
    /// the path that never had the check.
    ///
    /// Measured live before this existed: of seventeen wanted films, the
    /// sweep was asking every configured tracker every thirty minutes
    /// about a 2027 sequel that is still being shot and a sequel whose
    /// digital release was four days away — while `coverage` painted both
    /// "não estreou" on the shelf.
    #[test]
    fn a_film_that_is_not_out_yet_is_not_searched() {
        let now = OffsetDateTime::now_utc();
        let soon = now + time::Duration::days(4);
        let past = now - time::Duration::days(400);

        assert!(
            !movie_is_out(&film(Some(ProductionStatus::Released), Some(soon)), now),
            "a digital release still ahead of us is nothing to find"
        );
        assert!(
            !movie_is_out(&film(Some(ProductionStatus::InProduction), None), now),
            "still being shot, and carrying no date to reason about"
        );
        assert!(
            !movie_is_out(&film(Some(ProductionStatus::Announced), None), now),
            "announced and not shot"
        );
        assert!(
            movie_is_out(&film(Some(ProductionStatus::Released), Some(past)), now),
            "out, and out a while"
        );
        assert!(
            movie_is_out(&film(Some(ProductionStatus::Released), None), now),
            "TMDB carries no digital date for most older films; the \
             opposite default would take half a back catalogue out"
        );
        assert!(
            movie_is_out(&film(None, None), now),
            "an unknown status is not a reason to stop searching"
        );
    }

    // ---- the queue -----------------------------------------------------

    fn wanted_episode(
        series: &LibraryItem,
        season: i32,
        number: i32,
        aired_days_ago: i64,
        searched_days_ago: Option<i64>,
        now: OffsetDateTime,
    ) -> Wanted<'_> {
        let mut ep = episode(season, number, true, None);
        ep.air_date = Some(now - time::Duration::days(aired_days_ago));
        Wanted {
            tier: tier_of(series, Some(&ep), now),
            last_searched: searched_days_ago.map(|d| now - time::Duration::days(d)),
            target: Target {
                label: format!("{} S{season:02}E{number:02}", series.title),
                episode: Some(ep),
                keys: SearchKeys::default(),
                episode_marker: None,
            },
            item: series,
        }
    }

    /// The regression this whole rotation exists for, in the shape it was
    /// measured in.
    ///
    /// One title contributed twelve of the twenty-five slots a cycle had:
    /// a season whose files are on disk under a different cut of the show,
    /// so those gaps can never close and every search for them finds
    /// nothing — forever, since a target is wanted precisely because
    /// nothing was found. Under the old fixed order (season, then
    /// episode) the episode that aired two days earlier sat behind all of
    /// them and was never searched once, in any cycle.
    #[test]
    fn a_freshly_aired_episode_outranks_a_backlog_that_would_never_end() {
        let series = item(MediaType::Tv);
        let now = OffsetDateTime::now_utc();
        let mut queue: Vec<Wanted<'_>> = (14..=25)
            .map(|n| wanted_episode(&series, 1, n, 3500, Some(1), now))
            .collect();
        queue.push(wanted_episode(&series, 4, 12, 2, None, now));

        sort_queue(&mut queue);

        assert_eq!(queue[0].tier, Tier::Fresh);
        assert_eq!(
            queue[0].target.label, "The Matrix S04E12",
            "the episode the operator is actually waiting for"
        );
    }

    /// Never-searched ahead of searched, and older attempts ahead of
    /// newer ones. `Option`'s own ordering says the first half, which is
    /// why it is not written a second time in `sort_queue`.
    #[test]
    fn the_queue_rotates_least_recently_searched_first() {
        let series = item(MediaType::Tv);
        let now = OffsetDateTime::now_utc();
        let mut queue = vec![
            wanted_episode(&series, 1, 1, 900, Some(1), now),
            wanted_episode(&series, 1, 2, 900, None, now),
            wanted_episode(&series, 1, 3, 900, Some(30), now),
        ];

        sort_queue(&mut queue);

        let order: Vec<&str> = queue.iter().map(|w| w.target.label.as_str()).collect();
        assert_eq!(
            order,
            vec![
                "The Matrix S01E02",
                "The Matrix S01E03",
                "The Matrix S01E01"
            ],
            "never searched, then the oldest attempt, then the newest"
        );
    }

    /// Freshness is decided by the date, not by the tier being a nicer
    /// place to sit: a film with no digital date at all is most of a back
    /// catalogue, and putting it in front of what aired this week would
    /// undo the tier.
    #[test]
    fn a_film_with_no_release_date_is_backlog_not_fresh() {
        let now = OffsetDateTime::now_utc();
        let old = film(Some(ProductionStatus::Released), None);
        assert_eq!(tier_of(&old, None, now), Tier::Backlog);

        let just_out = film(
            Some(ProductionStatus::Released),
            Some(now - time::Duration::days(3)),
        );
        assert_eq!(tier_of(&just_out, None, now), Tier::Fresh);

        let last_year = film(
            Some(ProductionStatus::Released),
            Some(now - time::Duration::days(365)),
        );
        assert_eq!(tier_of(&last_year, None, now), Tier::Backlog);
    }

    /// A share is a floor for the tier that needs it and never a cap on
    /// the cycle — the arithmetic both directions depend on.
    #[test]
    fn the_budget_splits_without_leaving_slots_unspent() {
        // The measured case: the fresh tier alone exceeds the ceiling, so
        // strict priority would leave the backlog at zero forever.
        assert_eq!(split_budget(25, 30, 249), (17, 8));

        // A quiet week. The backlog takes what fresh cannot use, rather
        // than the cycle running at 30% for want of new episodes.
        assert_eq!(split_budget(25, 3, 249), (3, 22));
        assert_eq!(split_budget(25, 0, 5), (0, 5));

        // And the mirror: a nearly-empty backlog does not hold slots the
        // fresh tier could spend.
        assert_eq!(split_budget(25, 30, 2), (23, 2));

        // Nothing wanted at all, and a budget of one — where integer
        // rounding would otherwise hand the fresh tier zero.
        assert_eq!(split_budget(25, 0, 0), (0, 0));
        assert_eq!(split_budget(1, 5, 5), (1, 0));
    }

    /// The ceiling is hot-reloadable, and the one value it must never
    /// take is zero — that is a paused scanner spelled a second way, and
    /// `KEY_PAUSED` is the one that says so.
    #[tokio::test]
    async fn the_ceiling_is_read_fresh_and_floored() {
        let pool = crate::db::open_memory().await.expect("open in-memory db");
        assert_eq!(
            configured_budget(&pool).await,
            DEFAULT_SEARCHES_PER_CYCLE,
            "an unset key is the default, not a stall"
        );

        let key = crate::db::settings::KEY_SCAN_SEARCHES_PER_CYCLE;
        crate::db::settings::set(&pool, key, "60").await.unwrap();
        assert_eq!(configured_budget(&pool).await, 60);

        crate::db::settings::set(&pool, key, "0").await.unwrap();
        assert_eq!(configured_budget(&pool).await, MIN_SEARCHES_PER_CYCLE);

        crate::db::settings::set(&pool, key, "  ").await.unwrap();
        assert_eq!(
            configured_budget(&pool).await,
            DEFAULT_SEARCHES_PER_CYCLE,
            "blanked from the UI means the default"
        );
    }

    /// The operator's report, reduced to its mechanism.
    ///
    /// The automatic search on one episode answered "nada encontrado"
    /// while the interactive search on the *same* episode listed nine
    /// releases. Nothing was wrong with the trackers, the profile or the
    /// threshold: seven earlier sweeps had each grabbed a different
    /// release for that episode, every one of them had failed at import,
    /// and `failed` keeps its barrier key on purpose. So every candidate
    /// was refused, `take_first_available` returned `Nothing`, and that
    /// landed in the same counter as "nothing passed the threshold".
    ///
    /// This pins the counter. The badge built on it is pinned in
    /// `web::routes::tests`.
    #[tokio::test]
    async fn every_candidate_already_spent_is_exhausted_not_no_candidate() {
        use crate::db::grabs::{GrabStatus, NewGrab, Protocol};

        let pool = crate::db::open_memory().await.expect("open in-memory db");
        let stored = library::upsert(
            &pool,
            &crate::db::seed::Seed::movie(603, "The Matrix").build(),
        )
        .await
        .unwrap();
        let provider = crate::db::providers::insert(
            &pool,
            crate::db::providers::NewProvider {
                name: "capybara",
                base_url: &url::Url::parse("https://capybarabr.com/").unwrap(),
                api_token: "tok",
                kind: "unit3d",
                plugin_path: None,
            },
        )
        .await
        .unwrap();

        let mut candidate = decision("Matrix.1999.1080p", 500, 10);
        candidate.provider_id = Some(provider.id);

        // The earlier sweep: this exact release was taken and it failed
        // for good, so its key stays occupied.
        let spent = grabs::reserve(
            &pool,
            &NewGrab {
                item_id: stored.id,
                episode_id: None,
                season_number: None,
                decision_id: None,
                provider_id: provider.id,
                provider_name: "capybara",
                release_id_remote: &candidate.stable_release_key(),
                release_name: &candidate.release_name,
                download_url: None,
                protocol: Protocol::Torrent,
            },
        )
        .await
        .unwrap()
        .expect("the first sweep wins the reservation");
        grabs::set_status(&pool, spent.id, GrabStatus::Failed, Some("no import"))
            .await
            .unwrap();

        let state = AppState::new(pool, brarr_decision_service::Engine::baseline());
        let target = movie_target_for(&item(MediaType::Movie));
        let outcome = take_first_available(&state, &stored, &target, &[&candidate])
            .await
            .unwrap();

        assert!(
            matches!(outcome, TargetOutcome::Nothing),
            "the barrier refuses a release an earlier sweep burned — got {outcome:?}"
        );
        // Nothing reached a download client, so the delivery path was
        // never entered: this asserts the barrier, not the network.
        assert_eq!(
            grabs::for_item(state.pool(), stored.id)
                .await
                .unwrap()
                .len(),
            1,
            "a refused reservation must not leave a second row behind"
        );
    }
}
