//! The name a title's folder takes, from the same place the \*arr takes
//! it.
//!
//! brarr builds a folder name from `library_items.title`, which is the
//! **TMDB** title in the operator's language. Sonarr and Radarr build
//! theirs from **TheTVDB's English** name. For a library both programs
//! write into, that is a second folder per title the moment the two
//! disagree.
//!
//! Measured on this operator's catalogue, 176 series paired by TheTVDB
//! id: 112 titles agreed, 64 did not, 3 had already split on disk and 61
//! were one import away from it. The disagreements are not spellings —
//! `Os Simpsons` against `The Simpsons`, `O Tio de Outro Mundo` against
//! `Uncle from Another World` — so no amount of normalising bridges
//! them. Asking the same database they ask is what does.
//!
//! ## Why the English translation and not the record's own name
//!
//! `GET /series/{id}` answers with the title in the **original**
//! language: `未来日記`, `ゴブリンスレイヤー`. Taking that would have been
//! worse than what brarr had. Sonarr uses `GET
//! /series/{id}/translations/eng`, and against Sonarr's own stored
//! titles that matched 10 out of 10. Reproducing Sonarr's naming rule
//! over it accounts for 172 of the 176 folders on this disk.
//!
//! The other four hold a name that exists nowhere any more — one folder
//! made by hand, three that TheTVDB renamed after Sonarr created the
//! folder, and with `renameEpisodes = False` Sonarr never rewrites one.
//! **A folder is a snapshot of the title on the day it was made**, so no
//! rule can reach those; `library_items.arr_folder` is what does, and it
//! outranks this.
//!
//! ## This never touches what the operator reads
//!
//! Only `folder_title` is written. The catalogue keeps showing the
//! Portuguese title everywhere a person looks — the shelf, the detail
//! page, the search — because that is the one they chose. The English
//! name exists solely to name a directory.

use std::sync::Arc;
use std::time::Duration;

use brarr_tvdb::{TvdbAuth, TvdbClient};
use tokio::task::JoinHandle;
use tokio::time::sleep;
use tracing::{debug, info, warn};

use crate::db::{library, settings};
use crate::{AppError, AppState};

/// How often the sweep runs. A title is renamed on TheTVDB rarely, and
/// the cost of being a cycle late is nil — the folder brarr would
/// create is only consulted when it actually imports something.
const SWEEP_INTERVAL: Duration = Duration::from_secs(12 * 60 * 60);

/// Delay before the first sweep, so it does not pile onto startup.
const STARTUP_DELAY: Duration = Duration::from_secs(180);

/// The language the \*arr name their folders in.
///
/// Not configurable, and that is the point: this value does not exist to
/// describe anything to anybody, it exists to agree with Sonarr and
/// Radarr, which use English. An operator who wants Portuguese folders
/// wants brarr to *disagree* with the programs sharing the disk.
const FOLDER_LANGUAGE: &str = "eng";

/// What one sweep did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FolderNameSummary {
    /// Series looked at.
    pub considered: usize,
    /// Series whose folder name was written or changed.
    pub written: usize,
    /// Series TheTVDB has no English translation for.
    pub untranslated: usize,
    /// Lookups that failed.
    pub failed: usize,
}

/// Spawn the background sweep. Mirrors the other spawners: dropping the
/// handle aborts the task, and it no-ops while TheTVDB is unconfigured.
#[must_use]
pub fn spawn(state: AppState) -> JoinHandle<()> {
    let state = Arc::new(state);
    info!(
        target: "brarr_orchestrator::folder_names",
        interval_secs = SWEEP_INTERVAL.as_secs(),
        "starting the folder-name sweep"
    );
    tokio::spawn(async move {
        sleep(STARTUP_DELAY).await;
        loop {
            match run_once(&state).await {
                Ok(s) if s.written > 0 => info!(
                    target: "brarr_orchestrator::folder_names",
                    considered = s.considered,
                    written = s.written,
                    untranslated = s.untranslated,
                    failed = s.failed,
                    "folder names refreshed"
                ),
                Ok(s) => debug!(
                    target: "brarr_orchestrator::folder_names",
                    considered = s.considered,
                    "no folder name changed"
                ),
                Err(e) => {
                    warn!(target: "brarr_orchestrator::folder_names", error = %e, "sweep failed");
                }
            }
            sleep(SWEEP_INTERVAL).await;
        }
    })
}

/// One pass over every series carrying a TheTVDB id.
///
/// # Errors
///
/// Returns [`AppError::Database`] when the catalogue cannot be read.
/// A per-title lookup failure is counted, never propagated: one series
/// TheTVDB will not answer for must not stop the other 175.
pub async fn run_once(state: &AppState) -> Result<FolderNameSummary, AppError> {
    let mut summary = FolderNameSummary::default();
    let Some(client) = build_client(state).await else {
        debug!(
            target: "brarr_orchestrator::folder_names",
            "TheTVDB is not configured; nothing to ask"
        );
        return Ok(summary);
    };
    for (id, tvdb, current) in library::series_with_tvdb_id(state.pool()).await? {
        summary.considered += 1;
        match client.series_translation(tvdb, FOLDER_LANGUAGE).await {
            Ok(Some(t)) => {
                let name = t.name.unwrap_or_default();
                let name = name.trim();
                if name.is_empty() {
                    summary.untranslated += 1;
                } else if current.as_deref() != Some(name) {
                    library::set_folder_title(state.pool(), id, Some(name)).await?;
                    summary.written += 1;
                }
            }
            // No English translation is an answer, not a fault: the
            // catalogue title stays, which is what brarr did before.
            Ok(None) => summary.untranslated += 1,
            Err(e) => {
                summary.failed += 1;
                debug!(
                    target: "brarr_orchestrator::folder_names",
                    tvdb, error = %e,
                    "could not read the English name"
                );
            }
        }
    }
    Ok(summary)
}

/// A TheTVDB client pinned to English.
///
/// Built here rather than taken from the metadata registry because the
/// registry's client carries the operator's language chain — which is
/// the very thing this module must not use.
async fn build_client(state: &AppState) -> Option<TvdbClient> {
    let key = settings::get(state.pool(), "tvdb_api_key")
        .await
        .ok()??
        .value;
    let key = key.trim();
    if key.is_empty() {
        return None;
    }
    let pin = settings::get(state.pool(), "tvdb_pin")
        .await
        .ok()
        .flatten()
        .map(|p| p.value.trim().to_owned())
        .filter(|p| !p.is_empty());
    TvdbClient::new(TvdbAuth {
        api_key: key.to_owned(),
        pin,
    })
    .map(|c| c.with_languages([FOLDER_LANGUAGE]))
    .ok()
}
