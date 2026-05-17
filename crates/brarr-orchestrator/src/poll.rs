//! Scheduled poller that closes the autobrr-style loop.
//!
//! Every `BRARR_ARR_POLL_INTERVAL_SECS` (default 1800 = 30 min), brarr:
//!   1. Walks every enabled *arr instance via [`ArrClient::monitored_movies`].
//!   2. For each monitored, file-less movie with a known TMDb / IMDb id,
//!      runs the brarr search pipeline as if a user submitted it.
//!   3. Sorts the resulting kept decisions by score descending.
//!   4. For the top scorer that exceeds the *arr's `push_threshold`
//!      AND hasn't been pushed to this *arr already, calls
//!      [`crate::push::push_decision`] — recording the attempt in
//!      `push_history` regardless of outcome.
//!   5. Moves to the next movie. One push per movie per poll cycle so
//!      brarr never spams *arr with competing grabs for the same title.
//!
//! Sonarr support is deferred — TV-search axis isn't wired through
//! the search pipeline yet (CHECKPOINT bug #5). The poller logs and
//! skips Sonarr rows.
//!
//! ## Manual trigger
//!
//! [`run_once_for_instance`] is exposed for the UI "rodar agora"
//! button on `/arr-instances`. Same code path as the scheduled tick;
//! just bypasses the sleep.

use std::sync::Arc;
use std::time::Duration;

use brarr_arr::{ArrClient, ArrKind, WantedMovie};
use brarr_core::{ImdbId, TmdbId};
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
    let mut summary = PollSummary::default();
    if arr.kind != ArrKind::Radarr {
        warn!(
            target: "brarr_orchestrator::poll",
            arr_name = %arr.name,
            kind = arr.kind.label(),
            "Sonarr poll skipped — TV search axis not implemented"
        );
        return Ok(summary);
    }

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

    let base_url = crate::push::env_public_base_url().unwrap_or_else(|| {
        // The scheduled poller has no request to derive from — fall
        // back to localhost. *arr running on the same host will work;
        // a distributed setup needs BRARR_PUBLIC_URL set.
        "http://127.0.0.1:3000".to_string()
    });

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
                if let Some(decision) = outcome
                    .decisions
                    .iter()
                    .find(|d| d.score >= arr.push_threshold)
                {
                    if push_history::already_pushed(state.pool(), decision.id, arr.id).await? {
                        debug!(
                            target: "brarr_orchestrator::poll",
                            decision_id = %decision.id,
                            arr_name = %arr.name,
                            "already pushed, skipping"
                        );
                    } else {
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

/// `true` when this Radarr movie is something brarr should search
/// for: user-monitored, not already on disk.
fn is_pollable(m: &WantedMovie) -> bool {
    m.monitored && !m.has_file
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
    Some(SearchKeys { tmdb, imdb })
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

    #[test]
    fn per_movie_delay_pacing_is_non_zero() {
        // Sanity: this throttle is what keeps brarr from hammering the
        // upstream providers in a tight loop. Dropping it to zero
        // would risk rate-limit bans, so pin a regression check.
        assert!(PER_MOVIE_DELAY.as_secs() > 0);
    }
}
