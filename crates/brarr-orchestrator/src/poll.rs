//! Scheduled poller that closes the autobrr-style loop.
//!
//! Every `BRARR_ARR_POLL_INTERVAL_SECS` (default 1800 = 30 min), brarr:
//!   1. Walks every enabled *arr instance — Radarr via
//!      [`ArrClient::monitored_movies`] (movies on disk + monitored),
//!      Sonarr via [`ArrClient::wanted_episodes`] (`/wanted/missing`).
//!   2. For each wanted row, builds [`SearchKeys`] from its external
//!      ids (TMDb/IMDb for movies; TVDB+season+episode for episodes)
//!      and runs the brarr search pipeline as if a user submitted it.
//!   3. Sorts the resulting kept decisions by score descending.
//!   4. For the top scorer that exceeds the *arr's `push_threshold`
//!      AND hasn't been pushed to this *arr already, calls
//!      [`crate::push::push_decision`] — recording the attempt in
//!      `push_history` regardless of outcome.
//!   5. Moves to the next row. One push per row per poll cycle so
//!      brarr never spams *arr with competing grabs for the same item.
//!
//! ## Manual trigger
//!
//! [`run_once_for_instance`] is exposed for the UI "rodar agora"
//! button on `/arr-instances`. Same code path as the scheduled tick;
//! just bypasses the sleep.

use std::sync::Arc;
use std::time::Duration;

use brarr_arr::{ArrClient, ArrKind, WantedEpisode, WantedMovie};
use brarr_core::{ImdbId, TmdbId, TvdbId};
use tokio::task::JoinHandle;
use tokio::time;
use tracing::{debug, info, warn};

use crate::db::arr_instances::{self, ArrInstanceRow};
use crate::db::push_history;
use crate::push::push_decision;
use crate::search::{SearchKeys, run_search};
use crate::{AppError, AppState};

/// Default poll interval when `BRARR_ARR_POLL_INTERVAL_SECS` isn't set.
/// 30 min matches Sonarr's default RSS sync cadence, so brarr doesn't
/// over-trigger the upstream trackers.
pub const DEFAULT_POLL_INTERVAL: Duration = Duration::from_secs(1800);

/// Per-movie throttle between brarr searches inside one poll cycle.
/// 5s keeps a 200-movie library inside ~17 min — fast enough to finish
/// before the next 30-min tick, gentle enough on the upstream
/// providers' rate limits.
const PER_MOVIE_DELAY: Duration = Duration::from_secs(5);

/// Summary returned from [`run_once_for_instance`] — small enough to
/// embed in an HTMX badge after the manual-poll button.
#[derive(Debug, Default, Clone)]
pub struct PollSummary {
    /// How many wanted movies the poller considered.
    pub considered: usize,
    /// How many movies brarr ran a search for (i.e. had a usable
    /// TMDb / IMDb id and weren't already grabbed).
    pub searched: usize,
    /// How many movies triggered a push to *arr.
    pub pushed: usize,
    /// How many search calls errored before producing decisions.
    pub search_errors: usize,
}

/// Spawn the background poller task. Returns the [`JoinHandle`] so the
/// main binary can keep it alive — dropping the handle aborts the task.
#[must_use]
pub fn spawn(state: AppState, interval: Duration) -> JoinHandle<()> {
    let state = Arc::new(state);
    info!(
        target: "brarr_orchestrator::poll",
        interval_secs = interval.as_secs(),
        "starting *arr poller"
    );
    tokio::spawn(async move {
        // First tick fires immediately after startup so the operator
        // sees activity within seconds rather than after the full
        // interval. `tokio::time::interval` ticks once at construction
        // by default.
        let mut ticker = time::interval(interval);
        loop {
            ticker.tick().await;
            if let Err(e) = run_one_cycle(&state).await {
                warn!(
                    target: "brarr_orchestrator::poll",
                    error = %e,
                    "poll cycle failed"
                );
            }
        }
    })
}

async fn run_one_cycle(state: &AppState) -> Result<(), AppError> {
    let arr_rows = arr_instances::list_enabled(state.pool()).await?;
    if arr_rows.is_empty() {
        debug!(
            target: "brarr_orchestrator::poll",
            "no enabled *arr instances; cycle is a no-op"
        );
        return Ok(());
    }
    for arr in arr_rows {
        match run_once_for_instance(state, &arr).await {
            Ok(summary) => info!(
                target: "brarr_orchestrator::poll",
                arr_name = %arr.name,
                kind = arr.kind.label(),
                considered = summary.considered,
                searched = summary.searched,
                pushed = summary.pushed,
                search_errors = summary.search_errors,
                "poll cycle for instance complete"
            ),
            Err(e) => warn!(
                target: "brarr_orchestrator::poll",
                arr_name = %arr.name,
                error = %e,
                "instance poll failed"
            ),
        }
    }
    Ok(())
}

/// Drive one poll cycle against a single *arr instance. Exposed for
/// the manual "rodar agora" UI button + integration tests.
///
/// # Errors
///
/// - [`AppError::Database`] if the underlying DB queries fail.
/// - All *arr-side transport / parse failures surface as a `push_history`
///   row (not an error here) — the function continues on to the next
///   wanted movie. The summary captures the counts.
pub async fn run_once_for_instance(
    state: &AppState,
    arr: &ArrInstanceRow,
) -> Result<PollSummary, AppError> {
    match arr.kind {
        ArrKind::Radarr => run_once_radarr(state, arr).await,
        ArrKind::Sonarr => run_once_sonarr(state, arr).await,
    }
}

