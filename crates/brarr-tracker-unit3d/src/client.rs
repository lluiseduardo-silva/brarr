//! Cliente HTTP async para a API UNIT3D.

use std::time::Duration;

use brarr_core::{Release, TmdbId, TrackerSource};
use reqwest::{Client, header};
use url::Url;

use crate::dto::{Envelope, Unit3dTorrent};
use crate::error::ClientError;
use crate::retry::{RetryConfig, run_with_retry};

/// Default timeout para qualquer request feito pelo cliente.
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);

/// User-Agent advertised on every outgoing request. Some UNIT3D
/// deployments behind Cloudflare or similar challenge the reqwest
/// default UA. A stable, identifiable string also helps tracker
/// operators reason about brarr traffic in their logs.
const USER_AGENT: &str = concat!("brarr/", env!("CARGO_PKG_VERSION"));

/// Structured outcome of a connectivity probe. Returned by
/// [`Unit3dClient::ping`] (and the equivalent method on the Newznab
/// client) so the UI can render a green/red badge plus a one-line
/// human-readable detail without parsing free-form error strings.
#[derive(Debug, Clone)]
pub struct PingReport {
    /// `true` iff the probe round-tripped successfully and the response
    /// passed the minimum sanity check (2xx status, envelope shape).
    pub ok: bool,
    /// HTTP status returned by the upstream. `0` if the request never
    /// reached the server (DNS failure, TLS error, etc.) — those cases
    /// surface through [`ClientError`] instead.
    pub http_status: u16,
    /// Round-trip time in milliseconds. Capped at `u32::MAX` so it
    /// always fits a single column in the UI.
    pub elapsed_ms: u32,
    /// Short human-readable detail (item count, error excerpt, etc.).
    pub detail: String,
}

/// Cliente HTTP para um único tracker UNIT3D.
///
/// Um cliente carrega um [`TrackerSource`] (nome + `base_url`) e o
/// token de autenticação. Cada instância representa uma conexão lógica
/// com um tracker — para varrer múltiplos trackers em paralelo, instancie
/// um cliente por tracker e use `futures::join_all` ou similar.
///
/// O cliente é `Clone` (compartilha o `reqwest::Client` interno via `Arc`),
/// barato de copiar entre tasks.
#[derive(Debug, Clone)]
pub struct Unit3dClient {
    http: Client,
    base_url: Url,
    tracker: TrackerSource,
    retry: RetryConfig,
}

impl Unit3dClient {
    /// Constrói um cliente. Configura o header `Authorization: Bearer <token>`
    /// e um timeout default de 30s. Aplica [`RetryConfig::default`] em
    /// chamadas de busca; use [`Self::with_retry`] para customizar.
    ///
    /// # Errors
    ///
    /// - [`ClientError::InvalidToken`] se o token contiver caracteres
    ///   que não podem virar valor de header HTTP (não-ASCII, control chars).
    /// - [`ClientError::ClientBuild`] se o builder do `reqwest` falhar
    ///   (config TLS do sistema quebrada, por exemplo).
    pub fn new(tracker: TrackerSource, token: &str) -> Result<Self, ClientError> {
        let mut headers = header::HeaderMap::new();
        let mut auth_value = header::HeaderValue::from_str(&format!("Bearer {token}"))
            .map_err(|_| ClientError::InvalidToken)?;
        auth_value.set_sensitive(true);
        headers.insert(header::AUTHORIZATION, auth_value);
        headers.insert(
            header::ACCEPT,
            header::HeaderValue::from_static("application/json"),
        );

        let http = Client::builder()
            .user_agent(USER_AGENT)
            .default_headers(headers)
            .timeout(DEFAULT_TIMEOUT)
            .build()
            .map_err(ClientError::ClientBuild)?;

        let base_url = tracker.base_url.clone();
        Ok(Self {
            http,
            base_url,
            tracker,
            retry: RetryConfig::default(),
        })
    }

    /// Substitui a política de retry. Útil em testes (`RetryConfig::disabled()`
    /// para não esperar entre as tentativas) ou para configurar mais
    /// tentativas em ambientes flakey.
    #[must_use]
    pub const fn with_retry(mut self, retry: RetryConfig) -> Self {
        self.retry = retry;
        self
    }

    /// Reference to the [`TrackerSource`] this client was built with.
    ///
    /// Exposed so the [`brarr_core::TrackerProvider`] impl (defined in
    /// `provider_impl.rs`) can read the tracker name without cloning.
    #[must_use]
    pub const fn tracker_source(&self) -> &TrackerSource {
        &self.tracker
    }

