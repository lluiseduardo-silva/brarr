//! Retry com backoff exponencial para chamadas HTTP transientes.
//!
//! Distingue erros transientes (timeout, connect refused, 5xx, JSON
//! truncado) de permanentes (4xx, token inválido, conversão de DTO).
//! Só retenta os primeiros — os outros indicam bug ou estado externo
//! que não vai se curar com mais tentativas.

use std::time::Duration;

use tokio::time::sleep;
use tracing::{debug, warn};

use crate::error::ClientError;

/// Política de retry aplicada a cada request do
/// [`Unit3dClient`](crate::Unit3dClient).
///
/// `max_attempts` é o número **total** de tentativas (não apenas
/// retries). `max_attempts = 1` desliga o retry. `base_delay` é o
/// intervalo da primeira espera; a *n*-ésima espera é
/// `base_delay * 2^(n-1)`.
#[derive(Debug, Clone, Copy)]
pub struct RetryConfig {
    /// Quantas tentativas totais. Mínimo: 1 (desliga retry).
    pub max_attempts: u32,
    /// Atraso base; dobra a cada falha.
    pub base_delay: Duration,
}

impl Default for RetryConfig {
    fn default() -> Self {
        Self {
            max_attempts: 3,
            base_delay: Duration::from_millis(200),
        }
    }
}

impl RetryConfig {
    /// Desliga retry — exatamente uma tentativa.
    #[must_use]
    pub const fn disabled() -> Self {
        Self {
            max_attempts: 1,
            base_delay: Duration::from_millis(0),
        }
    }

    /// Delay para a *n*-ésima espera (1-indexed): `base * 2^(n-1)`.
    #[must_use]
    pub fn delay_for(&self, attempt_index: u32) -> Duration {
        // attempt_index 1 → base, 2 → 2*base, 3 → 4*base, ...
        // saturating shift evita overflow se alguém botar max_attempts gigante.
        let shift = attempt_index.saturating_sub(1).min(20);
        self.base_delay.saturating_mul(1_u32 << shift)
    }
}

/// Decide se um [`ClientError`] vale a pena retentar.
///
/// Transientes:
/// - `Http` com erro de transporte (`is_timeout` / `is_connect` /
///   `is_request`) ou status `5xx`.
/// - `BadJson` — frequentemente correlaciona com resposta truncada
///   ou compressão indevida pelo proxy.
///
/// Permanentes (não retenta):
/// - `Http` com 4xx — auth, not-found, rate limit semântico do tracker.
/// - `BadUrl`, `Conversion`, `InvalidToken`, `ClientBuild`.
#[must_use]
pub fn is_transient(err: &ClientError) -> bool {
    match err {
        ClientError::Http(e) => {
            if e.is_timeout() || e.is_connect() || e.is_request() {
                return true;
            }
            e.status().is_some_and(|s| s.is_server_error())
        }
        ClientError::BadJson(_) => true,
        ClientError::BadUrl(_)
        | ClientError::Conversion(_)
        | ClientError::InvalidToken
        | ClientError::ClientBuild(_) => false,
    }
}

