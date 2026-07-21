//! Observability metrics — per-provider fan-out timings and indexer
//! endpoint request timings.
//!
//! Two write paths feed these tables:
//! - [`crate::search::run_search`] inserts one [`ProviderMetricInsert`]
//!   per provider per fan-out (how long the upstream took, ok / error /
//!   timeout, release count).
//! - The Torznab/Newznab metrics middleware inserts one
//!   [`EndpointMetricInsert`] per request to `/torznab/*` / `/newznab/*`
//!   (total latency as Sonarr/Radarr experience it, plus whether the
//!   short-TTL search cache absorbed the request).
//!
//! Reads aggregate in Rust rather than SQL: the working set inside a
//! health window is small (providers × searches per day), and computing
//! percentiles over a sorted `Vec<u64>` is simpler and more testable
//! than window-function SQL.

use time::OffsetDateTime;
use uuid::Uuid;

use crate::{AppError, db::Pool};

/// Terminal state of one provider dispatch inside a search fan-out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderOutcome {
    /// Provider answered inside the budget.
    Ok,
    /// Provider (or client build) failed with an error.
    Error,
    /// Provider exceeded [`crate::search::PER_PROVIDER_BUDGET`].
    Timeout,
}

impl ProviderOutcome {
    /// Stable string form stored in the `outcome` column.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Error => "error",
            Self::Timeout => "timeout",
        }
    }
}

/// One provider dispatch measurement, ready to insert.
#[derive(Debug, Clone)]
pub struct ProviderMetricInsert {
    /// Search this dispatch belonged to.
    pub search_id: Option<Uuid>,
    /// Provider row id (snapshot — the row may be deleted later).
    pub provider_id: Option<Uuid>,
    /// Provider display name at dispatch time.
    pub provider_name: String,
    /// `unit3d` / `newznab` / `torznab` / `plugin`.
    pub provider_kind: String,
    /// How the dispatch ended.
    pub outcome: ProviderOutcome,
    /// Error text when `outcome != Ok`.
    pub error: Option<String>,
    /// Wall-clock duration of the dispatch (client build + HTTP).
    pub duration_ms: u64,
    /// Releases returned (0 on failure).
    pub release_count: u32,
}

