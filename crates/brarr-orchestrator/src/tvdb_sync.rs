//! `TheTVDB` → `library_episode_numbering`.
//!
//! **The catalogue is not touched.** Not a title, not an air date, not a
//! season row — the only thing that lands here is the translation
//! between the numbering brarr stores and the numbering releases use.
//! `brarr_tvdb`'s crate docs set out why at length; the short version is
//! that `library_episodes`' two number columns are simultaneously the
//! row's identity, the file name on disk and the pairing key with
//! Sonarr, so the one thing that must change is the network coordinate
//! and nothing else.
//!
//! ## Why this exists when the \*arr sweep already derives one
//!
//! Because that one only works while the \*arr is installed. brarr's
//! whole direction is replacing it, and the numbering was the one thing
//! that would have stopped working on the day it came out — measured:
//! 15 series, 669 episodes, and the canonical coordinate matches **zero**
//! of the release names those titles actually have.
//!
//! `TheTVDB` is where Sonarr's numbering comes from, so this is the same
//! answer at its source. [`episode_numbering::Source`] ranks it above
//! the \*arr for exactly that reason, and both below anything the
//! operator settled by hand.
//!
//! ## Which season type
//!
//! [`SeasonType::Official`] — the broadcast split, which is what the
//! scene follows. Verified live: Dragon Ball Super comes back as
//! 14/13/19/30/55, the shape on this operator's disk, against TMDB's
//! single season of 131. The absolute number rides along on each
//! episode, and it is what joins the two.

use std::time::Duration;

use brarr_tvdb::{SeasonType, TvdbAuth, TvdbClient, TvdbError};
use tokio::task::JoinHandle;
use tokio::time::sleep;
use tracing::{debug, info, warn};

use crate::db::library::{LibraryItem, MediaType};
use crate::db::{Pool, episode_numbering, library, settings};
use crate::episode_match::{self, ExternalNumber};
use crate::{AppError, AppState};

/// Default gap between sweeps.
///
/// A numbering changes when a contributor edits `TheTVDB`, which is
/// rare — hourly would be pointless traffic against someone else's free
/// tier, and a day is soon enough for a split that has been wrong for
/// years.
pub const DEFAULT_SYNC_INTERVAL: Duration = Duration::from_secs(12 * 60 * 60);

/// Floor, so a typo cannot turn the sweep into a hammer.
pub const MIN_SYNC_INTERVAL: Duration = Duration::from_secs(300);

/// Pause between titles inside one sweep, to stay a polite client.
const PER_TITLE_DELAY: Duration = Duration::from_millis(250);

/// What one sweep did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SyncReport {
    /// Series looked at.
    pub examined: usize,
    /// Series whose numbering was written or refreshed.
    pub numbered: usize,
    /// Series both sources number identically. The common answer.
    pub identical: usize,
    /// Series left alone because the operator, or a better source, had
    /// already settled them.
    pub skipped: usize,
    /// Series `TheTVDB` could not answer for.
    pub failed: usize,
}

/// Build a client from the stored credentials.
///
/// # Errors
///
/// [`AppError::InvalidInput`] when no key is configured.
pub async fn client(pool: &Pool) -> Result<TvdbClient, AppError> {
    let stored = settings::get_all(pool).await?;
    let pick = |key: &str| -> Option<String> {
        stored
            .get(key)
            .map(|v| v.trim().to_owned())
            .filter(|v| !v.is_empty())
    };
    let Some(api_key) = pick(settings::KEY_TVDB_API_KEY) else {
        return Err(AppError::InvalidInput(
            "TheTVDB não configurada — informe a chave de API em /settings".to_owned(),
        ));
    };
    TvdbClient::new(TvdbAuth {
        api_key,
        pin: pick(settings::KEY_TVDB_PIN),
    })
    .map_err(tvdb_error)
}

/// Whether a key is configured at all, so callers can no-op quietly.
///
/// # Errors
///
/// Returns [`AppError::Database`] on SQL failure.
pub async fn is_configured(pool: &Pool) -> Result<bool, AppError> {
    Ok(settings::get_all(pool)
        .await?
        .get(settings::KEY_TVDB_API_KEY)
        .is_some_and(|k| !k.trim().is_empty()))
}