/// Executa `op` até `cfg.max_attempts` vezes, esperando backoff entre
/// tentativas. Retorna o sucesso ou o último erro encontrado.
///
/// `op_label` é só pra log — descreve o que está sendo tentado (e.g.,
/// `"search_by_tmdb"`).
pub async fn run_with_retry<F, Fut, T>(
    cfg: RetryConfig,
    op_label: &'static str,
    mut op: F,
) -> Result<T, ClientError>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<T, ClientError>>,
{
    let mut last_err: Option<ClientError> = None;
    for attempt in 1..=cfg.max_attempts {
        match op().await {
            Ok(v) => {
                if attempt > 1 {
                    debug!(
                        target: "brarr_tracker_unit3d::retry",
                        %op_label,
                        attempt,
                        "succeeded after retry"
                    );
                }
                return Ok(v);
            }
            Err(e) => {
                if !is_transient(&e) || attempt == cfg.max_attempts {
                    last_err = Some(e);
                    break;
                }
                let delay = cfg.delay_for(attempt);
                warn!(
                    target: "brarr_tracker_unit3d::retry",
                    %op_label,
                    attempt,
                    max_attempts = cfg.max_attempts,
                    delay_ms = u64::try_from(delay.as_millis()).unwrap_or(u64::MAX),
                    error = %e,
                    "transient failure, retrying after backoff"
                );
                last_err = Some(e);
                sleep(delay).await;
            }
        }
    }
    // SAFETY-ish: loop entered at least once with attempt=1, so
    // last_err é sempre Some por aqui. Mas evitamos `unwrap()` pra
    // não disparar lint pedantic — usa `?`-style fallback de cortesia.
    Err(last_err.unwrap_or(ClientError::InvalidToken))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::{RetryConfig, is_transient, run_with_retry};
    use crate::error::ClientError;
    use std::cell::Cell;
    use std::time::Duration;

    #[test]
    fn retry_config_default_is_three_attempts() {
        let c = RetryConfig::default();
        assert_eq!(c.max_attempts, 3);
        assert_eq!(c.base_delay, Duration::from_millis(200));
    }

    #[test]
    fn retry_config_disabled_means_one_attempt() {
        let c = RetryConfig::disabled();
        assert_eq!(c.max_attempts, 1);
    }

    #[test]
    fn delay_doubles_per_attempt() {
        let c = RetryConfig {
            max_attempts: 4,
            base_delay: Duration::from_millis(100),
        };
        assert_eq!(c.delay_for(1), Duration::from_millis(100));
        assert_eq!(c.delay_for(2), Duration::from_millis(200));
        assert_eq!(c.delay_for(3), Duration::from_millis(400));
    }

    #[test]
    fn delay_saturates_at_huge_attempt_index() {
        let c = RetryConfig::default();
        // attempt 100 deveria gerar 200 * 2^99 — saturating evita
        let _ = c.delay_for(100); // não deve panic
    }

    #[test]
    fn is_transient_classifies_bad_json_as_transient() {
        let json_err = serde_json::from_str::<u32>("not json").unwrap_err();
        let err = ClientError::BadJson(json_err);
        assert!(is_transient(&err));
    }

    #[test]
    fn is_transient_classifies_invalid_token_as_permanent() {
        assert!(!is_transient(&ClientError::InvalidToken));
    }

    #[test]
    fn is_transient_classifies_bad_url_as_permanent() {
        let url_err = url::Url::parse("not a url").unwrap_err();
        assert!(!is_transient(&ClientError::BadUrl(url_err)));
    }

    #[tokio::test]
    async fn run_with_retry_succeeds_on_first_attempt() {
        let cfg = RetryConfig::default();
        let attempts = Cell::new(0_u32);
        let result: Result<u32, ClientError> = run_with_retry(cfg, "test", || {
            attempts.set(attempts.get() + 1);
            async { Ok(42_u32) }
        })
        .await;
        assert_eq!(result.unwrap(), 42);
        assert_eq!(attempts.get(), 1);
    }

    #[tokio::test]
    async fn run_with_retry_does_not_retry_permanent_error() {
        let cfg = RetryConfig::default();
        let attempts = Cell::new(0_u32);
        let result: Result<u32, ClientError> = run_with_retry(cfg, "test", || {
            attempts.set(attempts.get() + 1);
            async { Err::<u32, _>(ClientError::InvalidToken) }
        })
        .await;
        assert!(result.is_err());
        assert_eq!(attempts.get(), 1);
    }

    #[tokio::test]
    async fn run_with_retry_retries_transient_error_up_to_max() {
        let cfg = RetryConfig {
            max_attempts: 3,
            base_delay: Duration::from_millis(0),
        };
        let attempts = Cell::new(0_u32);
        let result: Result<u32, ClientError> = run_with_retry(cfg, "test", || {
            attempts.set(attempts.get() + 1);
            async {
                let e = serde_json::from_str::<u32>("not json").unwrap_err();
                Err::<u32, _>(ClientError::BadJson(e))
            }
        })
        .await;
        assert!(result.is_err());
        assert_eq!(attempts.get(), 3);
    }

    #[tokio::test]
    async fn run_with_retry_recovers_on_second_attempt() {
        let cfg = RetryConfig {
            max_attempts: 3,
            base_delay: Duration::from_millis(0),
        };
        let attempts = Cell::new(0_u32);
        let result: Result<u32, ClientError> = run_with_retry(cfg, "test", || {
            attempts.set(attempts.get() + 1);
            let current = attempts.get();
            async move {
                if current == 1 {
                    let e = serde_json::from_str::<u32>("not json").unwrap_err();
                    Err::<u32, _>(ClientError::BadJson(e))
                } else {
                    Ok(42_u32)
                }
            }
        })
        .await;
        assert_eq!(result.unwrap(), 42);
        assert_eq!(attempts.get(), 2);
    }
}