/// Insert one provider measurement.
///
/// # Errors
///
/// Surfaces [`AppError::Database`] on SQL failure.
pub async fn insert_provider_metric(pool: &Pool, m: ProviderMetricInsert) -> Result<(), AppError> {
    sqlx::query(
        "INSERT INTO provider_metrics \
           (id, search_id, provider_id, provider_name, provider_kind, outcome, error, \
            duration_ms, release_count, recorded_at) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(Uuid::new_v4().to_string())
    .bind(m.search_id.map(|u| u.to_string()))
    .bind(m.provider_id.map(|u| u.to_string()))
    .bind(&m.provider_name)
    .bind(&m.provider_kind)
    .bind(m.outcome.as_str())
    .bind(m.error.as_deref())
    .bind(i64::try_from(m.duration_ms).unwrap_or(i64::MAX))
    .bind(i64::from(m.release_count))
    .bind(OffsetDateTime::now_utc().unix_timestamp())
    .execute(pool)
    .await?;
    Ok(())
}

/// One indexer-endpoint request measurement, ready to insert.
#[derive(Debug, Clone)]
pub struct EndpointMetricInsert {
    /// `"torznab"` or `"newznab"`.
    pub endpoint: &'static str,
    /// The `t=` function (`caps` / `movie` / `tvsearch` / `search`) or
    /// `download` for the proxy route.
    pub function: String,
    /// HTTP status the handler returned.
    pub status: u16,
    /// Total handler latency (what the *arr client experienced).
    pub duration_ms: u64,
    /// `Some(true)` = served from the search cache, `Some(false)` = a
    /// full fan-out ran, `None` = the function doesn't search (`caps`,
    /// `download`, probes).
    pub cache_hit: Option<bool>,
}

/// Insert one endpoint measurement.
///
/// # Errors
///
/// Surfaces [`AppError::Database`] on SQL failure.
pub async fn insert_endpoint_metric(pool: &Pool, m: EndpointMetricInsert) -> Result<(), AppError> {
    let cache = m.cache_hit.map(|hit| if hit { "hit" } else { "miss" });
    sqlx::query(
        "INSERT INTO endpoint_metrics \
           (id, endpoint, function, status, duration_ms, cache, recorded_at) \
         VALUES (?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(Uuid::new_v4().to_string())
    .bind(m.endpoint)
    .bind(&m.function)
    .bind(i64::from(m.status))
    .bind(i64::try_from(m.duration_ms).unwrap_or(i64::MAX))
    .bind(cache)
    .bind(OffsetDateTime::now_utc().unix_timestamp())
    .execute(pool)
    .await?;
    Ok(())
}

/// Aggregated health of one provider over a time window.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderStats {
    /// Provider display name (grouping key — survives provider deletion).
    pub provider_name: String,
    /// Kind as of the most recent sample.
    pub provider_kind: String,
    /// Total dispatches in the window.
    pub total: u64,
    /// Dispatches that returned releases.
    pub ok: u64,
    /// Dispatches that errored.
    pub errors: u64,
    /// Dispatches that blew the per-provider budget.
    pub timeouts: u64,
    /// Mean duration across all dispatches.
    pub avg_ms: u64,
    /// 50th percentile duration.
    pub p50_ms: u64,
    /// 95th percentile duration — the "inconsistency" signal.
    pub p95_ms: u64,
    /// Worst duration in the window.
    pub max_ms: u64,
    /// Total releases returned in the window.
    pub releases: u64,
    /// Most recent error text, if any dispatch failed.
    pub last_error: Option<String>,
    /// Unix timestamp of the most recent sample.
    pub last_seen_unix: i64,
}

/// Aggregated health of one endpoint function over a time window.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EndpointStats {
    /// `"torznab"` or `"newznab"`.
    pub endpoint: String,
    /// `t=` function or `download`.
    pub function: String,
    /// Total requests in the window.
    pub total: u64,
    /// Requests answered with a non-2xx status.
    pub errors: u64,
    /// Search requests absorbed by the TTL cache.
    pub cache_hits: u64,
    /// Search requests that ran a full fan-out.
    pub cache_misses: u64,
    /// Mean latency.
    pub avg_ms: u64,
    /// 50th percentile latency.
    pub p50_ms: u64,
    /// 95th percentile latency.
    pub p95_ms: u64,
    /// Worst latency in the window.
    pub max_ms: u64,
}

/// One raw endpoint request row for the "recent requests" table.
#[derive(Debug, Clone)]
pub struct EndpointRequestRow {
    /// `"torznab"` or `"newznab"`.
    pub endpoint: String,
    /// `t=` function or `download`.
    pub function: String,
    /// HTTP status returned.
    pub status: u16,
    /// Handler latency.
    pub duration_ms: u64,
    /// `Some(true)` hit / `Some(false)` miss / `None` non-search.
    pub cache_hit: Option<bool>,
    /// Request timestamp.
    pub recorded_at: OffsetDateTime,
}

/// Per-provider aggregates for samples recorded at or after `since_unix`,
/// sorted by provider name.
///
/// # Errors
///
/// Surfaces [`AppError::Database`] on SQL failure.
pub async fn provider_stats(pool: &Pool, since_unix: i64) -> Result<Vec<ProviderStats>, AppError> {
    use sqlx::Row;
    let rows = sqlx::query(
        "SELECT provider_name, provider_kind, outcome, error, duration_ms, release_count, recorded_at \
         FROM provider_metrics WHERE recorded_at >= ? ORDER BY recorded_at ASC",
    )
    .bind(since_unix)
    .fetch_all(pool)
    .await?;

    let mut by_name: std::collections::BTreeMap<String, ProviderAccum> =
        std::collections::BTreeMap::new();
    for row in &rows {
        let name: String = row.try_get("provider_name")?;
        let kind: String = row.try_get("provider_kind")?;
        let outcome: String = row.try_get("outcome")?;
        let error: Option<String> = row.try_get("error")?;
        let duration: i64 = row.try_get("duration_ms")?;
        let releases: i64 = row.try_get("release_count")?;
        let recorded: i64 = row.try_get("recorded_at")?;
        let acc = by_name.entry(name).or_default();
        // Rows arrive in ascending recorded_at order, so the last write
        // per provider wins for "latest" fields.
        acc.kind = kind;
        acc.last_seen_unix = recorded;
        acc.durations.push(u64::try_from(duration).unwrap_or(0));
        acc.releases += u64::try_from(releases).unwrap_or(0);
        match outcome.as_str() {
            "ok" => acc.ok += 1,
            "timeout" => acc.timeouts += 1,
            _ => acc.errors += 1,
        }
        if let Some(e) = error {
            acc.last_error = Some(e);
        }
    }

    Ok(by_name
        .into_iter()
        .map(|(name, acc)| acc.finish(name))
        .collect())
}

