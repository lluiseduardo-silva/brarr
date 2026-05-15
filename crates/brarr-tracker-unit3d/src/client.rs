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
