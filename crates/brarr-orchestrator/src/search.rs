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
use std::time::{Duration, Instant};

use brarr_core::{ImdbId, Release, TmdbId, TrackerProvider, TrackerSource, TvdbId};
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
use crate::provider_cache::ProviderClientCache;
use crate::{AppError, AppState};

/// Wall-clock budget for one provider's search within the fan-out. Caps
/// the slowest/timing-out tracker so a single bad upstream can't hold the
/// whole search — and the waiting *arr client — for the full
/// 30s-per-attempt × retry window of the underlying HTTP client. Healthy
/// providers answer well inside this; a provider that exceeds it is
/// reported as a timeout failure and the rest of the fan-out is
/// unaffected (its results and DB persistence proceed normally).
pub const PER_PROVIDER_BUDGET: Duration = Duration::from_secs(15);

/// Time-to-live for the Torznab/Newznab pull-path search cache (see
/// [`crate::ttl_cache`] and [`run_search_cached`]). Long enough to absorb
/// the burst of duplicate requests Sonarr/Radarr issue per Interactive
/// Search (per season/episode, once per configured feed), short enough
/// that provider/profile edits surface on the next window.
pub const SEARCH_CACHE_TTL: Duration = Duration::from_secs(60);

/// Set of external media ids a search can carry. At least one of
/// `tmdb` / `imdb` / `tvdb` should be `Some(_)` — fully-empty keys
/// produce zero results.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)]
pub struct SearchKeys {
    /// TMDb id (movies / TV). Honored by UNIT3D and most plugins.
    pub tmdb: Option<TmdbId>,
    /// IMDb id (numeric tt-id without prefix). Honored by Newznab
    /// movie-search.
    pub imdb: Option<ImdbId>,
    /// TVDB series id. Honored by Newznab `tvsearch` axis. Required
    /// for the TV path — without it `season`/`episode` are ignored.
    pub tvdb: Option<TvdbId>,
    /// Optional TV season filter. Only meaningful when `tvdb` is set.
    /// `None` = match every season (series-wide search). Mirrors the
    /// Newznab `tvsearch&season=N` query param.
    pub season: Option<u16>,
    /// Optional TV episode filter. Only meaningful when `tvdb` AND
    /// `season` are set. `None` with a `season` = match every episode
    /// in that season (useful for season-pack discovery).
    pub episode: Option<u16>,
}

/// Which provider families a search fans out to.
///
/// The Torznab feed (`/torznab/api`) serves torrent results and the
/// Newznab feed (`/newznab/api`) serves Usenet results, so each only
/// needs to dispatch to the providers whose results it can actually
/// render. Before this scope existed both feeds fanned out to *every*
/// provider and filtered the decisions afterwards — the torrent feed
/// would wait on a slow Usenet indexer (and vice versa) for results it
/// then threw away.
///
/// Part of the search-cache key: the same [`SearchKeys`] under a
/// different scope hits a different provider set and must not share a
/// cached outcome.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub enum ProviderScope {
    /// Every enabled provider (admin UI, poller, gRPC, webhooks).
    #[default]
    All,
    /// Torrent-side providers: `unit3d`, `torznab`, and plugins —
    /// everything that is not `newznab`. Mirrors the Torznab feed's
    /// decision filter.
    Torrent,
    /// Usenet-side providers: `kind == "newznab"` only.
    Usenet,
}

impl ProviderScope {
    /// `true` when a provider row of `kind` belongs to this scope.
    #[must_use]
    pub fn includes(self, kind: &str) -> bool {
        match self {
            Self::All => true,
            Self::Usenet => kind.eq_ignore_ascii_case("newznab"),
            Self::Torrent => !kind.eq_ignore_ascii_case("newznab"),
        }
    }
}

impl SearchKeys {
    /// Convenience: build keys with only `tmdb` set.
    #[must_use]
    pub fn from_tmdb(tmdb: TmdbId) -> Self {
        Self {
            tmdb: Some(tmdb),
            ..Self::default()
        }
    }

    /// Convenience: build keys with only `imdb` set.
    #[must_use]
    pub fn from_imdb(imdb: ImdbId) -> Self {
        Self {
            imdb: Some(imdb),
            ..Self::default()
        }
    }