#[derive(Default)]
struct ProviderAccum {
    kind: String,
    ok: u64,
    errors: u64,
    timeouts: u64,
    durations: Vec<u64>,
    releases: u64,
    last_error: Option<String>,
    last_seen_unix: i64,
}

impl ProviderAccum {
    fn finish(mut self, name: String) -> ProviderStats {
        self.durations.sort_unstable();
        ProviderStats {
            provider_name: name,
            provider_kind: self.kind,
            total: self.ok + self.errors + self.timeouts,
            ok: self.ok,
            errors: self.errors,
            timeouts: self.timeouts,
            avg_ms: mean(&self.durations),
            p50_ms: percentile(&self.durations, 50),
            p95_ms: percentile(&self.durations, 95),
            max_ms: self.durations.last().copied().unwrap_or(0),
            releases: self.releases,
            last_error: self.last_error,
            last_seen_unix: self.last_seen_unix,
        }
    }
}

/// Per-endpoint-function aggregates for requests recorded at or after
/// `since_unix`, sorted by (endpoint, function).
///
/// # Errors
///
/// Surfaces [`AppError::Database`] on SQL failure.
pub async fn endpoint_stats(pool: &Pool, since_unix: i64) -> Result<Vec<EndpointStats>, AppError> {
    use sqlx::Row;
    let rows = sqlx::query(
        "SELECT endpoint, function, status, duration_ms, cache \
         FROM endpoint_metrics WHERE recorded_at >= ?",
    )
    .bind(since_unix)
    .fetch_all(pool)
    .await?;

    let mut by_key: std::collections::BTreeMap<(String, String), EndpointAccum> =
        std::collections::BTreeMap::new();
    for row in &rows {
        let endpoint: String = row.try_get("endpoint")?;
        let function: String = row.try_get("function")?;
        let status: i64 = row.try_get("status")?;
        let duration: i64 = row.try_get("duration_ms")?;
        let cache: Option<String> = row.try_get("cache")?;
        let acc = by_key.entry((endpoint, function)).or_default();
        acc.durations.push(u64::try_from(duration).unwrap_or(0));
        if !(200..300).contains(&status) {
            acc.errors += 1;
        }
        match cache.as_deref() {
            Some("hit") => acc.cache_hits += 1,
            Some("miss") => acc.cache_misses += 1,
            _ => {}
        }
    }

    Ok(by_key
        .into_iter()
        .map(|((endpoint, function), acc)| acc.finish(endpoint, function))
        .collect())
}

#[derive(Default)]
struct EndpointAccum {
    errors: u64,
    cache_hits: u64,
    cache_misses: u64,
    durations: Vec<u64>,
}

impl EndpointAccum {
    fn finish(mut self, endpoint: String, function: String) -> EndpointStats {
        self.durations.sort_unstable();
        EndpointStats {
            endpoint,
            function,
            total: u64::try_from(self.durations.len()).unwrap_or(u64::MAX),
            errors: self.errors,
            cache_hits: self.cache_hits,
            cache_misses: self.cache_misses,
            avg_ms: mean(&self.durations),
            p50_ms: percentile(&self.durations, 50),
            p95_ms: percentile(&self.durations, 95),
            max_ms: self.durations.last().copied().unwrap_or(0),
        }
    }
}

