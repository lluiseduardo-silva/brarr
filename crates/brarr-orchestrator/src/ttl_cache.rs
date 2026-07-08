//! A tiny time-to-live cache used to collapse duplicate searches on the
//! Torznab/Newznab pull path.
//!
//! Sonarr/Radarr fan a single Interactive Search out into many near-
//! identical indexer requests — one per season and/or episode, and once
//! per configured brarr indexer (the `/torznab` and `/newznab` feeds
//! both run the *same* fan-out, differing only in which protocol's
//! decisions they render). Without a cache each of those requests re-runs
//! the full provider fan-out and re-persists a fresh `searches` +
//! `decisions` set, so the *arr UI stalls for minutes while brarr hammers
//! every upstream tracker repeatedly. This cache lets the first request
//! for a given [`SearchKeys`](crate::search::SearchKeys) do the real work
//! and every duplicate within `ttl` reuse the result.
//!
//! Staleness is bounded by `ttl` (default 60s — see
//! [`crate::search::SEARCH_CACHE_TTL`]): an edit to a provider or quality
//! profile takes at most one `ttl` window to show up on the pull path.
//! That's an acceptable trade for the interactive-search path; the admin
//! UI and the background poller call the uncached
//! [`crate::search::run_search`] directly and always see fresh data.
//!
//! `ttl == 0` disables the cache entirely (every `get` misses), which is
//! handy for tests and for an operator who wants to opt out.

#![allow(
    clippy::module_name_repetitions,
    reason = "TtlCache reads clearly even though it repeats the module name"
)]

use std::collections::HashMap;
use std::hash::Hash;
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

/// A clone-on-read cache whose entries expire `ttl` after insertion.
///
/// The clock is injected into [`get`](Self::get) / [`insert`](Self::insert)
/// as an `Instant` so the expiry logic is deterministically testable
/// without sleeping. Production callers pass `Instant::now()`.
pub struct TtlCache<K, V> {
    ttl: Duration,
    inner: Mutex<HashMap<K, (Instant, V)>>,
}

impl<K, V> TtlCache<K, V>
where
    K: Eq + Hash,
    V: Clone,
{
    /// Build an empty cache whose entries live for `ttl`. A `ttl` of
    /// zero yields a cache that never hits.
    #[must_use]
    pub fn new(ttl: Duration) -> Self {
        Self {
            ttl,
            inner: Mutex::new(HashMap::new()),
        }
    }

    /// The configured time-to-live.
    #[must_use]
    pub const fn ttl(&self) -> Duration {
        self.ttl
    }

    /// Return a clone of the cached value for `key` when one exists and
    /// was inserted less than `ttl` before `now`; otherwise `None`.
    ///
    /// A poisoned lock is recovered rather than propagated — a cache is a
    /// best-effort optimization, never a correctness dependency.
    #[must_use]
    pub fn get(&self, key: &K, now: Instant) -> Option<V> {
        if self.ttl.is_zero() {
            return None;
        }
        let map = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        let (stored_at, value) = map.get(key)?;
        (now.saturating_duration_since(*stored_at) < self.ttl).then(|| value.clone())
    }

    /// Insert (or overwrite) `key`'s value, stamping it with `now`.
    ///
    /// Opportunistically evicts every already-expired entry so a long-
    /// running process with a churning key space doesn't grow unbounded.
    pub fn insert(&self, key: K, value: V, now: Instant) {
        if self.ttl.is_zero() {
            return;
        }
        let mut map = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        map.retain(|_, (stored_at, _)| now.saturating_duration_since(*stored_at) < self.ttl);
        map.insert(key, (now, value));
    }
}

#[cfg(test)]
mod tests {
    use super::TtlCache;
    use std::time::{Duration, Instant};

    #[test]
    fn fresh_entry_hits() {
        let cache: TtlCache<&str, u32> = TtlCache::new(Duration::from_secs(60));
        let t0 = Instant::now();
        cache.insert("k", 42, t0);
        assert_eq!(cache.get(&"k", t0 + Duration::from_secs(30)), Some(42));
    }

    #[test]
    fn expired_entry_misses() {
        let cache: TtlCache<&str, u32> = TtlCache::new(Duration::from_secs(60));
        let t0 = Instant::now();
        cache.insert("k", 42, t0);
        assert_eq!(cache.get(&"k", t0 + Duration::from_secs(61)), None);
    }

    #[test]
    fn boundary_is_exclusive() {
        // Exactly at ttl counts as expired (`<`, not `<=`).
        let cache: TtlCache<&str, u32> = TtlCache::new(Duration::from_secs(60));
        let t0 = Instant::now();
        cache.insert("k", 7, t0);
        assert_eq!(cache.get(&"k", t0 + Duration::from_secs(60)), None);
        assert_eq!(cache.get(&"k", t0 + Duration::from_secs(59)), Some(7));
    }

    #[test]
    fn absent_key_misses() {
        let cache: TtlCache<&str, u32> = TtlCache::new(Duration::from_secs(60));
        assert_eq!(cache.get(&"nope", Instant::now()), None);
    }

    #[test]
    fn zero_ttl_never_hits() {
        let cache: TtlCache<&str, u32> = TtlCache::new(Duration::ZERO);
        let t0 = Instant::now();
        cache.insert("k", 1, t0);
        assert_eq!(cache.get(&"k", t0), None);
    }

    #[test]
    fn insert_overwrites_and_refreshes_stamp() {
        let cache: TtlCache<&str, u32> = TtlCache::new(Duration::from_secs(60));
        let t0 = Instant::now();
        cache.insert("k", 1, t0);
        // Re-insert at t0+40s with a new value; it should live until +100s.
        cache.insert("k", 2, t0 + Duration::from_secs(40));
        assert_eq!(cache.get(&"k", t0 + Duration::from_secs(90)), Some(2));
    }

    #[test]
    fn insert_evicts_expired_entries() {
        let cache: TtlCache<u32, u32> = TtlCache::new(Duration::from_secs(60));
        let t0 = Instant::now();
        cache.insert(1, 1, t0);
        // Inserting much later prunes the stale entry for key 1.
        cache.insert(2, 2, t0 + Duration::from_secs(120));
        assert_eq!(cache.get(&1, t0 + Duration::from_secs(120)), None);
        assert_eq!(cache.get(&2, t0 + Duration::from_secs(120)), Some(2));
    }
}
