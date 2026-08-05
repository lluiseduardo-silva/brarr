//! SABnzbd client, over the `/api` query interface.
//!
//! ## Auth is a query parameter, and failure comes back as `200 OK`
//!
//! Every call carries `?apikey=<key>&output=json`. A wrong or missing
//! key does not produce a `401`: SABnzbd answers `200` with
//! `{"status": false, "error": "API Key Incorrect"}`, so the body has to
//! be inspected before the connection can be called healthy.
//!
//! ## Where the version comes from
//!
//! `mode=queue` both proves the key works and carries `version` inside
//! its payload, so the common path is a single round-trip. Older builds
//! omit that field; those fall back to `mode=version`, which is one of
//! the few modes SABnzbd serves without a key.

use std::time::Duration;

use reqwest::Client as HttpClient;
use serde::Deserialize;
use tracing::debug;
use url::Url;

use crate::error::truncate_body;
use crate::{
    ClientFuture, ClientStatus, DownloadClient, DownloadClientConfig, DownloadClientError,
    DownloadClientKind, endpoint,
};

/// The kind every error in this module carries.
const KIND: DownloadClientKind = DownloadClientKind::Sabnzbd;

/// HTTP client for one SABnzbd instance.
#[derive(Debug)]
pub struct SabnzbdClient {
    config: DownloadClientConfig,
    api_key: String,
    http: HttpClient,
}

/// Envelope shared by every `output=json` response. Each mode fills a
/// different field, and a refusal fills [`Self::error`] instead — all
/// three shapes arrive with status `200`.
#[derive(Debug, Deserialize)]
struct SabEnvelope {
    /// Present and `false` on a refusal.
    #[serde(default)]
    status: Option<bool>,
    /// Refusal reason, verbatim (`"API Key Incorrect"`).
    #[serde(default)]
    error: Option<String>,
    /// Payload of `mode=queue`.
    #[serde(default)]
    queue: Option<SabQueue>,
    /// Payload of `mode=version`.
    #[serde(default)]
    version: Option<String>,
}

/// The slice of the queue payload this crate reads. SABnzbd returns
/// ~40 fields; the rest are for the queue view, which is a later step.
#[derive(Debug, Deserialize)]
struct SabQueue {
    /// Present on current builds, absent on older ones.
    #[serde(default)]
    version: Option<String>,
}

impl SabnzbdClient {
    /// Build a client over `config`.
    ///
    /// # Errors
    ///
    /// - [`DownloadClientError::Config`] when no apikey is configured —
    ///   SABnzbd has no other authentication mechanism, so this can be
    ///   caught before any network call.
    /// - [`DownloadClientError::Transport`] if `reqwest` cannot
    ///   instantiate its TLS backend (system-level, rare).
    pub fn new(config: DownloadClientConfig) -> Result<Self, DownloadClientError> {
        let api_key = config
            .api_key
            .as_deref()
            .map(str::trim)
            .filter(|k| !k.is_empty())
            .ok_or_else(|| DownloadClientError::Config {
                kind: KIND,
                detail: "SABnzbd exige uma apikey (Config → General → API Key)".to_owned(),
            })?
            .to_owned();
        let http = HttpClient::builder()
            .user_agent(concat!("brarr/", env!("CARGO_PKG_VERSION")))
            .timeout(Duration::from_secs(15))
            .build()
            .map_err(|source| DownloadClientError::Transport { kind: KIND, source })?;
        Ok(Self {
            config,
            api_key,
            http,
        })
    }

    /// The configuration this client was built for.
    #[must_use]
    pub fn config(&self) -> &DownloadClientConfig {
        &self.config
    }

    /// Build `…/api?output=json&apikey=…` plus the caller's parameters.
    fn api_url(&self, params: &[(&str, &str)]) -> Result<Url, DownloadClientError> {
        let mut url = endpoint(&self.config.base_url, "api")?;
        {
            let mut query = url.query_pairs_mut();
            query.append_pair("output", "json");
            query.append_pair("apikey", &self.api_key);
            for (key, value) in params {
                query.append_pair(key, value);
            }
        }
        Ok(url)
    }