/// The most recent `limit` endpoint requests, newest first (capped at
/// 200).
///
/// # Errors
///
/// Surfaces [`AppError::Database`] on SQL failure.
pub async fn recent_endpoint_requests(
    pool: &Pool,
    limit: u32,
) -> Result<Vec<EndpointRequestRow>, AppError> {
    use sqlx::Row;
    let limit = limit.clamp(1, 200);
    let rows = sqlx::query(
        "SELECT endpoint, function, status, duration_ms, cache, recorded_at \
         FROM endpoint_metrics ORDER BY recorded_at DESC, rowid DESC LIMIT ?",
    )
    .bind(i64::from(limit))
    .fetch_all(pool)
    .await?;
    rows.iter()
        .map(|row| {
            let status: i64 = row.try_get("status")?;
            let duration: i64 = row.try_get("duration_ms")?;
            let cache: Option<String> = row.try_get("cache")?;
            let recorded: i64 = row.try_get("recorded_at")?;
            Ok(EndpointRequestRow {
                endpoint: row.try_get("endpoint")?,
                function: row.try_get("function")?,
                status: u16::try_from(status).unwrap_or(0),
                duration_ms: u64::try_from(duration).unwrap_or(0),
                cache_hit: cache.as_deref().map(|c| c == "hit"),
                recorded_at: OffsetDateTime::from_unix_timestamp(recorded)
                    .map_err(|e| AppError::InvalidInput(format!("invalid timestamp: {e}")))?,
            })
        })
        .collect()
}

/// Delete metric rows older than `cutoff_unix` from both tables. Returns
/// the total rows removed.
///
/// # Errors
///
/// Surfaces [`AppError::Database`] on SQL failure.
pub async fn prune(pool: &Pool, cutoff_unix: i64) -> Result<u64, AppError> {
    let providers = sqlx::query("DELETE FROM provider_metrics WHERE recorded_at < ?")
        .bind(cutoff_unix)
        .execute(pool)
        .await?
        .rows_affected();
    let endpoints = sqlx::query("DELETE FROM endpoint_metrics WHERE recorded_at < ?")
        .bind(cutoff_unix)
        .execute(pool)
        .await?
        .rows_affected();
    Ok(providers + endpoints)
}

/// Mean of a slice of durations, zero for an empty slice.
fn mean(sorted: &[u64]) -> u64 {
    let Ok(n) = u64::try_from(sorted.len()) else {
        return 0;
    };
    if n == 0 {
        return 0;
    }
    sorted.iter().sum::<u64>() / n
}