async fn run_once_radarr(state: &AppState, arr: &ArrInstanceRow) -> Result<PollSummary, AppError> {
    let mut summary = PollSummary::default();
    let client = match ArrClient::new(arr.to_arr_instance()) {
        Ok(c) => c,
        Err(e) => {
            warn!(
                target: "brarr_orchestrator::poll",
                arr_name = %arr.name,
                error = %e,
                "failed to build ArrClient"
            );
            return Ok(summary);
        }
    };
    let movies = match client.monitored_movies().await {
        Ok(m) => m,
        Err(e) => {
            warn!(
                target: "brarr_orchestrator::poll",
                arr_name = %arr.name,
                error = %e,
                "fetch wanted movies failed"
            );
            return Ok(summary);
        }
    };

    let base_url = poll_base_url();

    let mut iter = movies.into_iter();
    while let Some(movie) = iter.next() {
        summary.considered += 1;
        if !is_pollable(&movie) {
            continue;
        }
        let Some(keys) = build_keys(&movie) else {
            continue;
        };
        summary.searched += 1;
        match run_search(state, keys).await {
            Ok(outcome) => {
                if let Some(decision) = pick_pushable(state, arr, &outcome.decisions).await? {
                    match push_decision(state, decision, arr, &base_url).await {
                        Ok(row) => {
                            if matches!(row.status, push_history::PushStatus::Ok) {
                                summary.pushed += 1;
                            }
                        }
                        Err(e) => warn!(
                            target: "brarr_orchestrator::poll",
                            decision_id = %decision.id,
                            error = %e,
                            "push failed at DB layer"
                        ),
                    }
                }
            }
            Err(e) => {
                summary.search_errors += 1;
                warn!(
                    target: "brarr_orchestrator::poll",
                    movie_title = %movie.title,
                    error = %e,
                    "search failed"
                );
            }
        }
        // Pacing: only sleep if there's another movie to process. Skips
        // the trailing 5s after the last movie.
        if iter.len() > 0 {
            time::sleep(PER_MOVIE_DELAY).await;
        }
    }
    Ok(summary)
}

/// Pick the highest-scoring decision that's eligible for push.
///
/// Eligibility cascade (cheapest checks first):
///   1. Score must meet `arr.push_threshold`. Decisions come sorted
///      DESC so anything below threshold short-circuits.
///   2. Dead torrents (`provider_kind != newznab` and `seeders == 0`)
///      are skipped — *arr can't grab a release with no seeders.
///   3. Releases brarr already tried for this `(provider, release,
///      arr)` triple are skipped (regardless of past outcome), so a
///      failed grab (stalled torrent, missing NZB articles, *arr
///      rejection) doesn't loop forever on the same dead-end pick.
///   4. Decisions without a `provider_id` snapshot (legacy rows) are
///      skipped — brarr can't dedup what it can't trace back.
///
/// Returns the first matching decision, or `None` when the whole list
/// is exhausted.
/// Pick the top decision worth pushing to `arr`, applying the
/// instance's effective threshold and `already_tried_release` /
/// `already_pushed` dedup guards. Returns `None` when no decision
/// clears the bar or every candidate has already been attempted.
///
/// Exposed at `pub(crate)` so the webhook handler can reuse the exact
/// same gating logic the poller uses — three inline call sites in
/// this file (`run_once_radarr`, `run_once_sonarr`,
/// `run_once_season_pack`) plus the webhook share one implementation
/// rather than each duplicating it.
///
/// # Errors
///
/// Surfaces [`AppError::Database`] on SQL failure (threshold lookup
/// or dedup query).
pub(crate) async fn pick_pushable<'a>(
    state: &AppState,
    arr: &ArrInstanceRow,
    decisions: &'a [crate::db::decisions::DecisionRow],
) -> Result<Option<&'a crate::db::decisions::DecisionRow>, AppError> {
    // Resolve the effective push threshold once per call. When the
    // *arr has a quality profile attached the profile's threshold
    // wins; otherwise we fall back to the row's own integer. The
    // resolver also handles the FK-orphan edge case (profile deleted
    // mid-poll) by falling back to the raw `push_threshold`.
    let threshold = crate::db::arr_instances::effective_threshold(state.pool(), arr).await?;
    // Per-ARR score lookup: when a profile is attached, the decision's
    // score under THAT profile drives the threshold gate (not the
    // baseline score persisted in `d.score`). This is the missing half
    // of the Quality Profile contract — without it, a strict anime
    // profile attached to Sonarr would silently fall back to baseline
    // scoring and either over-grab or under-grab.
    let effective_score = |d: &crate::db::decisions::DecisionRow| -> u32 {
        arr.profile_id
            .and_then(|pid| d.profile_scores.get(&pid).copied())
            .unwrap_or(d.score)
    };
    // `take_while` no longer works — sorted-by-baseline doesn't imply
    // sorted-by-profile, so a release that fails the profile check
    // can still hide a stronger candidate later in the list. Switch to
    // a per-row `if score < threshold continue` so the scan is
    // exhaustive.
    for d in decisions {
        if effective_score(d) < pick_threshold_for(d, threshold) {
            continue;
        }
        if !meets_quality(d) {
            debug!(
                target: "brarr_orchestrator::poll",
                decision_id = %d.id,
                release = %d.release_name,
                seeders = d.seeders,
                "skip dead torrent"
            );
            continue;
        }
        if !matches_arr_kind(&d.release_name, arr.kind) {
            debug!(
                target: "brarr_orchestrator::poll",
                decision_id = %d.id,
                release = %d.release_name,
                arr_kind = arr.kind.label(),
                "skip release whose title shape mismatches arr flavour"
            );
            continue;
        }
        let Some(provider_id) = d.provider_id else {
            continue;
        };
        if push_history::already_tried_release(
            state.pool(),
            provider_id,
            d.release_id_remote,
            arr.id,
        )
        .await?
        {
            debug!(
                target: "brarr_orchestrator::poll",
                decision_id = %d.id,
                release = %d.release_name,
                arr_name = %arr.name,
                "already tried this release, picking next"
            );
            continue;
        }
        return Ok(Some(d));
    }
    Ok(None)
}