    /// One API call, decoded into the shared envelope. Refusals are
    /// converted here so callers never see a `status: false` body.
    async fn call(&self, params: &[(&str, &str)]) -> Result<SabEnvelope, DownloadClientError> {
        let url = self.api_url(params)?;
        debug!(
            target: "brarr_download_client",
            name = %self.config.name,
            // Deliberately not the full URL: the apikey lives in the query.
            path = url.path(),
            mode = params.first().map(|(_, v)| *v).unwrap_or_default(),
            "sabnzbd call"
        );
        let resp = self
            .http
            .get(url)
            .send()
            .await
            .map_err(|source| DownloadClientError::Transport { kind: KIND, source })?;
        let status = resp.status();
        let body = resp
            .text()
            .await
            .map_err(|source| DownloadClientError::Transport { kind: KIND, source })?;
        if !status.is_success() {
            return Err(DownloadClientError::Http {
                kind: KIND,
                status: status.as_u16(),
                body: truncate_body(&body),
            });
        }
        let envelope: SabEnvelope = serde_json::from_str(&body)
            .map_err(|source| DownloadClientError::Decode { kind: KIND, source })?;
        if let Some(error) = envelope.error.as_deref() {
            return Err(classify_error(error, status.as_u16()));
        }
        if envelope.status == Some(false) {
            return Err(DownloadClientError::Auth {
                kind: KIND,
                detail: "SABnzbd recusou a chamada sem informar o motivo".to_owned(),
            });
        }
        Ok(envelope)
    }
}

impl DownloadClient for SabnzbdClient {
    fn name(&self) -> &str {
        &self.config.name
    }

    fn kind(&self) -> DownloadClientKind {
        KIND
    }

    fn test_connection(&self) -> ClientFuture<'_, Result<ClientStatus, DownloadClientError>> {
        Box::pin(async move {
            // `queue` is the auth gate: `version` alone answers without a
            // key, so a green badge from it would prove nothing.
            let queue = self
                .call(&[("mode", "queue"), ("start", "0"), ("limit", "1")])
                .await?;
            if let Some(version) = queue.queue.and_then(|q| q.version) {
                return Ok(ClientStatus { version });
            }
            // Older builds leave it out of the queue payload.
            let fallback = self.call(&[("mode", "version")]).await?;
            Ok(ClientStatus {
                version: fallback.version.unwrap_or_default(),
            })
        })
    }
}

/// Map SABnzbd's error string onto an error variant.
///
/// The documented refusals are `API Key Incorrect`, `API Key Required`
/// and `Access denied`. Anything else — an unknown mode, a disk problem
/// — is a real server-side error and must not be reported to the
/// operator as a credentials problem.
fn classify_error(error: &str, status: u16) -> DownloadClientError {
    let lowered = error.to_ascii_lowercase();
    if lowered.contains("api key") || lowered.contains("access denied") {
        DownloadClientError::Auth {
            kind: KIND,
            detail: truncate_body(error),
        }
    } else {
        DownloadClientError::Http {
            kind: KIND,
            status,
            body: truncate_body(error),
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, reason = "tests assert on happy paths")]

    use super::*;

    fn config(api_key: Option<&str>) -> DownloadClientConfig {
        DownloadClientConfig {
            name: "sab".to_owned(),
            kind: KIND,
            base_url: Url::parse("http://10.0.1.246:8085/").unwrap(),
            username: None,
            password: None,
            api_key: api_key.map(str::to_owned),
            category: None,
        }
    }

    #[test]
    fn a_missing_api_key_fails_before_any_network_call() {
        let err = SabnzbdClient::new(config(None)).unwrap_err();
        assert!(matches!(err, DownloadClientError::Config { .. }));
        let err = SabnzbdClient::new(config(Some("   "))).unwrap_err();
        assert!(
            matches!(err, DownloadClientError::Config { .. }),
            "whitespace is not a key"
        );
    }

    #[test]
    fn the_api_key_is_url_encoded_into_the_query() {
        let client = SabnzbdClient::new(config(Some("a b&c"))).unwrap();
        let url = client.api_url(&[("mode", "queue")]).unwrap();
        assert!(url.as_str().contains("apikey=a+b%26c"), "got {url}");
        assert!(url.as_str().contains("output=json"));
        assert!(url.as_str().ends_with("mode=queue"));
    }

    #[test]
    fn a_reverse_proxy_prefix_survives() {
        let mut cfg = config(Some("k"));
        cfg.base_url = Url::parse("https://home.example/sabnzbd").unwrap();
        let client = SabnzbdClient::new(cfg).unwrap();
        let url = client.api_url(&[]).unwrap();
        assert_eq!(url.path(), "/sabnzbd/api");
    }

    #[test]
    fn only_credential_errors_are_reported_as_auth_failures() {
        assert!(matches!(
            classify_error("API Key Incorrect", 200),
            DownloadClientError::Auth { .. }
        ));
        assert!(matches!(
            classify_error("Access denied", 200),
            DownloadClientError::Auth { .. }
        ));
        assert!(
            matches!(
                classify_error("Missing intake directory", 200),
                DownloadClientError::Http { .. }
            ),
            "a server-side problem must not read as a wrong password"
        );
    }
}
