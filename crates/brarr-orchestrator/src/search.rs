//! Search orchestration: fan out to every persisted tracker, evaluate
//! each release through the rules engine, persist the run, and return
//! the structured outcome.
//!
//! Fan-out goes through the [`brarr_core::TrackerProvider`] trait, so
//! direct UNIT3D HTTP clients and WASM-loaded plugins live in the same
//! pipeline. The provider for each tracker row is built lazily inside
//! the per-tracker future:
//!
//! - `plugin_path == None`     → [`brarr_tracker_unit3d::Unit3dClient`]
//! - `plugin_path == Some(p)`  → [`brarr_plugin_host::WasmTrackerProvider`]
//!
//! A single `wasmtime::Engine` lives in [`AppState`] and is reused for
//! every plugin instantiation — cranelift initialization is the
//! expensive part; per-`Module` compilation is cheap.

use std::path::Path;
use std::sync::Arc;

use brarr_core::{Release, TmdbId, TrackerProvider, TrackerSource};
use brarr_plugin_host::{PluginConfig, WasmEpochTicker, WasmTrackerProvider};
use brarr_tracker_unit3d::Unit3dClient;
use futures::future::join_all;
use tracing::{debug, info, warn};
use uuid::Uuid;
use wasmtime::Engine as WasmEngine;

use crate::db::{
    decisions,
    decisions::DecisionInsert,
    searches::{self, SearchRequestJson, SearchRow},
    trackers::{self, TrackerRow},
};
use crate::{AppError, AppState};

/// Aggregate outcome of one search run.
#[derive(Debug, Clone)]
pub struct SearchRunOutcome {
    /// Persisted search row (already in the DB).
    pub search: SearchRow,
    /// All decision rows produced (non-rejected, ordered by score DESC,
    /// already persisted).
    pub decisions: Vec<crate::db::decisions::DecisionRow>,
    /// `(tracker_name, error_string)` pairs for trackers that errored.
    /// Not persisted today — surfaced for live UI feedback only.
    pub failures: Vec<(String, String)>,
}

/// Execute a TMDb search across every configured tracker, persist
/// results, and return the outcome.
///
/// This is the one entry point shared between the HTTP route and the
/// gRPC service so both paths land in identical DB state.
///
/// # Errors
///
/// Surfaces [`AppError::Database`] if the search row cannot be created
/// or a decision row cannot be persisted. Tracker-level errors (HTTP
/// timeout, decode failure, plugin trap, etc.) are **not** fatal —
/// they collect in [`SearchRunOutcome::failures`].
pub async fn run_tmdb_search(state: &AppState, tmdb: TmdbId) -> Result<SearchRunOutcome, AppError> {
    let pool = state.pool();
    let engine = state.engine();

    let request = SearchRequestJson {
        tmdb_id: Some(tmdb.get()),
        imdb_id: None,
    };
    let search = searches::create(pool, request).await?;
    info!(
        target: "brarr_orchestrator::search",
        search_id = %search.id,
        tmdb = tmdb.get(),
        "search created"
    );

    let trackers = trackers::list_all(pool).await?;
    if trackers.is_empty() {
        warn!(
            target: "brarr_orchestrator::search",
            search_id = %search.id,
            "no trackers configured"
        );
        return Ok(SearchRunOutcome {
            search,
            decisions: Vec::new(),
            failures: Vec::new(),
        });
    }

    let per_tracker = fan_out(state.wasm_engine(), state.wasm_ticker(), &trackers, tmdb).await;

    let mut decisions_out = Vec::new();
    let mut failures = Vec::new();

    for (tr, result) in per_tracker {
        match result {
            Ok(releases) => {
                debug!(
                    target: "brarr_orchestrator::search",
                    tracker = %tr.name,
                    count = releases.len(),
                    "tracker returned releases"
                );
                for release in releases {
                    let outcome = engine.evaluate(&release);
                    // Persist every release — including rejected ones —
                    // so the UI can show what was filtered out.
                    let ins = build_insert(&search.id, &tr, &release, &outcome);
                    let row = decisions::insert(pool, ins).await?;
                    if !outcome.rejected {
                        decisions_out.push(row);
                    }
                }
            }
            Err(e) => {
                warn!(
                    target: "brarr_orchestrator::search",
                    tracker = %tr.name,
                    error = %e,
                    "tracker failed"
                );
                failures.push((tr.name.clone(), e));
            }
        }
    }

    // Sort by score DESC, seeders DESC tiebreaker — mirrors brarr-cli.
    decisions_out.sort_by(|a, b| {
        b.score
            .cmp(&a.score)
            .then_with(|| b.seeders.cmp(&a.seeders))
    });

    let count = u32::try_from(decisions_out.len()).unwrap_or(u32::MAX);
    searches::set_result_count(pool, search.id, count).await?;
    let mut search = search;
    search.result_count = count;

    info!(
        target: "brarr_orchestrator::search",
        search_id = %search.id,
        kept = decisions_out.len(),
        failures = failures.len(),
        "search complete"
    );

    Ok(SearchRunOutcome {
        search,
        decisions: decisions_out,
        failures,
    })
}