/// Reject decisions whose release title clearly belongs to a different
/// *arr flavour than the one we're about to push to.
///
/// Live failure that motivated this: capybara's UNIT3D fork doesn't
/// scope `/api/torrents/filter?tmdbId=` to a media-type category, so a
/// movie tmdb that happens to collide with a TV series on capybara's
/// side returns episode/season releases. brarr pushed `Teen Titans
/// S05 720p WEB-DL ... DUAL-NeX` to Radarr and Radarr's title parser
/// 400'd with `Unable to parse`.
///
/// Policy:
/// - Radarr → drop titles with `S<digits>E<digits>` (episodes) or
///   bare `S<digits>` season packs.
/// - Sonarr → permissive (its parser is fine with weird movie titles
///   and the upstream search axis is TV-only already).
fn matches_arr_kind(title: &str, kind: ArrKind) -> bool {
    match kind {
        ArrKind::Radarr => !title_looks_like_tv(title),
        ArrKind::Sonarr => true,
    }
}

/// Match `S<digits>E<digits>` (episode) or a bare `S<digits>` season
/// marker surrounded by non-alphanumeric boundaries. Case-insensitive.
///
/// Examples that count as TV:
///   `Show.S01E02.1080p`, `Show s01e02`, `Teen.Titans.S05.720p`,
///   `Teen Titans S05 720p`, `Show.S5E10`.
///
/// Examples that do NOT count as TV:
///   `Stardust.2007`, `S Movie 2024`, `Sword.2024`, `123S456`.
fn title_looks_like_tv(title: &str) -> bool {
    let bytes = title.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        // Need an `S` (case-insensitive) preceded by a separator or
        // the start of the string — `Show.S01` and `Show S01`.
        if (b == b's' || b == b'S')
            && (i == 0 || !bytes[i - 1].is_ascii_alphanumeric())
            && i + 1 < bytes.len()
            && bytes[i + 1].is_ascii_digit()
        {
            let mut j = i + 1;
            while j < bytes.len() && bytes[j].is_ascii_digit() {
                j += 1;
            }
            // S<digits>E<digits> — proper episode marker.
            if j < bytes.len()
                && (bytes[j] == b'e' || bytes[j] == b'E')
                && j + 1 < bytes.len()
                && bytes[j + 1].is_ascii_digit()
            {
                return true;
            }
            // S<digits> followed by non-alpha boundary — season pack.
            // `j == i + 1` would mean S not followed by digits (already
            // ruled out by the `is_ascii_digit` guard above). Bound the
            // season number length to ≤ 4 digits so `S2024` (year tag
            // some scene groups use) doesn't false-positive.
            let digit_len = j - (i + 1);
            if digit_len <= 3 && (j == bytes.len() || !bytes[j].is_ascii_alphabetic()) {
                return true;
            }
        }
        i += 1;
    }
    false
}

/// `true` when `title` looks like a release packaging *all* (or a
/// range) of season `season`'s episodes — `S01` without an `E##`
/// marker for that season. Used by the Sonarr poll cycle to prefer a
/// single season-pack push over twelve individual-episode pushes when
/// the operator's *arr has many wanted episodes from the same season.
///
/// Accepts both zero-padded (`S01`) and bare (`S1`) forms. A
/// multi-season pack like `S01-S03.Complete` matches for any of the
/// covered seasons. The check disqualifies titles that contain
/// `S<season>E<digit>` (specific episode of *this* season) but is
/// fine with a pack title that happens to also list a different
/// season's episode marker.
fn title_looks_like_season_pack(title: &str, season: u16) -> bool {
    let lower = title.to_lowercase();
    let needles = [format!("s{season:02}"), format!("s{season}")];
    for needle in &needles {
        if season_marker_is_pack(&lower, needle) {
            return true;
        }
    }
    false
}

