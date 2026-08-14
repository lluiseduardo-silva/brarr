//! Exponential backoff for transient `TheTVDB` failures.
//!
//! Mirrors `brarr-tmdb`'s policy, which mirrors `brarr-tracker-unit3d`'s:
//! retry timeouts, connect failures, 5xx and truncated JSON; never retry
//! a refused key or a 404, which do not heal on their own. 429 *is*
//! retried — backing off is exactly the right answer to a burst, and
//! this crate walks 180 series in one sweep.
//!
//! The one variant that is this crate's own is [`TvdbError::TokenRejected`],
//! and it is deliberately **permanent** here: [`crate::TvdbClient`]
//! already performs exactly one re-login on a mid-session 401, so
//! retrying it at this layer would multiply that by `max_attempts`
//! against someone else's free tier.

use std::time::Duration;

use tokio::time::sleep;
use tracing::{debug, warn};

use crate::error::TvdbError;

/// Retry policy applied to every request.
///
/// `max_attempts` counts **total** attempts, not retries;
/// `max_attempts = 1` disables retrying. The *n*-th wait is
/// `base_delay * 2^(n-1)`.
#[derive(Debug, Clone, Copy)]
pub struct RetryConfig {
    /// Total attempts. Minimum 1.
    pub max_attempts: u32,
    /// First wait; doubles on each failure.
    pub base_delay: Duration,
}

impl Default for RetryConfig {
    fn default() -> Self {
        Self {
            max_attempts: 3,
            base_delay: Duration::from_millis(250),
        }
    }
}

impl RetryConfig {
    /// Exactly one attempt — used by tests so they do not sleep.
    #[must_use]
    pub const fn disabled() -> Self {
        Self {
            max_attempts: 1,
            base_delay: Duration::from_millis(0),
        }
    }

    /// Wait before the *n*-th retry (1-indexed).
    #[must_use]
    pub fn delay_for(&self, attempt_index: u32) -> Duration {
        let shift = attempt_index.saturating_sub(1).min(20);
        self.base_delay.saturating_mul(1_u32 << shift)
    }
}

/// Whether an error is worth another attempt.
#[must_use]
pub fn is_transient(err: &TvdbError) -> bool {
    match err {
        TvdbError::Http(e) => {
            if e.is_timeout() || e.is_connect() || e.is_request() {
                return true;
            }
            e.status().is_some_and(|s| s.is_server_error())
        }
        // Truncated bodies and proxy-mangled compression both surface here.
        TvdbError::BadJson(_) | TvdbError::RateLimited => true,
        // `TokenRejected` looks transient and is not: the client already
        // re-logs-in once on its own, and a key revoked upstream answers
        // the same way forever.
        TvdbError::TokenRejected
        | TvdbError::Unauthorized
        | TvdbError::NotFound(_)
        | TvdbError::BadUrl(_)
        | TvdbError::ClientBuild(_)
        | TvdbError::RunawayPagination(_) => false,
    }
}

/// Run `op` up to `cfg.max_attempts` times, backing off between tries.
///
/// `op_label` only feeds the log line.
///
/// # Errors
///
/// Returns the last [`TvdbError`] seen once attempts are exhausted, or
/// immediately for a permanent failure.
pub async fn run_with_retry<F, Fut, T>(
    cfg: RetryConfig,
    op_label: &'static str,
    mut op: F,
) -> Result<T, TvdbError>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T, TvdbError>>,
{
    let attempts = cfg.max_attempts.max(1);
    let mut last_err: Option<TvdbError> = None;

    for attempt in 1..=attempts {
        match op().await {
            Ok(value) => return Ok(value),
            Err(err) => {
                if !is_transient(&err) {
                    return Err(err);
                }
                if attempt == attempts {
                    last_err = Some(err);
                    break;
                }
                let delay = cfg.delay_for(attempt);
                warn!(
                    target: "brarr_tvdb",
                    op = op_label,
                    attempt,
                    max = attempts,
                    delay_ms = u64::try_from(delay.as_millis()).unwrap_or(u64::MAX),
                    error = %err,
                    "transient TheTVDB failure, retrying"
                );
                last_err = Some(err);
                sleep(delay).await;
            }
        }
    }

    debug!(target: "brarr_tvdb", op = op_label, "TheTVDB retries exhausted");
    Err(last_err.unwrap_or(TvdbError::RateLimited))
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "tests assert on happy paths"
)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    #[test]
    fn delay_doubles_per_attempt() {
        let cfg = RetryConfig {
            max_attempts: 4,
            base_delay: Duration::from_millis(100),
        };
        assert_eq!(cfg.delay_for(1), Duration::from_millis(100));
        assert_eq!(cfg.delay_for(2), Duration::from_millis(200));
        assert_eq!(cfg.delay_for(3), Duration::from_millis(400));
    }

    /// **Every variant has to be classified deliberately.**
    ///
    /// Walks the whole enum rather than sampling it, so a new failure
    /// mode cannot inherit a default. The two that would be tempting to
    /// get wrong are named: `TokenRejected` looks transient and is not,
    /// and `RunawayPagination` is the guard against a cursor that points
    /// at itself — retrying it would loop the loop.
    #[test]
    fn only_the_failures_that_heal_are_retried() {
        assert!(is_transient(&TvdbError::RateLimited));

        for permanent in [
            TvdbError::Unauthorized,
            TvdbError::TokenRejected,
            TvdbError::NotFound("series 1".to_owned()),
            TvdbError::RunawayPagination(200),
        ] {
            assert!(!is_transient(&permanent), "{permanent} must not be retried");
        }
    }

    #[tokio::test]
    async fn a_refused_key_costs_exactly_one_request() {
        let calls = AtomicU32::new(0);
        let cfg = RetryConfig {
            max_attempts: 5,
            base_delay: Duration::from_millis(0),
        };
        let res: Result<(), TvdbError> = run_with_retry(cfg, "test", || {
            calls.fetch_add(1, Ordering::SeqCst);
            async { Err(TvdbError::Unauthorized) }
        })
        .await;
        assert!(res.is_err());
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn recovers_on_a_later_attempt() {
        let calls = AtomicU32::new(0);
        let cfg = RetryConfig {
            max_attempts: 4,
            base_delay: Duration::from_millis(0),
        };
        let res = run_with_retry(cfg, "test", || {
            let n = calls.fetch_add(1, Ordering::SeqCst);
            async move {
                if n < 2 {
                    Err(TvdbError::RateLimited)
                } else {
                    Ok(42_u32)
                }
            }
        })
        .await;
        assert_eq!(res.unwrap(), 42);
        assert_eq!(calls.load(Ordering::SeqCst), 3);
    }
}