/// Derive and store one title's search numbering.
///
/// Returns whether anything was written. `false` covers both "the two
/// sources agree" and "somebody better already answered", which the
/// caller distinguishes by asking [`episode_numbering::source`] — the
/// sweep counts them separately and a single title's button does not
/// care.
///
/// # Errors
///
/// Propagates `TheTVDB` and database failures.
pub async fn sync_item(
    pool: &Pool,
    tvdb: &TvdbClient,
    item: &LibraryItem,
) -> Result<bool, AppError> {
    if item.media_type != MediaType::Tv {
        return Ok(false);
    }
    let Some(tvdb_id) = item.tvdb_id else {
        return Ok(false);
    };

    let found = tvdb
        .series_episodes(tvdb_id, SeasonType::Official, None)
        .await
        .map_err(tvdb_error)?;
    let external: Vec<ExternalNumber> = found
        .episodes
        .iter()
        .filter(|e| e.season_number > 0)
        .map(|e| ExternalNumber {
            season: e.season_number,
            episode: e.number,
            absolute: e.absolute_number,
        })
        .collect();

    let catalogue = library::episodes(pool, item.id).await?;
    let rows = episode_match::derive_numbering(&catalogue, &external);
    let wrote =
        episode_numbering::apply_derived(pool, item.id, episode_numbering::Source::Tvdb, &rows)
            .await?;

    if wrote && !rows.is_empty() {
        info!(
            target: "brarr_orchestrator::tvdb_sync",
            item = %item.id, title = %item.title, episodes = rows.len(),
            "derived the search numbering from TheTVDB"
        );
    }
    Ok(wrote && !rows.is_empty())
}

/// Sweep every series in the library.
///
/// Best-effort per title: one series `TheTVDB` has never heard of must
/// not take the other three hundred down with it.
///
/// # Errors
///
/// Returns [`AppError`] only for failures that affect the whole sweep —
/// no credentials, or the database.
pub async fn sync_all(state: &AppState) -> Result<SyncReport, AppError> {
    let pool = state.pool();
    let tvdb = client(pool).await?;
    let mut report = SyncReport::default();

    for item in library::list(pool).await? {
        if item.media_type != MediaType::Tv || item.tvdb_id.is_none() {
            continue;
        }
        report.examined += 1;

        // Asked before the network call, not after: a title the operator
        // settled is a request brarr should not make at all.
        let current = episode_numbering::source(pool, item.id).await?;
        if !episode_numbering::Source::Tvdb.may_replace(current) {
            report.skipped += 1;
            continue;
        }

        match sync_item(pool, &tvdb, &item).await {
            Ok(true) => report.numbered += 1,
            Ok(false) => report.identical += 1,
            Err(e) => {
                report.failed += 1;
                warn!(
                    target: "brarr_orchestrator::tvdb_sync",
                    item = %item.id, title = %item.title, error = %e,
                    "could not derive a numbering from TheTVDB"
                );
            }
        }
        sleep(PER_TITLE_DELAY).await;
    }

    info!(
        target: "brarr_orchestrator::tvdb_sync",
        examined = report.examined, numbered = report.numbered,
        identical = report.identical, skipped = report.skipped,
        failed = report.failed,
        "TheTVDB numbering sweep finished"
    );
    Ok(report)
}

/// The background sweep.
///
/// No-ops entirely while no key is configured, like every other
/// background task in brarr — a deployment that never opens the
/// corresponding screen sees no change in behaviour.
#[must_use]
pub fn spawn(state: AppState) -> JoinHandle<()> {
    tokio::spawn(async move {
        // Staggered past the *arr sweep so a cold start does not open
        // both against the same catalogue at once.
        sleep(Duration::from_secs(180)).await;
        loop {
            match is_configured(state.pool()).await {
                Ok(true) => {
                    if let Err(e) = sync_all(&state).await {
                        warn!(
                            target: "brarr_orchestrator::tvdb_sync",
                            error = %e, "TheTVDB numbering sweep failed"
                        );
                    }
                }
                Ok(false) => debug!(
                    target: "brarr_orchestrator::tvdb_sync",
                    "no TheTVDB key configured — skipping"
                ),
                Err(e) => warn!(
                    target: "brarr_orchestrator::tvdb_sync",
                    error = %e, "could not read the TheTVDB settings"
                ),
            }
            sleep(configured_interval(state.pool()).await).await;
        }
    })
}

/// Read the interval fresh each cycle, so `/settings` takes effect
/// without a restart.
async fn configured_interval(pool: &Pool) -> Duration {
    let Ok(stored) = settings::get_all(pool).await else {
        return DEFAULT_SYNC_INTERVAL;
    };
    let Some(raw) = stored.get(settings::KEY_TVDB_SYNC_INTERVAL_SECS) else {
        return DEFAULT_SYNC_INTERVAL;
    };
    match raw.trim().parse::<u64>() {
        // A blank setting reads as "use the default", the contract every
        // other hot-reloadable value in brarr has.
        Ok(0) | Err(_) => DEFAULT_SYNC_INTERVAL,
        Ok(secs) => Duration::from_secs(secs).max(MIN_SYNC_INTERVAL),
    }
}

/// Peel the two failures worth naming out of the transport noise.
///
/// A refused key is configuration and the message has to say so; a
/// series `TheTVDB` does not have is not an error worth a stack of
/// context. Everything else keeps its own words.
fn tvdb_error(e: TvdbError) -> AppError {
    match e {
        TvdbError::Unauthorized | TvdbError::TokenRejected => AppError::InvalidInput(format!(
            "TheTVDB recusou a credencial — confira a chave em /settings ({e})"
        )),
        TvdbError::NotFound(what) => AppError::NotFound(what),
        other => AppError::InvalidInput(other.to_string()),
    }
}
