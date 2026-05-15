//! [`TrackerProvider`] — the abstraction over "something that can return
//! [`Release`]s for a `TmdbId`".
//!
//! The orchestrator does not care whether a provider talks UNIT3D over
//! HTTPS, runs a WASM plugin, or reads from a fixture file — it just
//! needs releases. Two concrete impls live elsewhere in the workspace:
//!
//! - `brarr_tracker_unit3d::Unit3dClient` (direct HTTP)
//! - `brarr_plugin_host::WasmTrackerProvider` (sandboxed plugin)
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

use crate::{Release, TmdbId};

/// Heap-allocated boxed future returned by [`TrackerProvider`] methods.
///
/// `'a` ties the future to the borrow of `&self`, so the provider does
/// not need to be `'static` to be invoked.
pub type ProviderFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// A source of [`Release`] data for a given [`TmdbId`].
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
}