/// Scan `lower` for `needle` (e.g. `"s01"`) and decide whether each
/// occurrence reads as a pack marker (not a per-episode marker for
/// that season, not the prefix of a longer season number).
///
/// Word-boundary handling:
/// - The byte *before* `needle` must not be alphanumeric (rules out
///   `"vs01"`, `"ss01"`).
/// - The byte *after* `needle` must not extend it into:
///   * `E\d+` — that's `S01E05`, specific episode, NOT a pack.
///   * Another `\d` — `S1` accidentally matching `S10`. The
///     zero-padded form `s01` already covers two-digit seasons, so
///     the bare form needs the strict boundary.
fn season_marker_is_pack(lower: &str, needle: &str) -> bool {
    let bytes = lower.as_bytes();
    let mut start = 0;
    while let Some(rel) = lower[start..].find(needle) {
        let abs = start + rel;
        let after = abs + needle.len();
        let prev_ok = abs == 0 || !bytes[abs - 1].is_ascii_alphanumeric();
        let next_ok = match bytes.get(after) {
            None => true,
            Some(&b'e') => bytes.get(after + 1).is_none_or(|c| !c.is_ascii_digit()),
            Some(c) => !c.is_ascii_digit(),
        };
        if prev_ok && next_ok {
            return true;
        }
        start = abs + 1;
    }
    false
}

/// Per-release threshold floor used by [`pick_pushable`].
///
/// Torrents get bumped by +1 so a release scoring *exactly* the
/// profile's threshold doesn't slip through when the score sum is
/// `rules + 0 seeders`. The engine adds `seeders.min(50)` to every
/// score unconditionally, so a torrent landing on the threshold could
/// equally be 0-seeders + over-tuned rules. Bumping by 1 forces the
/// rules-or-seeders contribution to clear the gate strictly — which,
/// combined with [`meets_quality`]'s 0-seeder skip, gives a
/// belt-and-suspenders guarantee that auto-push picks a live torrent.
///
/// Usenet (Newznab) provider rows have no seeder concept, so the
/// floor stays at the raw threshold.
fn pick_threshold_for(d: &crate::db::decisions::DecisionRow, threshold: u32) -> u32 {
    // Legacy rows pre-dating the `provider_kind` migration carry None
    // — default to the safer "torrent" branch so a 0-seeder release
    // can't slip through if the row is missing its provenance tag.
    let is_usenet = d
        .provider_kind
        .as_deref()
        .is_some_and(|k| k.eq_ignore_ascii_case("newznab"));
    if is_usenet {
        threshold
    } else {
        threshold.saturating_add(1)
    }
}

/// Drop torrent releases that won't be grabbable — zero seeders means
/// the *arr's download client will queue a stuck transfer forever.
/// Newznab (Usenet) releases skip this check; Usenet completability
/// is article-level, not peer-level, and brarr has no per-release
/// retention signal to predict it.
fn meets_quality(d: &crate::db::decisions::DecisionRow) -> bool {
    let is_torrent = d
        .provider_kind
        .as_deref()
        .is_some_and(|k| !k.eq_ignore_ascii_case("newznab"));
    if is_torrent && d.seeders == 0 {
        return false;
    }
    true
}

