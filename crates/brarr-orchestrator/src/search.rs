//! Search orchestration: fan out to every persisted provider, evaluate
//! each release through the rules engine, persist the run, and return
//! the structured outcome.
//!
//! Fan-out goes through the [`brarr_core::TrackerProvider`] trait, so
//! direct UNIT3D HTTP clients, Newznab indexers, and WASM-loaded plugins
//! live in the same pipeline. The client for each provider row is built
//! lazily inside the per-provider future based on the row's `kind` and
//! `plugin_path`:
//!
//! | `kind`        | `plugin_path` | Client                                      |
//! |---------------|---------------|---------------------------------------------|
//! | `"unit3d"`    | `None`        | [`brarr_tracker_unit3d::Unit3dClient`]      |
//! | `"newznab"`   | `None`        | [`brarr_tracker_newznab::NewznabClient`]    |
//! | `"torznab"`   | `None`        | [`brarr_tracker_newznab::NewznabClient`]    |
//! | any           | `Some(path)`  | [`brarr_plugin_host::WasmTrackerProvider`]  |
//!
//! A single `wasmtime::Engine` lives in [`AppState`] and is reused for
//! every plugin instantiation — cranelift initialization is the
//! expensive part; per-`Module` compilation is cheap.

use std::path::Path;
use std::sync::Arc;

use brarr_core::{ImdbId, Release, TmdbId, TrackerProvider, TrackerSource};
use brarr_plugin_host::{PluginConfig, WasmEpochTicker, WasmTrackerProvider};
use brarr_tracker_newznab::NewznabClient;
use brarr_tracker_unit3d::Unit3dClient;
use futures::future::join_all;
use tracing::{debug, info, warn};
use uuid::Uuid;
use wasmtime::Engine as WasmEngine;

use crate::db::{
    decisions,
    decisions::DecisionInsert,
    providers::{self, ProviderRow},
    searches::{self, SearchRequestJson, SearchRow},
};
use crate::{AppError, AppState};

/// Set of external media ids a search can carry. At least one field
/// should be `Some(_)` — fully-empty keys produce zero results.
#[derive(Debug, Clone, Default)]
pub struct SearchKeys {
    /// TMDb id (movies / TV). Honored by UNIT3D and most plugins.
    pub tmdb: Option<TmdbId>,
    /// IMDb id (numeric tt-id without prefix). Honored by Newznab
    /// movie-search.
    pub imdb: Option<ImdbId>,
}

impl SearchKeys {
    /// Convenience: build keys with only `tmdb` set.
    #[must_use]
    pub fn from_tmdb(tmdb: TmdbId) -> Self {
        Self {
            tmdb: Some(tmdb),
            imdb: None,
        }
    }

    /// Convenience: build keys with only `imdb` set.
    #[must_use]
    pub fn from_imdb(imdb: ImdbId) -> Self {
        Self {
            tmdb: None,
            imdb: Some(imdb),
        }
    }
}

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

/// Backwards-compatible wrapper around [`run_search`] that only carries
/// a TMDb id. Existing callers (gRPC `Search`, the HTMX form) stay
/// untouched; new callers should prefer [`run_search`] with a full
/// [`SearchKeys`] bundle.
///
/// # Errors
///
/// See [`run_search`].
pub async fn run_tmdb_search(state: &AppState, tmdb: TmdbId) -> Result<SearchRunOutcome, AppError> {
    run_search(state, SearchKeys::from_tmdb(tmdb)).await
}