    /// Convenience: build keys for a TV search.
    #[must_use]
    pub fn from_tvdb(tvdb: TvdbId, season: Option<u16>, episode: Option<u16>) -> Self {
        Self {
            tvdb: Some(tvdb),
            season,
            episode,
            ..Self::default()
        }
    }

    /// `true` when at least one search axis is populated.
    #[must_use]
    pub fn has_any(&self) -> bool {
        self.tmdb.is_some() || self.imdb.is_some() || self.tvdb.is_some()
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

/// Cache-aware wrapper around [`run_search_scoped`] for the
/// Torznab/Newznab pull path. Returns a cached [`SearchRunOutcome`] (and
/// `true` for "cache hit") when an identical search under the same
/// [`ProviderScope`] ran within [`SEARCH_CACHE_TTL`]; otherwise runs the
/// real search, stores it, and returns it with `false`.
///
/// Sonarr/Radarr explode one Interactive Search into many near-identical
/// indexer requests (per season/episode, and once per configured brarr
/// feed). This collapses that burst onto a single fan-out per scope
/// instead of re-hammering every upstream tracker and re-persisting a
/// `searches` + `decisions` set per request. The admin UI and the
/// background poller call [`run_search`] directly so they always observe
/// fresh data.
///
/// # Errors
///
/// See [`run_search`]. A cache hit never errors.
pub async fn run_search_cached(
    state: &AppState,
    keys: SearchKeys,
    scope: ProviderScope,
) -> Result<(SearchRunOutcome, bool), AppError> {
    let now = Instant::now();
    let cache_key = (keys.clone(), scope);
    if let Some(hit) = state.search_cache().get(&cache_key, now) {
        debug!(
            target: "brarr_orchestrator::search",
            tmdb = ?keys.tmdb.map(TmdbId::get),
            imdb = ?keys.imdb.map(ImdbId::get),
            tvdb = ?keys.tvdb.map(TvdbId::get),
            season = ?keys.season,
            episode = ?keys.episode,
            scope = ?scope,
            "search cache hit — skipping fan-out"
        );
        return Ok((hit, true));
    }
    let outcome = run_search_scoped(state, keys, scope).await?;
    state.search_cache().insert(cache_key, outcome.clone(), now);
    Ok((outcome, false))
}

/// Execute a search across every configured tracker, persist results,
/// and return the outcome. Equivalent to [`run_search_scoped`] with
/// [`ProviderScope::All`].
///
/// # Errors
///
/// See [`run_search_scoped`].
pub async fn run_search(state: &AppState, keys: SearchKeys) -> Result<SearchRunOutcome, AppError> {
    run_search_scoped(state, keys, ProviderScope::All).await
}

/// Execute a search across every configured tracker within `scope`,
/// persist results, and return the outcome.
///
/// The provider dispatch picks the right search axis per-tracker:
/// - UNIT3D + plugin rows prefer `tmdb`, fall back to `imdb`.
/// - Newznab rows prefer `imdb`, fall back to `tmdb` (best-effort —
///   most Newznab servers don't accept TMDb on movie-search).
///
/// Each provider dispatch is timed and persisted to `provider_metrics`
/// (ok / error / timeout + duration) so the `/health` page can show
/// which upstream is the bottleneck. Metric insertion is best-effort:
/// a failure is logged, never fatal to the search.
///
/// # Errors
///
/// Surfaces [`AppError::Database`] if the search row cannot be created
/// or a decision row cannot be persisted. Tracker-level errors (HTTP
/// timeout, decode failure, plugin trap, etc.) are **not** fatal —
/// they collect in [`SearchRunOutcome::failures`].
#[allow(
    clippy::too_many_lines,
    reason = "search pipeline is a single linear story (load profiles → fan out → score → persist); splitting into helpers obscures the read path"
)]
pub async fn run_search_scoped(
    state: &AppState,
    keys: SearchKeys,
    scope: ProviderScope,
) -> Result<SearchRunOutcome, AppError> {
    run_search_inner(state, keys, scope, false).await
}

/// A search that hides nothing.
///
/// [`run_search_scoped`] drops releases that every quality profile
/// rejects — junk to everyone, and keeping it would grow `decisions`
/// for nothing. That is right for the automatic path and **wrong for
/// the interactive one**, whose entire purpose is to show what the
/// rules would not take and let the operator decide anyway.
///
/// Found with real data: a search that returned 102 releases from four
/// providers rendered an empty table, because both of the operator's
/// profiles rejected all of them. Nothing was broken in the UI — the
/// releases never reached it.
///
/// # Errors
///
/// Same as [`run_search_scoped`].
pub async fn run_search_unfiltered(
    state: &AppState,
    keys: SearchKeys,
) -> Result<SearchRunOutcome, AppError> {
    run_search_inner(state, keys, ProviderScope::All, true).await
}