    /// Connectivity probe. Hits `GET /api/torrents/filter?tmdbId=1`
    /// (Toy Story — universally available test id) and returns the
    /// HTTP status code along with the number of items the server
    /// reported. A `200` with non-zero items confirms URL + auth +
    /// schema are all good; a `401`/`403` indicates a bad token; a
    /// `404` typically means a wrong `base_url`.
    ///
    /// Skips the retry policy on purpose — the operator wants a fast
    /// signal, not a 12-second exponential-backoff wait when the URL
    /// is plain wrong.
    ///
    /// # Errors
    ///
    /// Returns [`ClientError`] on transport failure, bad status code,
    /// or unparseable response. Each variant maps to a distinct
    /// remediation hint, so the caller can render a precise UI message.
    pub async fn ping(&self) -> Result<PingReport, ClientError> {
        let url = self.base_url.join("api/torrents/filter")?;
        let started = std::time::Instant::now();
        let resp = self.http.get(url).query(&[("tmdbId", 1u32)]).send().await?;
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        let elapsed_ms = u32::try_from(started.elapsed().as_millis()).unwrap_or(u32::MAX);
        if !status.is_success() {
            return Ok(PingReport {
                ok: false,
                http_status: status.as_u16(),
                elapsed_ms,
                detail: format!(
                    "{} — {}",
                    status,
                    body.chars().take(120).collect::<String>()
                ),
            });
        }
        // Try to extract item count from the envelope so the operator
        // gets a sanity check that the JSON shape matches.
        let items: Option<usize> = serde_json::from_str::<Envelope<Vec<Unit3dTorrent>>>(&body)
            .ok()
            .map(|env| env.data.len());
        let detail = items.map_or_else(
            || {
                "status 200 but envelope did not deserialize (token works, schema may differ)"
                    .into()
            },
            |n| format!("status 200, envelope OK, {n} item(s) for tmdb=1"),
        );
        Ok(PingReport {
            ok: true,
            http_status: 200,
            elapsed_ms,
            detail,
        })
    }

    /// Endpoint `GET /api/torrents/filter?tmdbId=<id>` — retorna a lista
    /// de releases que combinam com o ID TMDB no tracker.
    ///
    /// Aplica [`RetryConfig`] em falhas transientes (timeout, 5xx,
    /// JSON truncado).
    ///
    /// # Errors
    ///
    /// Veja [`ClientError`] — qualquer falha de rede, status
    /// `4xx`/`5xx`, JSON malformado, ou conversão DTO inválida propaga aqui.
    pub async fn search_by_tmdb(&self, tmdb: TmdbId) -> Result<Vec<Release>, ClientError> {
        run_with_retry(self.retry, "search_by_tmdb", || {
            self.search_by_tmdb_once(tmdb)
        })
        .await
    }

    async fn search_by_tmdb_once(&self, tmdb: TmdbId) -> Result<Vec<Release>, ClientError> {
        let url = self.base_url.join("api/torrents/filter")?;
        let resp = self
            .http
            .get(url)
            .query(&[("tmdbId", tmdb.get())])
            .send()
            .await?
            .error_for_status()?;

        let body = resp.text().await?;
        let envelope: Envelope<Vec<Unit3dTorrent>> = match serde_json::from_str(&body) {
            Ok(v) => v,
            Err(e) => {
                // Log o body cru em debug para que falhas de
                // desserialização sejam diagnosticáveis sem precisar
                // re-rodar a request fora do brarr.
                tracing::debug!(
                    target: "brarr_tracker_unit3d::client",
                    error = %e,
                    body_len = body.len(),
                    body_excerpt = %body.chars().take(2000).collect::<String>(),
                    "search_by_tmdb: failed to decode envelope"
                );
                return Err(ClientError::BadJson(e));
            }
        };

        envelope
            .data
            .into_iter()
            .map(|dto| dto.into_release(self.tracker.clone()).map_err(Into::into))
            .collect()
    }

    /// Endpoint `GET /api/torrents/{id}` — busca um release específico
    /// pelo ID do tracker (string opaca).
    ///
    /// # Errors
    ///
    /// Veja [`ClientError`].
    pub async fn get_torrent(&self, id: &str) -> Result<Release, ClientError> {
        run_with_retry(self.retry, "get_torrent", || self.get_torrent_once(id)).await
    }

    async fn get_torrent_once(&self, id: &str) -> Result<Release, ClientError> {
        let url = self.base_url.join(&format!("api/torrents/{id}"))?;
        let body = self
            .http
            .get(url)
            .send()
            .await?
            .error_for_status()?
            .text()
            .await?;
        let envelope: Envelope<Unit3dTorrent> =
            serde_json::from_str(&body).map_err(ClientError::BadJson)?;
        envelope
            .data
            .into_release(self.tracker.clone())
            .map_err(Into::into)
    }
}