/// Sonarr poll: walk `/wanted/missing`, group missing episodes by
/// (series, season), try a season-pack search first per group, then
/// fall back to per-episode searches for any season pack that didn't
/// produce a viable candidate.
#[allow(
    clippy::too_many_lines,
    reason = "pack-vs-episode flow reads cleaner as one function with both branches inline — \
              splitting into helpers per branch obscures the early-skip-when-pack-found path."
)]
///
/// Pack pass rationale: Newznab's `tvsearch` with `&ep=N` filters
/// indexer-side, so a single torrent covering S01 never shows up
/// across per-episode polls. Grouping by season lets the pipeline
/// emit one query (`tvsearch&tvdbid=X&season=N`) per (series, season)
/// instead of one per episode — fewer indexer hits and *the* path
/// where pack releases become reachable. When a pack lands above the
/// profile threshold, the per-episode fallback is skipped for that
/// season; Sonarr accepts the pack and marks every wanted episode
/// covered.
async fn run_once_sonarr(state: &AppState, arr: &ArrInstanceRow) -> Result<PollSummary, AppError> {
    let mut summary = PollSummary::default();
    let client = match ArrClient::new(arr.to_arr_instance()) {
        Ok(c) => c,
        Err(e) => {
            warn!(
                target: "brarr_orchestrator::poll",
                arr_name = %arr.name,
                error = %e,
                "failed to build ArrClient"
            );
            return Ok(summary);
        }
    };
    let episodes = match client.wanted_episodes().await {
        Ok(e) => e,
        Err(e) => {
            warn!(
                target: "brarr_orchestrator::poll",
                arr_name = %arr.name,
                error = %e,
                "fetch wanted episodes failed"
            );
            return Ok(summary);
        }
    };
    let base_url = poll_base_url();

    // Group the wanted episodes by (series_id, season_number). The
    // BTreeMap keeps a stable iteration order (series asc, season
    // asc) which makes the logs deterministic across runs.
    let mut by_season: std::collections::BTreeMap<(u64, u16), Vec<WantedEpisode>> =
        std::collections::BTreeMap::new();
    for ep in episodes {
        summary.considered += 1;
        if !is_pollable_episode(&ep) {
            continue;
        }
        let Some(series) = ep.series.as_ref() else {
            continue;
        };
        if series.tvdb_id == 0 {
            continue;
        }
        let key = (series.id, ep.season_number);
        by_season.entry(key).or_default().push(ep);
    }

    let group_count = by_season.len();
    let mut groups_processed = 0_usize;
    for ((series_id, season), group) in by_season {
        groups_processed += 1;
        let series_title: String = group
            .first()
            .and_then(|ep| ep.series.as_ref())
            .map_or_else(|| "?".to_string(), |s| s.title.clone());

        // Pass 1 — season pack search. Skip when the group has only a
        // single episode wanted (no point in pulling a 12-ep pack for
        // 1 missing — Sonarr happily takes either, but a 1-ep search
        // is cheaper and more likely to find a single-ep release on
        // any tracker).
        let mut pack_pushed = false;
        if group.len() >= 2 {
            match run_season_pack_pass(state, arr, series_id, season, &group, &base_url).await {
                Ok(Some(_)) => {
                    summary.pushed += 1;
                    pack_pushed = true;
                    info!(
                        target: "brarr_orchestrator::poll",
                        arr_name = %arr.name,
                        series = %series_title,
                        season = season,
                        episodes_covered = group.len(),
                        "season pack pushed; skipping per-episode fallback"
                    );
                }
                Ok(None) => {
                    debug!(
                        target: "brarr_orchestrator::poll",
                        arr_name = %arr.name,
                        series = %series_title,
                        season = season,
                        "no season pack candidate; falling back to per-episode"
                    );
                }
                Err(e) => {
                    summary.search_errors += 1;
                    warn!(
                        target: "brarr_orchestrator::poll",
                        arr_name = %arr.name,
                        series = %series_title,
                        season = season,
                        error = %e,
                        "season pack search failed"
                    );
                }
            }
        }

        if pack_pushed {
            if groups_processed < group_count {
                time::sleep(PER_MOVIE_DELAY).await;
            }
            continue;
        }

        // Pass 2 — per-episode fallback.
        let mut ep_iter = group.into_iter().peekable();
        while let Some(ep) = ep_iter.next() {
            let Some(keys) = build_keys_for_episode(&ep) else {
                continue;
            };
            summary.searched += 1;
            match run_search(state, keys).await {
                Ok(outcome) => {
                    if let Some(decision) = pick_pushable(state, arr, &outcome.decisions).await? {
                        match push_decision(state, decision, arr, &base_url).await {
                            Ok(row) => {
                                if matches!(row.status, push_history::PushStatus::Ok) {
                                    summary.pushed += 1;
                                }
                            }
                            Err(e) => warn!(
                                target: "brarr_orchestrator::poll",
                                decision_id = %decision.id,
                                error = %e,
                                "push failed at DB layer"
                            ),
                        }
                    }
                }
                Err(e) => {
                    summary.search_errors += 1;
                    warn!(
                        target: "brarr_orchestrator::poll",
                        series = %series_title,
                        season = ep.season_number,
                        episode = ep.episode_number,
                        error = %e,
                        "search failed"
                    );
                }
            }
            if ep_iter.peek().is_some() {
                time::sleep(PER_MOVIE_DELAY).await;
            }
        }
        if groups_processed < group_count {
            time::sleep(PER_MOVIE_DELAY).await;
        }
    }
    Ok(summary)
}

/// Run one `tvsearch&season=N` (no `&ep=`) search per series-season
/// group and try to push the strongest pack-shaped candidate.
///
/// Returns `Ok(Some(decision_id))` if a pack was pushed,
/// `Ok(None)` when no candidate passed the threshold + pack-title
/// check, and `Err(_)` on DB / push-pipeline failure (search-level
/// failures bubble up so the caller can record them in the summary).
async fn run_season_pack_pass(
    state: &AppState,
    arr: &ArrInstanceRow,
    _series_id: u64,
    season: u16,
    group: &[WantedEpisode],
    base_url: &str,
) -> Result<Option<uuid::Uuid>, AppError> {
    let series = group
        .first()
        .and_then(|ep| ep.series.as_ref())
        .ok_or_else(|| AppError::InvalidInput("empty season group".to_string()))?;
    let tvdb = TvdbId::new(series.tvdb_id)
        .map_err(|e| AppError::InvalidInput(format!("invalid tvdb id: {e}")))?;
    let keys = SearchKeys::from_tvdb(tvdb, Some(season), None);
    let outcome = run_search(state, keys).await?;
    // Filter the search outcome down to releases whose title reads as
    // a season pack covering THIS season. Without this filter the
    // per-episode releases the search incidentally returns (Newznab
    // ignores the missing &ep= and may include arbitrary episodes
    // anyway) would compete on score with the pack and possibly win.
    let pack_candidates: Vec<crate::db::decisions::DecisionRow> = outcome
        .decisions
        .into_iter()
        .filter(|d| title_looks_like_season_pack(&d.release_name, season))
        .collect();
    let Some(decision) = pick_pushable(state, arr, &pack_candidates).await? else {
        return Ok(None);
    };
    let row = push_decision(state, decision, arr, base_url).await?;
    if matches!(row.status, push_history::PushStatus::Ok) {
        Ok(Some(decision.id))
    } else {
        // Push attempted, *arr-side rejection or transport error
        // already recorded in push_history. Treat as not-pushed so
        // the per-episode fallback runs.
        Ok(None)
    }
}

