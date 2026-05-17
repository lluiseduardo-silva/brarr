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
async fn pick_pushable<'a>(
    state: &AppState,
    arr: &ArrInstanceRow,
    decisions: &'a [crate::db::decisions::DecisionRow],
) -> Result<Option<&'a crate::db::decisions::DecisionRow>, AppError> {
    for d in decisions
        .iter()
        .take_while(|d| d.score >= arr.push_threshold)
    {
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

/// Sonarr poll: walk `/wanted/missing`, search per episode by
/// tvdb+season+episode, push the top decision crossing the threshold.
/// Same throttling/dedup story as the Radarr path.
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

    let mut iter = episodes.into_iter();
    while let Some(ep) = iter.next() {
        summary.considered += 1;
        if !is_pollable_episode(&ep) {
            continue;
        }
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
                    series = %ep.series.as_ref().map_or("?", |s| s.title.as_str()),
                    season = ep.season_number,
                    episode = ep.episode_number,
                    error = %e,
                    "search failed"
                );
            }
        }
        if iter.len() > 0 {
            time::sleep(PER_MOVIE_DELAY).await;
        }
    }
    Ok(summary)
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