async fn fan_out(
    wasm_engine: &WasmEngine,
    ticker: &WasmEpochTicker,
    trackers: &[TrackerRow],
    tmdb: TmdbId,
) -> Vec<(TrackerRow, Result<Vec<Release>, String>)> {
    let wasm_engine = wasm_engine.clone();
    // The ticker is borrowed through `state.wasm_ticker()` for the
    // lifetime of the search; we capture it by raw pointer through
    // the engine and a closure scope. Simpler: just pass &WasmEngine +
    // call `build_provider` per-tracker which takes refs.
    let futures = trackers.iter().cloned().map(|tr| {
        let wasm_engine = wasm_engine.clone();
        async move {
            let provider = match build_provider(&wasm_engine, ticker, &tr).await {
                Ok(p) => p,
                Err(e) => return (tr, Err(e)),
            };
            let result = provider
                .search_by_tmdb(tmdb)
                .await
                .map_err(|e| e.to_string());
            (tr, result)
        }
    });
    join_all(futures).await
}

/// Build a `TrackerProvider` for `tr`. UNIT3D rows return a
/// [`Unit3dClient`] wrapper; rows with `plugin_path` return a
/// [`WasmTrackerProvider`] compiled against `wasm_engine` (with the
/// shared `ticker` enforcing the per-call epoch deadline).
async fn build_provider(
    wasm_engine: &WasmEngine,
    ticker: &WasmEpochTicker,
    tr: &TrackerRow,
) -> Result<Arc<dyn TrackerProvider>, String> {
    let source = TrackerSource::new(tr.name.clone(), tr.base_url.clone())
        .map_err(|e| format!("invalid tracker source: {e}"))?;

    if let Some(path) = tr.plugin_path.as_deref() {
        let bytes =
            read_plugin_bytes(path).map_err(|e| format!("read plugin {}: {e}", path.display()))?;
        let provider = WasmTrackerProvider::load_with_engine(
            wasm_engine,
            ticker,
            &bytes,
            PluginConfig::new(source),
        )
        .await
        .map_err(|e| format!("load plugin {}: {e}", path.display()))?;
        Ok(Arc::new(provider))
    } else {
        let client = Unit3dClient::new(source, &tr.api_token).map_err(|e| e.to_string())?;
        Ok(Arc::new(client))
    }
}

fn read_plugin_bytes(path: &Path) -> std::io::Result<Vec<u8>> {
    std::fs::read(path)
}

fn build_insert(
    search_id: &Uuid,
    tracker: &TrackerRow,
    release: &Release,
    outcome: &brarr_decision_service::DecisionOutcome,
) -> DecisionInsert {
    // Best-effort parse of the tracker-side release id. Falls back to 0
    // if the source kept the id as a non-numeric string.
    let release_id_remote = release.tracker_release_id.parse::<u64>().unwrap_or(0);
    DecisionInsert {
        search_id: *search_id,
        tracker_id: Some(tracker.id),
        tracker_name: tracker.name.clone(),
        release_name: release.title.clone(),
        release_id_remote,
        score: outcome.score.get(),
        rejected: outcome.rejected,
        tags: outcome.tags.clone(),
        matched_rules: outcome.matched_rules.clone(),
        seeders: release.seeders,
        leechers: release.leechers,
        size_bytes: release.size_bytes,
        resolution: release.resolution.clone(),
        kind: release.kind.clone(),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::run_tmdb_search;
    use crate::AppState;
    use crate::db::open_memory;
    use brarr_core::TmdbId;
    use brarr_decision_service::Engine;

    #[tokio::test]
    async fn search_with_no_trackers_returns_empty_outcome() {
        let pool = open_memory().await.unwrap();
        let state = AppState::new(pool, Engine::baseline());
        let outcome = run_tmdb_search(&state, TmdbId::new(603).unwrap())
            .await
            .unwrap();
        assert!(outcome.decisions.is_empty());
        assert!(outcome.failures.is_empty());
        // search row was still persisted with result_count = 0
        assert_eq!(outcome.search.tmdb_id, Some(603));
        assert_eq!(outcome.search.result_count, 0);
    }
}