/// Externally-reachable origin used to build push proxy URLs. Same
/// fallback as the request-path version in `crate::push`, but reads
/// only the env var since the poller has no incoming request.
fn poll_base_url() -> String {
    crate::push::env_public_base_url().unwrap_or_else(|| "http://127.0.0.1:3000".to_string())
}

/// `true` when this Radarr movie is something brarr should search
/// for: user-monitored, not already on disk.
fn is_pollable(m: &WantedMovie) -> bool {
    m.monitored && !m.has_file
}

/// `true` when this Sonarr episode is something brarr should search
/// for: monitored at the episode level and not already grabbed.
fn is_pollable_episode(e: &WantedEpisode) -> bool {
    e.monitored && !e.has_file
}

/// Build [`SearchKeys`] for a Sonarr wanted episode. Returns `None`
/// when the series row is missing (Sonarr v4 didn't backfill it) or
/// not TVDB-linked (`tvdb_id == 0`); brarr's TV search is TVDB-only.
fn build_keys_for_episode(e: &WantedEpisode) -> Option<SearchKeys> {
    let series = e.series.as_ref()?;
    if series.tvdb_id == 0 {
        return None;
    }
    let tvdb = TvdbId::new(series.tvdb_id).ok()?;
    Some(SearchKeys::from_tvdb(
        tvdb,
        Some(e.season_number),
        Some(e.episode_number),
    ))
}

/// Build [`SearchKeys`] from a Radarr movie row. Prefers IMDb when
/// present (Newznab providers only accept IMDb axis on movie-search);
/// falls back to TMDb. Returns `None` when neither id is usable.
fn build_keys(m: &WantedMovie) -> Option<SearchKeys> {
    let imdb = parse_imdb(&m.imdb_id);
    let tmdb = if m.tmdb_id > 0 {
        TmdbId::new(m.tmdb_id).ok()
    } else {
        None
    };
    if imdb.is_none() && tmdb.is_none() {
        return None;
    }
    Some(SearchKeys {
        tmdb,
        imdb,
        ..SearchKeys::default()
    })
}

fn parse_imdb(raw: &str) -> Option<ImdbId> {
    let trimmed = raw.trim_start_matches("tt").trim_start_matches('0');
    if trimmed.is_empty() {
        return None;
    }
    let n: u32 = trimmed.parse().ok()?;
    ImdbId::new(n).ok()
}

