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

/// What [`sync_item`] concluded about one title.
///
/// Four outcomes rather than a `bool`, because they ask for four
/// different reactions and the operator is the one who has to react. The
/// first version returned `bool` and the screen said "a TheTVDB numera
/// esta série igual ao TMDB, ou outra fonte já foi escolhida aqui" — one
/// sentence covering a title with no TVDB id, a title the operator had
/// deliberately set aside, and a title where nothing was wrong. Naming
/// three conditions with an "ou" is the same defect the scan badge had
/// when it said "nada encontrado" for three different reasons.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NumberingOutcome {
    /// Written. Carries how many episodes now translate.
    Numbered(usize),
    /// `TheTVDB` numbers this title exactly the way TMDB does. Nothing
    /// was wrong and nothing needed doing — the common answer, 164 of
    /// this operator's 180 series.
    Identical,
    /// Somebody ranking at or above `TheTVDB` already answered.
    Settled(episode_numbering::Source),
    /// No TVDB id on the item, so there is nothing to ask about.
    NoSearchId,
    /// Not a series.
    NotSeries,
}

impl NumberingOutcome {
    /// What the panel says. Portuguese, and it names the actual
    /// condition — including the way out, where there is one.
    #[must_use]
    pub fn message(self) -> String {
        match self {
            Self::Numbered(n) => format!("{n} episódios passaram a ser buscados pela TheTVDB"),
            Self::Identical => {
                "a TheTVDB numera esta série igual ao TMDB — não há nada a traduzir".to_owned()
            }
            Self::Settled(source) => format!(
                "a numeração desta série já está definida ({}) — limpe-a antes para usar a TheTVDB",
                source.description()
            ),
            Self::NoSearchId => {
                "esta série não tem id da TheTVDB no catálogo, então não há o que consultar"
                    .to_owned()
            }
            Self::NotSeries => "agrupamentos só existem para séries".to_owned(),
        }
    }

    /// Whether anything changed, for the sweep's counters.
    #[must_use]
    pub fn wrote(self) -> bool {
        matches!(self, Self::Numbered(_))
    }
}

/// Derive and store one title's search numbering.
///
/// # Errors
///
/// Propagates `TheTVDB` and database failures.
pub async fn sync_item(
    pool: &Pool,
    tvdb: &TvdbClient,
    item: &LibraryItem,
) -> Result<NumberingOutcome, AppError> {
    if item.media_type != MediaType::Tv {
        return Ok(NumberingOutcome::NotSeries);
    }
    let Some(tvdb_id) = item.tvdb_id else {
        return Ok(NumberingOutcome::NoSearchId);
    };
    // Asked before the network call: a title somebody already settled is
    // a request brarr should not make at all.
    let current = episode_numbering::source(pool, item.id).await?;
    if !episode_numbering::Source::Tvdb.may_replace(current) {
        return Ok(NumberingOutcome::Settled(
            current.unwrap_or(episode_numbering::Source::Off),
        ));
    }

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

    // The refusal is re-read rather than assumed: `apply_derived` holds
    // the rule, and a race between the check above and the write is a
    // race the write should win.
    if !wrote {
        let now = episode_numbering::source(pool, item.id).await?;
        return Ok(NumberingOutcome::Settled(
            now.unwrap_or(episode_numbering::Source::Off),
        ));
    }
    if rows.is_empty() {
        return Ok(NumberingOutcome::Identical);
    }
    info!(
        target: "brarr_orchestrator::tvdb_sync",
        item = %item.id, title = %item.title, episodes = rows.len(),
        "derived the search numbering from TheTVDB"
    );
    Ok(NumberingOutcome::Numbered(rows.len()))
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

        match sync_item(pool, &tvdb, &item).await {
            Ok(NumberingOutcome::Numbered(_)) => report.numbered += 1,
            Ok(NumberingOutcome::Identical) => report.identical += 1,
            Ok(
                NumberingOutcome::Settled(_)
                | NumberingOutcome::NoSearchId
                | NumberingOutcome::NotSeries,
            ) => {
                report.skipped += 1;
                // Nothing was asked of TheTVDB, so nothing to be polite
                // about — the delay below is for requests, not for rows.
                continue;
            }
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

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "test assertions")]
mod tests {
    use super::*;
    use episode_numbering::Source;

    /// **Four conditions, four sentences.** The first version returned a
    /// `bool` and the screen said "a TheTVDB numera esta série igual ao
    /// TMDB, ou outra fonte já foi escolhida aqui" — which was true for a
    /// title with no TVDB id, for one the operator had set aside, and for
    /// one where nothing was wrong. This walks the enum so a new variant
    /// cannot quietly reuse somebody else's wording.
    #[test]
    fn every_outcome_says_something_different() {
        let all = [
            NumberingOutcome::Numbered(13),
            NumberingOutcome::Identical,
            NumberingOutcome::Settled(Source::Manual),
            NumberingOutcome::NoSearchId,
            NumberingOutcome::NotSeries,
        ];
        let messages: std::collections::HashSet<String> = all.iter().map(|o| o.message()).collect();
        assert_eq!(messages.len(), all.len(), "two outcomes share a sentence");
        assert!(all.iter().all(|o| !o.message().is_empty()));

        // The one that carries a way out has to name it.
        let settled = NumberingOutcome::Settled(Source::Manual).message();
        assert!(settled.contains("blocos definidos por você"), "{settled}");
        assert!(settled.contains("limpe"), "{settled}");

        // And a settled title names *which* source, or the operator has
        // to guess which of three they set.
        assert_ne!(settled, NumberingOutcome::Settled(Source::Tmdb).message());

        // Only one of them is success.
        assert!(NumberingOutcome::Numbered(1).wrote());
        assert!(
            all.iter().filter(|o| o.wrote()).count() == 1,
            "success must be exactly one outcome — rendering it as an error \
             is how a working feature reads as broken"
        );
    }
}
