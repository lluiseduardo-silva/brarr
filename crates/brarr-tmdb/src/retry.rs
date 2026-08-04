//! Exponential backoff for transient TMDB failures.
//!
//! Mirrors the policy in `brarr-tracker-unit3d`: retry timeouts, connect
//! failures, 5xx and truncated JSON; never retry a 401 or a 404, which
//! will not heal on their own. 429 *is* retried — the backoff is exactly
//! the right response to a burst.

use std::time::Duration;

use tokio::time::sleep;
use tracing::{debug, warn};

use crate::error::TmdbError;

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
pub fn is_transient(err: &TmdbError) -> bool {
    match err {
        TmdbError::Http(e) => {
            if e.is_timeout() || e.is_connect() || e.is_request() {
                return true;
            }
            e.status().is_some_and(|s| s.is_server_error())
        }
        // Truncated bodies and proxy-mangled compression both surface here.
        TmdbError::BadJson(_) | TmdbError::RateLimited => true,
        TmdbError::Unauthorized
        | TmdbError::NotFound(_)
        | TmdbError::BadUrl(_)
        | TmdbError::InvalidToken
        | TmdbError::ClientBuild(_) => false,
    }
}

/// Run `op` up to `cfg.max_attempts` times, backing off between tries.
///
/// `op_label` only feeds the log line.
///
/// # Errors
///
/// Returns the last [`TmdbError`] seen once attempts are exhausted, or
/// immediately for a permanent failure.
pub async fn run_with_retry<F, Fut, T>(
    cfg: RetryConfig,
    op_label: &'static str,
    mut op: F,
) -> Result<T, TmdbError>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T, TmdbError>>,
{
    let attempts = cfg.max_attempts.max(1);
    let mut last_err: Option<TmdbError> = None;

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
                    op = op_label,
                    attempt,
                    max = attempts,
                    delay_ms = u64::try_from(delay.as_millis()).unwrap_or(u64::MAX),
                    error = %err,
                    "transient TMDB failure, retrying"
                );
                last_err = Some(err);
                sleep(delay).await;
            }
        }
    }

    debug!(op = op_label, "TMDB retries exhausted");
    Err(last_err.unwrap_or(TmdbError::RateLimited))
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

    #[test]
    fn auth_and_not_found_are_permanent() {
        assert!(!is_transient(&TmdbError::Unauthorized));
        assert!(!is_transient(&TmdbError::NotFound("movie 1".to_owned())));
        assert!(!is_transient(&TmdbError::InvalidToken));
    }

    #[test]
    fn rate_limit_is_worth_backing_off() {
        assert!(is_transient(&TmdbError::RateLimited));
    }

    #[tokio::test]
    async fn stops_immediately_on_a_permanent_error() {
        let calls = AtomicU32::new(0);
        let cfg = RetryConfig {
            max_attempts: 5,
            base_delay: Duration::from_millis(0),
        };
        let res: Result<(), TmdbError> = run_with_retry(cfg, "test", || {
            calls.fetch_add(1, Ordering::SeqCst);
            async { Err(TmdbError::Unauthorized) }
        })
        .await;
        assert!(res.is_err());
        assert_eq!(calls.load(Ordering::SeqCst), 1, "401 must not be retried");
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
                    Err(TmdbError::RateLimited)
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