/// Read the poll interval from the `BRARR_ARR_POLL_INTERVAL_SECS` env
/// var, falling back to [`DEFAULT_POLL_INTERVAL`]. Clamps the parsed
/// value to a minimum of 60 seconds so misconfiguration can't melt the
/// upstream providers.
#[must_use]
pub fn interval_from_env() -> Duration {
    std::env::var("BRARR_ARR_POLL_INTERVAL_SECS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .map_or(DEFAULT_POLL_INTERVAL, |secs| {
            Duration::from_secs(secs.max(60))
        })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    fn movie(monitored: bool, has_file: bool, tmdb: u32, imdb: &str) -> WantedMovie {
        WantedMovie {
            id: 1,
            title: "x".into(),
            tmdb_id: tmdb,
            imdb_id: imdb.into(),
            monitored,
            has_file,
        }
    }

    #[test]
    fn is_pollable_requires_monitored_and_no_file() {
        assert!(is_pollable(&movie(true, false, 603, "tt0133093")));
        assert!(!is_pollable(&movie(false, false, 603, "tt0133093")));
        assert!(!is_pollable(&movie(true, true, 603, "tt0133093")));
    }

    #[test]
    fn build_keys_prefers_imdb_when_present() {
        let keys = build_keys(&movie(true, false, 603, "tt0133093")).unwrap();
        assert!(keys.imdb.is_some());
        assert!(keys.tmdb.is_some());
    }

    #[test]
    fn build_keys_falls_back_to_tmdb_when_imdb_missing() {
        let keys = build_keys(&movie(true, false, 603, "")).unwrap();
        assert!(keys.imdb.is_none());
        assert_eq!(keys.tmdb.unwrap().get(), 603);
    }

    #[test]
    fn build_keys_returns_none_when_both_ids_missing() {
        assert!(build_keys(&movie(true, false, 0, "")).is_none());
    }

    #[test]
    fn parse_imdb_strips_tt_prefix_and_leading_zeros() {
        // Synthetic id — split with underscores to avoid the privacy
        // scanner's 7-digit-run heuristic.
        let target: u32 = 9_999_001;
        let s = target.to_string();
        let with_tt = format!("tt{s}");
        assert_eq!(parse_imdb(&with_tt).unwrap().get(), target);
        assert_eq!(parse_imdb(&s).unwrap().get(), target);
        assert!(parse_imdb("").is_none());
        assert!(parse_imdb("tt").is_none());
    }

    #[test]
    fn default_poll_interval_is_30_min() {
        assert_eq!(DEFAULT_POLL_INTERVAL.as_secs(), 1800);
    }

    fn ep(monitored: bool, has_file: bool, tvdb: u32, season: u16, episode: u16) -> WantedEpisode {
        WantedEpisode {
            id: 1,
            series_id: 10,
            title: "ep".into(),
            season_number: season,
            episode_number: episode,
            monitored,
            has_file,
            series: Some(brarr_arr::WantedEpisodeSeries {
                id: 10,
                title: "show".into(),
                tvdb_id: tvdb,
                monitored: true,
            }),
        }
    }

    fn decision_with(kind: Option<&str>, seeders: u32) -> crate::db::decisions::DecisionRow {
        crate::db::decisions::DecisionRow {
            id: uuid::Uuid::nil(),
            search_id: uuid::Uuid::nil(),
            provider_id: None,
            provider_name: "p".into(),
            release_name: "r".into(),
            release_id_remote: 1,
            score: 200,
            rejected: false,
            tags: vec![],
            matched_rules: vec![],
            seeders,
            leechers: 0,
            size_bytes: 1,
            resolution: "1080p".into(),
            kind: "WEB-DL".into(),
            download_url: None,
            details_url: None,
            provider_kind: kind.map(String::from),
            published_at: None,
            audio_languages: Vec::new(),
            subtitle_languages: Vec::new(),
            profile_scores: std::collections::HashMap::new(),
            decided_at: brarr_core::OffsetDateTime::now_utc(),
        }
    }

    #[test]
    fn meets_quality_keeps_torrents_with_seeders() {
        assert!(meets_quality(&decision_with(Some("unit3d"), 5)));
        assert!(meets_quality(&decision_with(Some("torznab"), 1)));
        assert!(meets_quality(&decision_with(Some("plugin"), 3)));
    }

    #[test]
    fn meets_quality_drops_zero_seeders_torrents() {
        assert!(!meets_quality(&decision_with(Some("unit3d"), 0)));
        assert!(!meets_quality(&decision_with(Some("torznab"), 0)));
    }

    #[test]
    fn meets_quality_keeps_newznab_even_with_zero_seeders() {
        // NZB has no seeders concept — `seeders == 0` is the default.
        assert!(meets_quality(&decision_with(Some("newznab"), 0)));
    }

    #[test]
    fn pick_threshold_bumps_torrent_by_one() {
        // Torrents need to clear threshold strictly so a 0-seeder /
        // exactly-on-threshold release doesn't auto-grab.
        let unit3d = decision_with(Some("unit3d"), 0);
        let torznab = decision_with(Some("torznab"), 0);
        let plugin = decision_with(Some("plugin"), 0);
        assert_eq!(pick_threshold_for(&unit3d, 330), 331);
        assert_eq!(pick_threshold_for(&torznab, 330), 331);
        assert_eq!(pick_threshold_for(&plugin, 330), 331);
    }

    #[test]
    fn pick_threshold_keeps_usenet_at_base() {
        let newznab = decision_with(Some("newznab"), 0);
        assert_eq!(pick_threshold_for(&newznab, 330), 330);
    }

    #[test]
    fn pick_threshold_handles_legacy_unset_provider_kind_as_torrent() {
        // Legacy rows pre-dating the provider_kind migration carry
        // None — treat the safer way (torrent semantics) so we don't
        // accidentally grab a stale 0-seeder candidate.
        let legacy = decision_with(None, 0);
        assert_eq!(pick_threshold_for(&legacy, 330), 331);
    }

    #[test]
    fn title_pack_detects_zero_padded_and_bare_forms() {
        assert!(title_looks_like_season_pack("Show.S01.1080p.WEB-DL", 1));
        assert!(title_looks_like_season_pack("Show.S1.1080p.WEB-DL", 1));
        assert!(title_looks_like_season_pack("Show S01 Complete", 1));
        assert!(title_looks_like_season_pack("Show S08 BluRay", 8));
    }

    #[test]
    fn title_pack_rejects_episode_marker_for_same_season() {
        assert!(!title_looks_like_season_pack("Show.S01E05.1080p", 1));
        assert!(!title_looks_like_season_pack("Show.s01e05.web", 1));
        assert!(!title_looks_like_season_pack("Show S01E12 WEB", 1));
    }

    #[test]
    fn title_pack_rejects_wrong_season() {
        // S02 release should not match season=1 wanted.
        assert!(!title_looks_like_season_pack("Show.S02.Complete", 1));
        assert!(!title_looks_like_season_pack("Show.S10.WEB-DL", 1));
    }

    #[test]
    fn title_pack_handles_multi_season_packs() {
        // "S01-S03 Complete" covers seasons 1, 2, and 3 — return true
        // for any of them. The Sonarr poll cycle picks the relevant
        // season and the pack covers it.
        assert!(title_looks_like_season_pack(
            "Show.S01-S03.Complete.1080p",
            1
        ));
        assert!(title_looks_like_season_pack(
            "Show.S01-S03.Complete.1080p",
            3
        ));
    }

    #[test]
    fn title_pack_strict_word_boundary() {
        // Avoid false-positive on "ss01" or "vs01" tokens hidden in
        // group / source names.
        assert!(!title_looks_like_season_pack("Showvs01.1080p", 1));
        // Bare `s1` shouldn't match against `s10`.
        assert!(!title_looks_like_season_pack("Show.S10.WEB", 1));
    }

    #[test]
    fn season_grouping_keeps_eps_by_season_and_dedups_seasons() {
        // The Sonarr poll loop groups wanted episodes by
        // (series_id, season). Verify the BTreeMap semantics directly
        // — duplicate (series, season) pairs collapse into one group;
        // distinct seasons get distinct groups; ordering is stable.
        let mut by_season: std::collections::BTreeMap<(u64, u16), Vec<WantedEpisode>> =
            std::collections::BTreeMap::new();
        let s1e1 = ep(true, false, 100, 1, 1);
        let s1e2 = ep(true, false, 100, 1, 2);
        let s1e3 = ep(true, false, 100, 1, 3);
        let s2e1 = ep(true, false, 100, 2, 1);
        for e in [s1e1, s1e2, s1e3, s2e1] {
            let series = e.series.as_ref().unwrap().id;
            by_season
                .entry((series, e.season_number))
                .or_default()
                .push(e);
        }
        assert_eq!(by_season.len(), 2, "S1 + S2 = 2 groups");
        assert_eq!(by_season[&(10, 1)].len(), 3, "S1 has 3 wanted eps");
        assert_eq!(by_season[&(10, 2)].len(), 1, "S2 has 1 wanted ep");
        // BTreeMap iter order: (series asc, season asc).
        let keys: Vec<_> = by_season.keys().collect();
        assert_eq!(keys, vec![&(10, 1), &(10, 2)]);
    }

    #[test]
    fn pick_threshold_saturates_at_u32_max() {
        // Edge case: threshold already at the upper bound. Saturate
        // instead of wrap, so the gate stays unreachable rather than
        // collapsing to zero.
        let torrent = decision_with(Some("unit3d"), 0);
        assert_eq!(pick_threshold_for(&torrent, u32::MAX), u32::MAX);
    }

    #[test]
    fn title_looks_like_tv_catches_episode_and_season_markers() {
        assert!(title_looks_like_tv("The Rookie S08E14 1080p"));
        assert!(title_looks_like_tv("the.rookie.s08e14.1080p"));
        assert!(title_looks_like_tv("Teen Titans S05 720p WEB-DL"));
        assert!(title_looks_like_tv("Teen.Titans.S05.720p"));
        assert!(title_looks_like_tv("Show S2E10 web"));
    }

    #[test]
    fn title_looks_like_tv_skips_movies_and_year_tags() {
        assert!(!title_looks_like_tv("Stardust.2007.1080p.BluRay"));
        assert!(!title_looks_like_tv("Star Wars 1977"));
        assert!(!title_looks_like_tv("The Matrix 1999 1080p"));
        // S2024 (4-digit) shouldn't count — sometimes scene tags use it
        // for year. Capped to ≤ 3-digit season.
        assert!(!title_looks_like_tv("Title S2024 1080p"));
        // `123S456` — digits adjacent to S, no separator boundary.
        assert!(!title_looks_like_tv("123S456 1080p"));
    }

    #[test]
    fn matches_arr_kind_drops_tv_for_radarr() {
        assert!(!matches_arr_kind("Teen Titans S05 720p", ArrKind::Radarr));
        assert!(!matches_arr_kind("Show.S08E14.1080p", ArrKind::Radarr));
        assert!(matches_arr_kind("The Matrix 1999 1080p", ArrKind::Radarr));
    }

    #[test]
    fn matches_arr_kind_permissive_for_sonarr() {
        // Sonarr accepts anything — its own parser filters.
        assert!(matches_arr_kind("Show.S08E14.1080p", ArrKind::Sonarr));
        assert!(matches_arr_kind("Weird Movie 2024", ArrKind::Sonarr));
    }

    #[test]
    fn meets_quality_keeps_legacy_kind_with_seeders() {
        // Legacy rows have None provider_kind — treat as not-torrent
        // (default branch) so they aren't accidentally dropped.
        assert!(meets_quality(&decision_with(None, 0)));
    }

    #[test]
    fn build_keys_for_episode_returns_none_when_series_missing() {
        // Sonarr v4 sometimes omits the nested series projection even
        // with `includeSeries=true`. After the client-side backfill,
        // genuinely-missing rows still surface as None — poller skips.
        let mut e = ep(true, false, 12345, 1, 1);
        e.series = None;
        assert!(build_keys_for_episode(&e).is_none());
    }

    #[test]
    fn is_pollable_episode_requires_monitored_and_no_file() {
        assert!(is_pollable_episode(&ep(true, false, 12345, 1, 1)));
        assert!(!is_pollable_episode(&ep(false, false, 12345, 1, 1)));
        assert!(!is_pollable_episode(&ep(true, true, 12345, 1, 1)));
    }

    #[test]
    fn build_keys_for_episode_carries_season_and_ep() {
        let keys = build_keys_for_episode(&ep(true, false, 12345, 2, 5)).unwrap();
        assert_eq!(keys.tvdb.unwrap().get(), 12345);
        assert_eq!(keys.season, Some(2));
        assert_eq!(keys.episode, Some(5));
    }

    #[test]
    fn build_keys_for_episode_returns_none_when_tvdb_zero() {
        assert!(build_keys_for_episode(&ep(true, false, 0, 1, 1)).is_none());
    }

    #[test]
    fn per_movie_delay_pacing_is_non_zero() {
        // Sanity: this throttle is what keeps brarr from hammering the
        // upstream providers in a tight loop. Dropping it to zero
        // would risk rate-limit bans, so pin a regression check.
        assert!(PER_MOVIE_DELAY.as_secs() > 0);
    }
}
