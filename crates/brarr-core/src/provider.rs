#![allow(
    clippy::doc_markdown,
    reason = "TMDb/IMDb/TVDB/AniList/TVMaze acronyms appear too often in user-facing docs to be worth backticking each instance"
)]

//! [`TrackerProvider`] — the abstraction over "something that can return
//! [`Release`]s for a known external media id".
//!
//! The orchestrator does not care whether a provider talks UNIT3D over
//! HTTPS, hits a Newznab Usenet indexer, runs a WASM plugin, or reads
//! from a fixture file — it just needs releases. Three concrete impls
//! live elsewhere in the workspace:
//!
//! - `brarr_tracker_unit3d::Unit3dClient` (UNIT3D torrent JSON)
//! - `brarr_tracker_newznab::NewznabClient` (Newznab/Torznab XML)
//! - `brarr_plugin_host::WasmTrackerProvider` (sandboxed plugin)
//!
//! ## Search-axis fan-out (current state)
//!
//! Different sources accept different keys:
//! - UNIT3D: TMDb id
//! - Newznab movie-search: IMDb id (some servers accept TMDb)
//! - Newznab tv-search: TVDB id + season/episode
//! - Both fall back to free-text `q` queries
//!
//! The trait exposes a method per axis. **Each method has a default
//! impl returning `Ok(vec![])`** — providers that don't support a
//! given axis can omit it and the orchestrator's fan-out treats them
//! as silently returning zero results for that key. This is the
//! pragmatic v1 (added when Newznab integration arrived); see the
//! "Future work" note below.
//!
//! ## Future work: `SearchKey` enum refactor
//!
//! The per-axis methods get unwieldy as new sources land (TVMaze id,
//! AniList id, MAL id with season). A cleaner v2 collapses everything
//! to a single method taking a `SearchKey` enum:
//!
//! ```ignore
//! pub enum SearchKey {
//!     Tmdb(TmdbId),
//!     Imdb(ImdbId),
//!     Tvdb { id: TvdbId, season: Option<u32>, episode: Option<u32> },
//!     Mal(MalId),
//!     Query(String),
//! }
//!
//! pub trait TrackerProvider: Send + Sync {
//!     fn name(&self) -> &str;
//!     fn search(&self, key: &SearchKey) -> ProviderFuture<'_, Result<Vec<Release>, ProviderError>>;
//! }
//! ```
//!
//! That refactor will touch every concrete provider plus the WASM
//! plugin ABI (bumping to v2). Defer until at least one of the
//! following is true:
//! - A third axis lands (e.g. season+episode TV search).
//! - Plugin authors need richer queries than current fixed methods.
//! - The orchestrator wants to express "tried key X, none matched,
//!   fall through to query Y" without N round-trips through the trait.
//!
//! ## Async-fn-in-trait vs `Pin<Box<dyn Future>>`
//!
//! Native `async fn` in trait is stable since Rust 1.75, but the
//! resulting trait is not `dyn`-compatible without explicit boxing.
//! Orchestrator code wants `Vec<Box<dyn TrackerProvider>>` to mix
//! direct and plugin-loaded providers in one list, so this trait spells
//! out the boxed future explicitly. Concrete impls can still use
//! `async fn` internals via `Box::pin(async move { ... })`.

use std::future::Future;
use std::pin::Pin;

use crate::{ImdbId, Release, TmdbId, TvdbId};

/// Heap-allocated boxed future returned by [`TrackerProvider`] methods.
///
/// `'a` ties the future to the borrow of `&self`, so the provider does
/// not need to be `'static` to be invoked.
pub type ProviderFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// A source of [`Release`] data for known external media ids.
pub trait TrackerProvider: Send + Sync {
    /// Human-readable display name (e.g. `"capybara"`). Used in logs
    /// and the admin UI.
    fn name(&self) -> &str;

    /// Search for releases matching the given `TMDb` id.
    ///
    /// Implementations return a heap-allocated future so the trait
    /// remains `dyn`-compatible. See module docs for the rationale.
    fn search_by_tmdb(
        &self,
        tmdb: TmdbId,
    ) -> ProviderFuture<'_, Result<Vec<Release>, ProviderError>>;

    /// Search for releases matching the given `IMDb` id. Defaults to
    /// returning an empty vec so providers that only speak TMDb don't
    /// have to opt in.
    fn search_by_imdb(
        &self,
        _imdb: ImdbId,
    ) -> ProviderFuture<'_, Result<Vec<Release>, ProviderError>> {
        Box::pin(async { Ok(Vec::new()) })
    }

    /// Search for TV releases by `TVDB` id, optional `season`, and
    /// optional `episode`. The three-tier shape mirrors what Newznab
    /// `t=tvsearch` accepts (and what Sonarr sends): all three are
    /// supported with the convention that omitting season/episode
    /// widens the match.
    ///
    /// - `(id, None, None)` → all episodes of the series
    /// - `(id, Some(N), None)` → entire season N (season packs + episodes)
    /// - `(id, Some(N), Some(M))` → single episode
    ///
    /// Defaults to an empty result so providers that don't speak TV
    /// (UNIT3D movie-only deployments, IMDb-only Newznabs, plugin ABI
    /// v1) opt out silently.
    fn search_by_tvdb(
        &self,
        _tvdb: TvdbId,
        _season: Option<u16>,
        _episode: Option<u16>,
    ) -> ProviderFuture<'_, Result<Vec<Release>, ProviderError>> {
        Box::pin(async { Ok(Vec::new()) })
    }
}

/// Erased error type returned by [`TrackerProvider::search_by_tmdb`].
///
/// We deliberately collapse to a `String` payload: the orchestrator
/// renders failures to the user as free-form text, and we want a
/// uniform shape regardless of whether the underlying error came from
/// `reqwest`, `serde_json`, or a WASM trap. Concrete crates keep their
/// own typed error enums for their internal use.
#[derive(Debug, thiserror::Error)]
#[error("{message}")]
pub struct ProviderError {
    /// Source provider name (e.g. tracker name).
    pub source_name: String,
    /// Free-form description of the failure.
    pub message: String,
}

impl ProviderError {
    /// Build a new error.
    #[must_use]
    pub fn new(source_name: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            source_name: source_name.into(),
            message: message.into(),
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::unnecessary_literal_bound)]

    use super::*;

    /// Compile-time check that `TrackerProvider` is `dyn`-compatible.
    #[test]
    fn trait_is_dyn_compatible() {
        fn _accepts_dyn(_p: &dyn TrackerProvider) {}
    }

    #[test]
    fn provider_error_display_uses_message_only() {
        let err = ProviderError::new("capybara", "timeout");
        assert_eq!(format!("{err}"), "timeout");
        assert_eq!(err.source_name, "capybara");
    }

    /// Verifies the default `search_by_imdb` impl returns an empty vec
    /// (so TMDb-only providers don't need to implement it).
    #[tokio::test]
    async fn default_search_by_imdb_returns_empty() {
        struct Stub;
        impl TrackerProvider for Stub {
            fn name(&self) -> &str {
                "stub"
            }
            fn search_by_tmdb(
                &self,
                _tmdb: TmdbId,
            ) -> ProviderFuture<'_, Result<Vec<Release>, ProviderError>> {
                Box::pin(async { Ok(Vec::new()) })
            }
        }
        let s = Stub;
        let releases = s
            .search_by_imdb(ImdbId::new(9_999_001).unwrap())
            .await
            .unwrap();
        assert!(releases.is_empty());
    }
}