#[allow(
    clippy::too_many_lines,
    reason = "search pipeline is a single linear story (load profiles → fan out → score → persist); splitting into helpers obscures the read path"
)]
async fn run_search_inner(
    state: &AppState,
    keys: SearchKeys,
    scope: ProviderScope,
    keep_rejected: bool,
) -> Result<SearchRunOutcome, AppError> {
    let pool = state.pool();
    let engine = state.engine();

    let request = SearchRequestJson {
        tmdb_id: keys.tmdb.map(TmdbId::get),
        imdb_id: keys.imdb.map(|i| i.get().to_string()),
        tvdb_id: keys.tvdb.map(TvdbId::get),
        season: keys.season,
        episode: keys.episode,
    };
    let search = searches::create(pool, request).await?;
    info!(
        target: "brarr_orchestrator::search",
        search_id = %search.id,
        tmdb = ?keys.tmdb.map(TmdbId::get),
        imdb = ?keys.imdb.map(ImdbId::get),
        tvdb = ?keys.tvdb.map(TvdbId::get),
        season = ?keys.season,
        episode = ?keys.episode,
        "search created"
    );

    let mut providers = providers::list_enabled(pool).await?;
    providers.retain(|p| scope.includes(&p.kind));
    if providers.is_empty() {
        warn!(
            target: "brarr_orchestrator::search",
            search_id = %search.id,
            scope = ?scope,
            "no enabled providers in scope"
        );
        return Ok(SearchRunOutcome {
            search,
            decisions: Vec::new(),
            failures: Vec::new(),
        });
    }

    // Pre-load every quality profile so each release can be scored
    // against both the baseline engine and each operator-defined
    // profile rule list. The release card displays
    // `max(baseline, profile_scores)` so a release that scores low
    // under baseline but high under, say, an anime profile reads
    // correctly in the UI without the operator having to re-run.
    let profile_rows = crate::db::quality_profiles::list_all(pool).await?;
    let profile_engines: Vec<(uuid::Uuid, brarr_decision_service::Engine)> = profile_rows
        .iter()
        .map(|p| {
            (
                p.id,
                brarr_decision_service::Engine::from_profile_rules(p.rules.clone()),
            )
        })
        .collect();

    let per_provider = fan_out(
        state.wasm_engine(),
        state.wasm_ticker(),
        state.provider_clients(),
        &providers,
        &keys,
    )
    .await;

    let mut decisions_out = Vec::new();
    let mut failures = Vec::new();
    // Keep-all override: when on, universally-rejected releases are still
    // persisted (so the history shows everything that was filtered out);
    // when off (default) they're dropped before insert to keep the
    // `decisions` table lean. Read once per run so `/settings` edits take
    // effect on the next search without a respawn.
    //
    // `keep_rejected` forces it on for the interactive search, which has
    // to show the operator everything that was found.
    let persist_rejected = keep_rejected || state.persist_rejected();

    for fetch in per_provider {
        let ProviderFetch {
            provider: pr,
            result,
            duration,
        } = fetch;
        record_provider_metric(pool, &search.id, &pr, &result, duration).await;
        match result {
            Ok(releases) => {
                debug!(
                    target: "brarr_orchestrator::search",
                    provider = %pr.name,
                    count = releases.len(),
                    "provider returned releases"
                );
                for release in releases {
                    let mut outcome = engine.evaluate(&release);
                    // Score the same release against every persisted
                    // profile's engine so the UI can surface custom
                    // rule outputs (e.g. anime-jp +80 / pt-sub +150)
                    // alongside the baseline. The baseline engine carries
                    // no `reject` rules, so a profile's reject verdict is
                    // the only one that can filter — collect each here.
                    let mut profile_scores = std::collections::HashMap::new();
                    let mut profile_rejections = Vec::with_capacity(profile_engines.len());
                    for (profile_id, profile_engine) in &profile_engines {
                        let p_outcome = profile_engine.evaluate(&release);
                        profile_scores.insert(*profile_id, p_outcome.score.get());
                        profile_rejections.push(p_outcome.rejected);
                    }
                    // Drop releases that every profile rejects (junk to
                    // everyone) unless the operator opted into keeping all.
                    // The `rejected` column is stamped with that universal
                    // verdict so keep-all mode still badges filtered rows.
                    let verdict = persistence_verdict(&profile_rejections, persist_rejected);
                    if !verdict.persist {
                        continue;
                    }
                    outcome.rejected = verdict.rejected;
                    let ins = build_insert(&search.id, &pr, &release, &outcome, profile_scores);
                    let row = decisions::insert(pool, ins).await?;
                    // The automatic path only wants what it could act on;
                    // the interactive one wants the whole picture, badge
                    // and all.
                    if keep_rejected || !verdict.rejected {
                        decisions_out.push(row);
                    }
                }
            }
            Err(failure) => {
                warn!(
                    target: "brarr_orchestrator::search",
                    provider = %pr.name,
                    error = %failure.message,
                    timed_out = failure.timed_out,
                    "provider failed"
                );
                failures.push((pr.name.clone(), failure.message));
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

/// One provider's dispatch result within a fan-out, with wall-clock
/// timing. The duration covers client build + the (budgeted) upstream
/// call, i.e. how long this provider actually held the search open.
struct ProviderFetch {
    provider: ProviderRow,
    result: Result<Vec<Release>, DispatchFailure>,
    duration: Duration,
}

/// A failed dispatch: the message surfaced to the UI plus whether the
/// failure was a [`PER_PROVIDER_BUDGET`] overrun (recorded as a
/// `timeout` in `provider_metrics`, vs a plain `error`).
struct DispatchFailure {
    message: String,
    timed_out: bool,
}

impl DispatchFailure {
    fn error(message: String) -> Self {
        Self {
            message,
            timed_out: false,
        }
    }
}

async fn fan_out(
    wasm_engine: &WasmEngine,
    ticker: &WasmEpochTicker,
    clients: &ProviderClientCache,
    providers: &[ProviderRow],
    keys: &SearchKeys,
) -> Vec<ProviderFetch> {
    let wasm_engine = wasm_engine.clone();
    let futures = providers.iter().cloned().map(|pr| {
        let wasm_engine = wasm_engine.clone();
        let keys = keys.clone();
        async move {
            let started = Instant::now();
            let result = match build_provider(&wasm_engine, ticker, clients, &pr).await {
                Ok(client) => {
                    with_budget(
                        PER_PROVIDER_BUDGET,
                        dispatch_search(&pr, client.as_ref(), &keys),
                    )
                    .await
                }
                Err(e) => Err(DispatchFailure::error(e)),
            };
            ProviderFetch {
                provider: pr,
                result,
                duration: started.elapsed(),
            }
        }
    });
    join_all(futures).await
}

/// Run `fut` (one provider's axis dispatch) under a wall-clock `budget`,
/// folding both a provider error and a budget overrun into the
/// [`DispatchFailure`] the fan-out collects. On overrun the future is
/// dropped, so a stalled upstream stops consuming the fan-out's
/// wall-clock instead of holding the whole search open for its own
/// (much longer) HTTP timeout and retry budget.
async fn with_budget<F>(budget: Duration, fut: F) -> Result<Vec<Release>, DispatchFailure>
where
    F: std::future::Future<Output = Result<Vec<Release>, brarr_core::ProviderError>>,
{
    match tokio::time::timeout(budget, fut).await {
        Ok(result) => result.map_err(|e| DispatchFailure::error(e.to_string())),
        Err(_) => Err(DispatchFailure {
            message: format!("timed out after {}s", budget.as_secs()),
            timed_out: true,
        }),
    }
}

/// Persist one provider dispatch measurement. Best-effort: a metrics
/// insert failure is logged and swallowed — observability must never
/// fail a search.
async fn record_provider_metric(
    pool: &crate::db::Pool,
    search_id: &Uuid,
    pr: &ProviderRow,
    result: &Result<Vec<Release>, DispatchFailure>,
    duration: Duration,
) {
    use crate::db::metrics::{ProviderMetricInsert, ProviderOutcome, insert_provider_metric};
    let (outcome, error, release_count) = match result {
        Ok(releases) => (
            ProviderOutcome::Ok,
            None,
            u32::try_from(releases.len()).unwrap_or(u32::MAX),
        ),
        Err(f) if f.timed_out => (ProviderOutcome::Timeout, Some(f.message.clone()), 0),
        Err(f) => (ProviderOutcome::Error, Some(f.message.clone()), 0),
    };
    let metric = ProviderMetricInsert {
        search_id: Some(*search_id),
        provider_id: Some(pr.id),
        provider_name: pr.name.clone(),
        provider_kind: pr.kind.clone(),
        outcome,
        error,
        duration_ms: u64::try_from(duration.as_millis()).unwrap_or(u64::MAX),
        release_count,
    };
    if let Err(e) = insert_provider_metric(pool, metric).await {
        warn!(
            target: "brarr_orchestrator::search",
            provider = %pr.name,
            error = %e,
            "failed to persist provider metric"
        );
    }
}

/// Per-provider axis picker.
///
/// Priority order:
///   1. **TV** — if `keys.tvdb` is set, always take that path
///      regardless of provider kind. Newznab returns real results;
///      UNIT3D and plugins fall through to their default empty impl
///      (defer until TV search lands on UNIT3D).
///   2. **Movie** — Newznab/Torznab prefer IMDb (canonical Newznab
///      movie-search axis); UNIT3D + plugins prefer TMDb. Falls back
///      to the other movie axis when the preferred one isn't supplied.
///
/// Returns `Ok(vec![])` when no axis is usable (so the provider
/// shows up with zero hits instead of an error).
async fn dispatch_search(
    pr: &ProviderRow,
    provider: &dyn TrackerProvider,
    keys: &SearchKeys,
) -> Result<Vec<Release>, brarr_core::ProviderError> {
    if let Some(tvdb) = keys.tvdb {
        return provider
            .search_by_tvdb(tvdb, keys.season, keys.episode)
            .await;
    }
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
    clients: &ProviderClientCache,
    pr: &ProviderRow,
) -> Result<Arc<dyn TrackerProvider>, String> {
    let source = TrackerSource::new(pr.name.clone(), pr.base_url.clone())
        .map_err(|e| format!("invalid provider source: {e}"))?;

    if let Some(path) = pr.plugin_path.as_deref() {
        // Plugins aren't cached: they own a WASM module loaded from disk
        // and have their own lifecycle (see `state` docs). Re-load per
        // call as before.
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

    // Reuse the built HTTP client when this provider's config is
    // unchanged — keeps the `reqwest` connection pool + TLS session warm
    // across the many requests Sonarr/Radarr issue per search.
    let fingerprint = crate::provider_cache::fingerprint(pr);
    if let Some(cached) = clients.get(pr.id, fingerprint) {
        return Ok(cached);
    }

    // Newznab and Torznab share the same XML wire format; the same
    // client handles both. The distinction is captured by `kind` for
    // future divergence (e.g. download URL shape, category defaults).
    let built: Arc<dyn TrackerProvider> =
        if pr.kind.eq_ignore_ascii_case("newznab") || pr.kind.eq_ignore_ascii_case("torznab") {
            let client = NewznabClient::new(source, &pr.api_token).map_err(|e| e.to_string())?;
            Arc::new(client)
        } else {
            // Default to UNIT3D for unknown kinds — gives a useful error
            // if the token is wrong instead of silently swallowing.
            let client = Unit3dClient::new(source, &pr.api_token).map_err(|e| e.to_string())?;
            Arc::new(client)
        };
    clients.put(pr.id, fingerprint, Arc::clone(&built));
    Ok(built)
}

fn read_plugin_bytes(path: &Path) -> std::io::Result<Vec<u8>> {
    std::fs::read(path)
}

/// Persistence verdict for one release, derived from how every quality
/// profile judged it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PersistVerdict {
    /// Whether the decision row should be written to `decisions` at all.
    persist: bool,
    /// Value to stamp in the `rejected` column when it is written.
    rejected: bool,
}

/// Decide whether to persist a release and what its `rejected` flag is.
///
/// A release is *universally rejected* — junk to every profile that
/// opined — only when at least one profile exists and **all** of them
/// reject it. The baseline engine carries no `reject` rules, so it never
/// participates here; with zero profiles nothing is ever dropped.
///
/// - Universally rejected + keep-all off ⇒ drop (don't persist).
/// - Universally rejected + keep-all on ⇒ persist, `rejected = true`.
/// - Apt to at least one profile (or no profiles) ⇒ persist,
///   `rejected = false`.
fn persistence_verdict(profile_rejections: &[bool], persist_rejected: bool) -> PersistVerdict {
    let universally_rejected =
        !profile_rejections.is_empty() && profile_rejections.iter().all(|&r| r);
    PersistVerdict {
        persist: persist_rejected || !universally_rejected,
        rejected: universally_rejected,
    }
}

fn build_insert(
    search_id: &Uuid,
    provider: &ProviderRow,
    release: &Release,
    outcome: &brarr_decision_service::DecisionOutcome,
    profile_scores: std::collections::HashMap<Uuid, u32>,
) -> DecisionInsert {
    // Best-effort parse of the provider-side release id. Falls back to 0
    // if the source kept the id as a non-numeric string — which Newznab
    // guids always are. `release_guid` carries the untouched form
    // alongside it; anything that needs a *stable* release key (the grab
    // barrier) must read that one, not this.
    let release_id_remote = release.tracker_release_id.parse::<u64>().unwrap_or(0);
    DecisionInsert {
        search_id: *search_id,
        provider_id: Some(provider.id),
        provider_name: provider.name.clone(),
        release_name: release.title.clone(),
        release_id_remote,
        release_guid: Some(release.tracker_release_id.clone()),
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
        provider_kind: Some(provider.kind.clone()),
        published_at: release.published_at,
        audio_languages: release
            .enrichment
            .as_ref()
            .map(|e| e.audio_languages.clone())
            .unwrap_or_default(),
        subtitle_languages: release
            .enrichment
            .as_ref()
            .map(|e| e.subtitle_languages.clone())
            .unwrap_or_default(),
        profile_scores,
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use std::time::Duration;

    use super::{
        ProviderScope, SearchKeys, persistence_verdict, run_search_cached, run_search_scoped,
        run_tmdb_search, with_budget,
    };
    use crate::AppState;
    use crate::db::{open_memory, providers};
    use brarr_core::{ProviderError, Release, TmdbId};
    use brarr_decision_service::Engine;

    #[test]
    fn verdict_no_profiles_keeps_release() {
        // No profiles opined ⇒ nothing to reject against ⇒ always kept.
        let v = persistence_verdict(&[], false);
        assert!(v.persist);
        assert!(!v.rejected);
    }

    #[test]
    fn verdict_apt_to_some_profile_is_kept() {
        // Rejected by one profile but apt to another ⇒ not universal junk.
        let v = persistence_verdict(&[true, false], false);
        assert!(v.persist);
        assert!(!v.rejected);
    }

    #[test]
    fn verdict_universally_rejected_is_dropped_by_default() {
        // Every profile rejects ⇒ junk to everyone ⇒ dropped when
        // keep-all is off.
        let v = persistence_verdict(&[true, true], false);
        assert!(!v.persist);
        assert!(v.rejected);
    }

    #[test]
    fn verdict_universally_rejected_is_kept_when_keep_all_on() {
        // Same junk, but keep-all on ⇒ persisted and badged rejected.
        let v = persistence_verdict(&[true, true], true);
        assert!(v.persist);
        assert!(v.rejected);
    }

    #[test]
    fn verdict_single_profile_keep_is_persisted() {
        let v = persistence_verdict(&[false], false);
        assert!(v.persist);
        assert!(!v.rejected);
    }

    #[tokio::test]
    async fn with_budget_returns_ok_for_fast_future() {
        let res = with_budget(Duration::from_secs(5), async {
            Ok::<Vec<Release>, ProviderError>(Vec::new())
        })
        .await;
        assert!(res.is_ok());
    }

    #[tokio::test]
    async fn with_budget_times_out_slow_future() {
        let res = with_budget(Duration::from_millis(10), async {
            tokio::time::sleep(Duration::from_millis(500)).await;
            Ok::<Vec<Release>, ProviderError>(Vec::new())
        })
        .await;
        let err = res.expect_err("slow future must hit the budget");
        assert!(err.timed_out);
        assert!(err.message.contains("timed out"), "got: {}", err.message);
    }

    #[tokio::test]
    async fn with_budget_propagates_provider_error() {
        let res = with_budget(Duration::from_secs(5), async {
            Err::<Vec<Release>, ProviderError>(ProviderError::new("x", "boom"))
        })
        .await;
        let err = res.expect_err("provider error must surface");
        assert!(!err.timed_out);
        assert!(err.message.contains("boom"), "got: {}", err.message);
    }

    #[test]
    fn scope_includes_maps_kinds_to_protocol_side() {
        // Torrent side: everything that is not newznab.
        assert!(ProviderScope::Torrent.includes("unit3d"));
        assert!(ProviderScope::Torrent.includes("torznab"));
        assert!(ProviderScope::Torrent.includes("plugin"));
        assert!(!ProviderScope::Torrent.includes("newznab"));
        assert!(!ProviderScope::Torrent.includes("NEWZNAB"));
        // Usenet side: newznab only.
        assert!(ProviderScope::Usenet.includes("newznab"));
        assert!(ProviderScope::Usenet.includes("Newznab"));
        assert!(!ProviderScope::Usenet.includes("unit3d"));
        assert!(!ProviderScope::Usenet.includes("torznab"));
        // All includes both sides.
        assert!(ProviderScope::All.includes("unit3d"));
        assert!(ProviderScope::All.includes("newznab"));
    }

    /// Seed one unreachable provider of each protocol side. Port 9 on
    /// localhost is unassigned, so dispatches fail fast with a
    /// connection error instead of consuming the per-provider budget.
    async fn seed_unreachable_providers(pool: &crate::db::Pool) {
        let base = url::Url::parse("http://127.0.0.1:9/").unwrap();
        for (name, kind) in [("torrent-side", "unit3d"), ("usenet-side", "newznab")] {
            providers::insert(
                pool,
                providers::NewProvider {
                    name,
                    base_url: &base,
                    api_token: "t",
                    kind,
                    plugin_path: None,
                },
            )
            .await
            .unwrap();
        }
    }

    #[tokio::test]
    async fn scoped_search_only_dispatches_matching_providers() {
        let pool = open_memory().await.unwrap();
        let state = AppState::new(pool.clone(), Engine::baseline());
        seed_unreachable_providers(&pool).await;

        let keys = SearchKeys::from_tmdb(TmdbId::new(603).unwrap());
        let outcome = run_search_scoped(&state, keys.clone(), ProviderScope::Torrent)
            .await
            .unwrap();
        // Only the unit3d provider was dispatched (and failed fast).
        assert_eq!(outcome.failures.len(), 1);
        assert_eq!(outcome.failures[0].0, "torrent-side");

        let outcome = run_search_scoped(&state, keys.clone(), ProviderScope::Usenet)
            .await
            .unwrap();
        assert_eq!(outcome.failures.len(), 1);
        assert_eq!(outcome.failures[0].0, "usenet-side");

        let outcome = run_search_scoped(&state, keys, ProviderScope::All)
            .await
            .unwrap();
        assert_eq!(outcome.failures.len(), 2);
    }

    #[tokio::test]
    async fn scoped_search_records_provider_metrics() {
        let pool = open_memory().await.unwrap();
        let state = AppState::new(pool.clone(), Engine::baseline());
        seed_unreachable_providers(&pool).await;

        let keys = SearchKeys::from_tmdb(TmdbId::new(603).unwrap());
        run_search_scoped(&state, keys, ProviderScope::Torrent)
            .await
            .unwrap();

        let recorded = crate::db::metrics::provider_stats(&pool, 0).await.unwrap();
        assert_eq!(recorded.len(), 1, "only the in-scope provider is measured");
        assert_eq!(recorded[0].provider_name, "torrent-side");
        assert_eq!(recorded[0].total, 1);
        assert_eq!(recorded[0].errors, 1);
        assert!(recorded[0].last_error.is_some());
    }

    #[tokio::test]
    async fn cached_search_is_keyed_by_scope() {
        let pool = open_memory().await.unwrap();
        let state = AppState::new(pool, Engine::baseline());
        let keys = SearchKeys::from_tmdb(TmdbId::new(603).unwrap());

        let (_, hit) = run_search_cached(&state, keys.clone(), ProviderScope::Torrent)
            .await
            .unwrap();
        assert!(!hit, "first torrent-scoped search is a miss");
        let (_, hit) = run_search_cached(&state, keys.clone(), ProviderScope::Torrent)
            .await
            .unwrap();
        assert!(hit, "identical torrent-scoped search hits the cache");
        let (_, hit) = run_search_cached(&state, keys, ProviderScope::Usenet)
            .await
            .unwrap();
        assert!(
            !hit,
            "same keys under another scope must not share the entry"
        );
    }

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