/// Execute a search across every configured tracker, persist results,
/// and return the outcome.
///
/// The provider dispatch picks the right search axis per-tracker:
/// - UNIT3D + plugin rows prefer `tmdb`, fall back to `imdb`.
/// - Newznab rows prefer `imdb`, fall back to `tmdb` (best-effort —
///   most Newznab servers don't accept TMDb on movie-search).
///
/// # Errors
///
/// Surfaces [`AppError::Database`] if the search row cannot be created
/// or a decision row cannot be persisted. Tracker-level errors (HTTP
/// timeout, decode failure, plugin trap, etc.) are **not** fatal —
/// they collect in [`SearchRunOutcome::failures`].
pub async fn run_search(state: &AppState, keys: SearchKeys) -> Result<SearchRunOutcome, AppError> {
    let pool = state.pool();
    let engine = state.engine();

    let request = SearchRequestJson {
        tmdb_id: keys.tmdb.map(TmdbId::get),
        imdb_id: keys.imdb.map(|i| i.get().to_string()),
    };
    let search = searches::create(pool, request).await?;
    info!(
        target: "brarr_orchestrator::search",
        search_id = %search.id,
        tmdb = ?keys.tmdb.map(TmdbId::get),
        imdb = ?keys.imdb.map(ImdbId::get),
        "search created"
    );

    let providers = providers::list_all(pool).await?;
    if providers.is_empty() {
        warn!(
            target: "brarr_orchestrator::search",
            search_id = %search.id,
            "no providers configured"
        );
        return Ok(SearchRunOutcome {
            search,
            decisions: Vec::new(),
            failures: Vec::new(),
        });
    }

    let per_provider = fan_out(state.wasm_engine(), state.wasm_ticker(), &providers, &keys).await;

    let mut decisions_out = Vec::new();
    let mut failures = Vec::new();

    for (pr, result) in per_provider {
        match result {
            Ok(releases) => {
                debug!(
                    target: "brarr_orchestrator::search",
                    provider = %pr.name,
                    count = releases.len(),
                    "provider returned releases"
                );
                for release in releases {
                    let outcome = engine.evaluate(&release);
                    // Persist every release — including rejected ones —
                    // so the UI can show what was filtered out.
                    let ins = build_insert(&search.id, &pr, &release, &outcome);
                    let row = decisions::insert(pool, ins).await?;
                    if !outcome.rejected {
                        decisions_out.push(row);
                    }
                }
            }
            Err(e) => {
                warn!(
                    target: "brarr_orchestrator::search",
                    provider = %pr.name,
                    error = %e,
                    "provider failed"
                );
                failures.push((pr.name.clone(), e));
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
    providers: &[ProviderRow],
    keys: &SearchKeys,
) -> Vec<(ProviderRow, Result<Vec<Release>, String>)> {
    let wasm_engine = wasm_engine.clone();
    let futures = providers.iter().cloned().map(|pr| {
        let wasm_engine = wasm_engine.clone();
        let keys = keys.clone();
        async move {
            let client = match build_provider(&wasm_engine, ticker, &pr).await {
                Ok(p) => p,
                Err(e) => return (pr, Err(e)),
            };
            let result = dispatch_search(&pr, client.as_ref(), &keys)
                .await
                .map_err(|e| e.to_string());
            (pr, result)
        }
    });
    join_all(futures).await
}

/// Per-provider axis picker. Newznab/Torznab rows prefer IMDb; everything
/// else prefers TMDb. Falls back to the other axis when the preferred
/// one is missing from `keys`. Returns `Ok(vec![])` when no usable axis
/// is available (so the provider shows up with zero hits instead of an
/// error).
async fn dispatch_search(
    pr: &ProviderRow,
    provider: &dyn TrackerProvider,
    keys: &SearchKeys,
) -> Result<Vec<Release>, brarr_core::ProviderError> {
    let prefer_imdb =
        pr.kind.eq_ignore_ascii_case("newznab") || pr.kind.eq_ignore_ascii_case("torznab");
    if prefer_imdb && let Some(imdb) = keys.imdb {
        return provider.search_by_imdb(imdb).await;
    }
    if !prefer_imdb && let Some(tmdb) = keys.tmdb {
        return provider.search_by_tmdb(tmdb).await;
    }
    // Fallback axis when the preferred one wasn't supplied.
    if let Some(tmdb) = keys.tmdb {
        return provider.search_by_tmdb(tmdb).await;
    }
    if let Some(imdb) = keys.imdb {
        return provider.search_by_imdb(imdb).await;
    }
    Ok(Vec::new())
}

/// Build a `TrackerProvider` for `pr`. Dispatch matrix:
///
/// | `kind`        | `plugin_path` | Client                                |
/// |---------------|---------------|---------------------------------------|
/// | `unit3d`      | `None`        | [`Unit3dClient`]                      |
/// | `newznab`     | `None`        | [`NewznabClient`] (Usenet shape)      |
/// | `torznab`     | `None`        | [`NewznabClient`] (torrent shape)     |
/// | any           | `Some(path)`  | [`WasmTrackerProvider`]               |
async fn build_provider(
    wasm_engine: &WasmEngine,
    ticker: &WasmEpochTicker,
    pr: &ProviderRow,
) -> Result<Arc<dyn TrackerProvider>, String> {
    let source = TrackerSource::new(pr.name.clone(), pr.base_url.clone())
        .map_err(|e| format!("invalid provider source: {e}"))?;

    if let Some(path) = pr.plugin_path.as_deref() {
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
        return Ok(Arc::new(provider));
    }

    // Newznab and Torznab share the same XML wire format; the same
    // client handles both. The distinction is captured by `kind` for
    // future divergence (e.g. download URL shape, category defaults).
    if pr.kind.eq_ignore_ascii_case("newznab") || pr.kind.eq_ignore_ascii_case("torznab") {
        let client = NewznabClient::new(source, &pr.api_token).map_err(|e| e.to_string())?;
        Ok(Arc::new(client))
    } else {
        // Default to UNIT3D for unknown kinds — gives a useful error
        // if the token is wrong instead of silently swallowing.
        let client = Unit3dClient::new(source, &pr.api_token).map_err(|e| e.to_string())?;
        Ok(Arc::new(client))
    }
}

fn read_plugin_bytes(path: &Path) -> std::io::Result<Vec<u8>> {
    std::fs::read(path)
}

fn build_insert(
    search_id: &Uuid,
    provider: &ProviderRow,
    release: &Release,
    outcome: &brarr_decision_service::DecisionOutcome,
) -> DecisionInsert {
    // Best-effort parse of the provider-side release id. Falls back to 0
    // if the source kept the id as a non-numeric string.
    let release_id_remote = release.tracker_release_id.parse::<u64>().unwrap_or(0);
    DecisionInsert {
        search_id: *search_id,
        provider_id: Some(provider.id),
        provider_name: provider.name.clone(),
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
        download_url: release.urls.download.as_ref().map(url::Url::to_string),
        details_url: release.urls.details.as_ref().map(url::Url::to_string),
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
    async fn search_with_no_providers_returns_empty_outcome() {
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
