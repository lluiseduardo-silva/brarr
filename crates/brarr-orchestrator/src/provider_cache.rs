//! Cache of built HTTP [`TrackerProvider`]s, keyed by provider id.
//!
//! [`crate::search::build_provider`] used to construct a brand-new
//! `reqwest::Client` for every provider on every search. Under Sonarr's
//! many-requests-per-search pattern that means a fresh TLS handshake and
//! a cold connection pool on each call. Caching the built provider keeps
//! the underlying `reqwest::Client` — and its keep-alive connection pool
//! and TLS session cache — alive across requests, so repeat searches to
//! the same tracker reuse warm connections.
//!
//! Only the built-in HTTP clients (UNIT3D / Newznab / Torznab) are
//! cached. Plugin providers load a WASM module from disk and have their
//! own lifecycle (see [`crate::state`] docs); they're rebuilt per call as
//! before.
//!
//! Cache entries carry a [`fingerprint`] of the provider's mutable
//! config (base URL + token + kind). When the operator edits a provider
//! the fingerprint changes and the stale client is transparently dropped
//! on the next lookup — no explicit invalidation hook needed.

use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::sync::{Arc, Mutex, PoisonError};

use brarr_core::TrackerProvider;
use uuid::Uuid;

use crate::db::providers::ProviderRow;

/// Thread-safe map of `provider id -> (config fingerprint, client)`.
#[derive(Default)]
pub struct ProviderClientCache {
    inner: Mutex<HashMap<Uuid, Entry>>,
}

struct Entry {
    fingerprint: u64,
    provider: Arc<dyn TrackerProvider>,
}

impl ProviderClientCache {
    /// Empty cache.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Return the cached client for `id` when present **and** its stored
    /// fingerprint still matches `fingerprint` (i.e. the provider config
    /// hasn't changed since it was built). A mismatch is treated as a
    /// miss so the caller rebuilds with the new config.
    #[must_use]
    pub fn get(&self, id: Uuid, fingerprint: u64) -> Option<Arc<dyn TrackerProvider>> {
        let map = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        let entry = map.get(&id)?;
        (entry.fingerprint == fingerprint).then(|| Arc::clone(&entry.provider))
    }

    /// Store `provider` under `id` with its config `fingerprint`,
    /// replacing any previous entry.
    pub fn put(&self, id: Uuid, fingerprint: u64, provider: Arc<dyn TrackerProvider>) {
        let mut map = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        map.insert(
            id,
            Entry {
                fingerprint,
                provider,
            },
        );
    }
}

impl std::fmt::Debug for ProviderClientCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let len = self
            .inner
            .lock()
            .map_or_else(|e| e.into_inner().len(), |m| m.len());
        f.debug_struct("ProviderClientCache")
            .field("entries", &len)
            .finish()
    }
}

/// Fingerprint a provider's mutable config. Two rows with the same
/// fingerprint produce an interchangeable client; any edit to the base
/// URL, token, or kind changes it and invalidates the cached client.
///
/// `plugin_path` is deliberately excluded — plugin providers are never
/// cached, so they never reach this function on the hit path.
#[must_use]
pub fn fingerprint(pr: &ProviderRow) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    pr.base_url.as_str().hash(&mut hasher);
    pr.api_token.hash(&mut hasher);
    pr.kind.hash(&mut hasher);
    hasher.finish()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::ProviderClientCache;
    use brarr_core::{ProviderError, Release, TmdbId, TrackerProvider};
    use std::sync::Arc;
    use uuid::Uuid;

    /// Minimal provider used only to occupy a cache slot. Its identity is
    /// the `tag` so tests can assert which instance came back.
    struct StubProvider {
        tag: &'static str,
    }

    impl TrackerProvider for StubProvider {
        fn name(&self) -> &str {
            self.tag
        }

        fn search_by_tmdb(
            &self,
            _tmdb: TmdbId,
        ) -> brarr_core::ProviderFuture<'_, Result<Vec<Release>, ProviderError>> {
            Box::pin(async { Ok(Vec::new()) })
        }
    }

    fn stub(tag: &'static str) -> Arc<dyn TrackerProvider> {
        Arc::new(StubProvider { tag })
    }

    #[test]
    fn get_returns_put_value_on_fingerprint_match() {
        let cache = ProviderClientCache::new();
        let id = Uuid::nil();
        cache.put(id, 1, stub("a"));
        let got = cache.get(id, 1).expect("hit");
        assert_eq!(got.name(), "a");
    }

    #[test]
    fn fingerprint_mismatch_is_a_miss() {
        let cache = ProviderClientCache::new();
        let id = Uuid::nil();
        cache.put(id, 1, stub("a"));
        assert!(cache.get(id, 2).is_none());
    }

    #[test]
    fn absent_id_is_a_miss() {
        let cache = ProviderClientCache::new();
        assert!(cache.get(Uuid::nil(), 1).is_none());
    }

    #[test]
    fn put_overwrites_previous_entry() {
        let cache = ProviderClientCache::new();
        let id = Uuid::nil();
        cache.put(id, 1, stub("old"));
        cache.put(id, 2, stub("new"));
        assert!(cache.get(id, 1).is_none(), "old fingerprint no longer hits");
        assert_eq!(cache.get(id, 2).expect("new hit").name(), "new");
    }
}