/// Nearest-rank percentile over an ascending-sorted slice. `pct` is
/// 0..=100; an empty slice yields 0. Uses the ceiling form
/// (`rank = ⌈pct/100 × n⌉`), so p95 of a small sample reports the worst
/// observation instead of hiding it — exactly the tail we built this to
/// expose.
fn percentile(sorted: &[u64], pct: usize) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    let rank = (pct * sorted.len()).div_ceil(100).max(1);
    sorted[(rank - 1).min(sorted.len() - 1)]
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use crate::db::open_memory;

    fn provider_metric(name: &str, outcome: ProviderOutcome, ms: u64) -> ProviderMetricInsert {
        ProviderMetricInsert {
            search_id: None,
            provider_id: None,
            provider_name: name.to_string(),
            provider_kind: "unit3d".to_string(),
            outcome,
            error: matches!(outcome, ProviderOutcome::Error)
                .then(|| "connection refused".to_string()),
            duration_ms: ms,
            release_count: u32::from(matches!(outcome, ProviderOutcome::Ok)) * 3,
        }
    }

    #[test]
    fn percentile_nearest_rank() {
        let sorted = [10, 20, 30, 40, 100];
        assert_eq!(percentile(&sorted, 50), 30);
        assert_eq!(percentile(&sorted, 95), 100);
        assert_eq!(percentile(&sorted, 0), 10);
        assert_eq!(percentile(&[], 95), 0);
        assert_eq!(percentile(&[7], 95), 7);
    }

    #[test]
    fn mean_handles_empty_and_values() {
        assert_eq!(mean(&[]), 0);
        assert_eq!(mean(&[10, 20, 30]), 20);
    }

    #[tokio::test]
    async fn provider_stats_aggregates_outcomes_and_durations() {
        let pool = open_memory().await.unwrap();
        insert_provider_metric(&pool, provider_metric("loca", ProviderOutcome::Ok, 100))
            .await
            .unwrap();
        insert_provider_metric(&pool, provider_metric("loca", ProviderOutcome::Ok, 300))
            .await
            .unwrap();
        insert_provider_metric(
            &pool,
            provider_metric("loca", ProviderOutcome::Timeout, 15_000),
        )
        .await
        .unwrap();
        insert_provider_metric(&pool, provider_metric("nzb", ProviderOutcome::Error, 50))
            .await
            .unwrap();

        let stats = provider_stats(&pool, 0).await.unwrap();
        assert_eq!(stats.len(), 2);

        let loca = &stats[0];
        assert_eq!(loca.provider_name, "loca");
        assert_eq!(loca.total, 3);
        assert_eq!(loca.ok, 2);
        assert_eq!(loca.timeouts, 1);
        assert_eq!(loca.errors, 0);
        assert_eq!(loca.releases, 6);
        assert_eq!(loca.max_ms, 15_000);
        assert_eq!(loca.p50_ms, 300);
        assert!(loca.last_error.is_none());

        let nzb = &stats[1];
        assert_eq!(nzb.errors, 1);
        assert_eq!(nzb.last_error.as_deref(), Some("connection refused"));
    }

    #[tokio::test]
    async fn provider_stats_window_excludes_older_samples() {
        let pool = open_memory().await.unwrap();
        insert_provider_metric(&pool, provider_metric("loca", ProviderOutcome::Ok, 100))
            .await
            .unwrap();
        let future = OffsetDateTime::now_utc().unix_timestamp() + 3600;
        let stats = provider_stats(&pool, future).await.unwrap();
        assert!(stats.is_empty());
    }

    #[tokio::test]
    async fn endpoint_stats_groups_by_endpoint_and_function() {
        let pool = open_memory().await.unwrap();
        for (status, ms, hit) in [(200_u16, 40_u64, Some(true)), (200, 900, Some(false))] {
            insert_endpoint_metric(
                &pool,
                EndpointMetricInsert {
                    endpoint: "torznab",
                    function: "movie".to_string(),
                    status,
                    duration_ms: ms,
                    cache_hit: hit,
                },
            )
            .await
            .unwrap();
        }
        insert_endpoint_metric(
            &pool,
            EndpointMetricInsert {
                endpoint: "newznab",
                function: "caps".to_string(),
                status: 401,
                duration_ms: 5,
                cache_hit: None,
            },
        )
        .await
        .unwrap();

        let stats = endpoint_stats(&pool, 0).await.unwrap();
        assert_eq!(stats.len(), 2);
        // BTreeMap ordering: newznab < torznab.
        assert_eq!(stats[0].endpoint, "newznab");
        assert_eq!(stats[0].errors, 1);
        assert_eq!(stats[1].endpoint, "torznab");
        assert_eq!(stats[1].total, 2);
        assert_eq!(stats[1].cache_hits, 1);
        assert_eq!(stats[1].cache_misses, 1);
        assert_eq!(stats[1].max_ms, 900);
    }

    #[tokio::test]
    async fn recent_endpoint_requests_returns_newest_first() {
        let pool = open_memory().await.unwrap();
        for i in 0..3_u64 {
            insert_endpoint_metric(
                &pool,
                EndpointMetricInsert {
                    endpoint: "torznab",
                    function: format!("f{i}"),
                    status: 200,
                    duration_ms: i,
                    cache_hit: None,
                },
            )
            .await
            .unwrap();
        }
        let recent = recent_endpoint_requests(&pool, 2).await.unwrap();
        assert_eq!(recent.len(), 2);
        // Same-second inserts fall back to rowid DESC → newest first.
        assert_eq!(recent[0].function, "f2");
        assert_eq!(recent[1].function, "f1");
    }

    #[tokio::test]
    async fn prune_removes_old_rows_from_both_tables() {
        let pool = open_memory().await.unwrap();
        insert_provider_metric(&pool, provider_metric("loca", ProviderOutcome::Ok, 100))
            .await
            .unwrap();
        insert_endpoint_metric(
            &pool,
            EndpointMetricInsert {
                endpoint: "torznab",
                function: "movie".to_string(),
                status: 200,
                duration_ms: 10,
                cache_hit: Some(false),
            },
        )
        .await
        .unwrap();

        // Cutoff in the past deletes nothing.
        let removed = prune(&pool, 0).await.unwrap();
        assert_eq!(removed, 0);
        // Cutoff in the future deletes both rows.
        let future = OffsetDateTime::now_utc().unix_timestamp() + 3600;
        let removed = prune(&pool, future).await.unwrap();
        assert_eq!(removed, 2);
    }
}
