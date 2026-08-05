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
    AddedRelease, ClientFuture, ClientStatus, DownloadClient, DownloadClientConfig,
    DownloadClientError, DownloadClientKind, DownloadState, DownloadStatus, ReleaseFile, endpoint,
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
    /// Payload of `mode=addfile` — one entry per accepted nzb.
    #[serde(default)]
    nzo_ids: Option<Vec<String>>,
    /// Payload of `mode=history`.
    #[serde(default)]
    history: Option<SabHistory>,
}

/// The slice of the queue payload this crate reads.
#[derive(Debug, Deserialize)]
struct SabQueue {
    /// Present on current builds, absent on older ones.
    #[serde(default)]
    version: Option<String>,
    /// One entry per job still downloading.
    #[serde(default)]
    slots: Vec<SabQueueSlot>,
}

/// A job still in the queue.
///
/// Every numeric field is typed `String`: SABnzbd quotes its numbers
/// (`"percentage": "42"`, `"mb": "4096.5"`) and has changed which ones
/// over the years, so parsing them here beats a deserialise that fails
/// the whole payload over one field's type.
#[derive(Debug, Deserialize)]
struct SabQueueSlot {
    #[serde(default)]
    nzo_id: String,
    /// `Downloading` | `Queued` | `Paused` | `Propagating` | `Fetching`.
    #[serde(default)]
    status: String,
    /// Whole percent, `"0"`..`"100"`.
    #[serde(default)]
    percentage: String,
    /// Total size in megabytes.
    #[serde(default)]
    mb: String,
    /// `h:mm:ss` remaining.
    #[serde(default)]
    timeleft: String,
}

/// Payload of `mode=history`.
#[derive(Debug, Deserialize)]
struct SabHistory {
    #[serde(default)]
    slots: Vec<SabHistorySlot>,
}

/// A job that left the queue — finished, failed, or post-processing.
#[derive(Debug, Deserialize)]
struct SabHistorySlot {
    #[serde(default)]
    nzo_id: String,
    /// `Completed` | `Failed` | `Extracting` | `Repairing` | …
    #[serde(default)]
    status: String,
    /// Final directory. What the import phase will want.
    #[serde(default)]
    storage: String,
    /// Populated when `status == "Failed"`.
    #[serde(default)]
    fail_message: String,
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
        self.send(self.http.get(url), params).await
    }

    /// Shared tail of every call: run the request, reject a non-2xx, then
    /// look inside the body for the refusals SABnzbd delivers with a
    /// `200`.
    async fn send(
        &self,
        req: reqwest::RequestBuilder,
        params: &[(&str, &str)],
    ) -> Result<SabEnvelope, DownloadClientError> {
        debug!(
            target: "brarr_download_client",
            name = %self.config.name,
            // Deliberately not the full URL: the apikey lives in the query.
            mode = params.first().map(|(_, v)| *v).unwrap_or_default(),
            "sabnzbd call"
        );
        let resp = req
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

    fn add<'a>(
        &'a self,
        name: &'a str,
        file: ReleaseFile<'a>,
    ) -> ClientFuture<'a, Result<AddedRelease, DownloadClientError>> {
        Box::pin(async move {
            let ReleaseFile::Bytes(bytes) = file else {
                // Unreachable through the scanner — a magnet only ever
                // comes off a torrent provider, and those route to the
                // torrent client. Worth an explicit error rather than a
                // confusing upload failure if a caller gets it wrong.
                return Err(DownloadClientError::Config {
                    kind: KIND,
                    detail: "SABnzbd é usenet e não aceita magnet".to_owned(),
                });
            };
            let mut params: Vec<(&str, &str)> = vec![("mode", "addfile"), ("nzbname", name)];
            if let Some(category) = self
                .config
                .category
                .as_deref()
                .map(str::trim)
                .filter(|c| !c.is_empty())
            {
                params.push(("cat", category));
            }
            let url = self.api_url(&params)?;
            // The upload field is literally called `name`; SABnzbd reads
            // the nzb out of it and takes the display name from the
            // `nzbname` query parameter above.
            let part = reqwest::multipart::Part::bytes(bytes.to_vec())
                .file_name(format!("{name}.nzb"))
                .mime_str("application/x-nzb")
                .unwrap_or_else(|_| {
                    reqwest::multipart::Part::bytes(bytes.to_vec()).file_name(format!("{name}.nzb"))
                });
            let form = reqwest::multipart::Form::new().part("name", part);
            let envelope = self
                .send(self.http.post(url).multipart(form), &params)
                .await?;
            // An accepted upload always names what it created; an empty
            // list means SABnzbd took the request and queued nothing.
            let client_item_id = envelope
                .nzo_ids
                .unwrap_or_default()
                .into_iter()
                .next()
                .filter(|id| !id.is_empty());
            if client_item_id.is_none() {
                return Err(DownloadClientError::Http {
                    kind: KIND,
                    status: 200,
                    body: "SABnzbd aceitou a chamada mas não enfileirou nada (nzo_ids vazio)"
                        .to_owned(),
                });
            }
            Ok(AddedRelease { client_item_id })
        })
    }

    fn status<'a>(
        &'a self,
        client_item_id: &'a str,
    ) -> ClientFuture<'a, Result<Option<DownloadStatus>, DownloadClientError>> {
        Box::pin(async move {
            // A job lives in the queue until it finishes, then moves to
            // the history. Two places, so two lookups — queue first,
            // because that is where anything still running is.
            let queue = self.call(&[("mode", "queue")]).await?;
            if let Some(slot) = queue
                .queue
                .into_iter()
                .flat_map(|q| q.slots)
                .find(|s| s.nzo_id == client_item_id)
            {
                return Ok(Some(slot.into_status()));
            }
            let history = self
                .call(&[("mode", "history"), ("nzo_ids", client_item_id)])
                .await?;
            Ok(history
                .history
                .into_iter()
                .flat_map(|h| h.slots)
                // The `nzo_ids` filter does the work, but old builds
                // ignore unknown parameters and return everything.
                .find(|s| s.nzo_id == client_item_id)
                .map(SabHistorySlot::into_status))
        })
    }
}

impl SabQueueSlot {
    fn into_status(self) -> DownloadStatus {
        let percent = self.percentage.trim().parse::<f32>().unwrap_or(0.0);
        let size_bytes = megabytes_to_bytes(&self.mb);
        DownloadStatus {
            state: match self.status.as_str() {
                "Paused" | "Queued" | "Propagating" => DownloadState::Queued,
                // Downloading, Fetching, and anything newer.
                _ => DownloadState::Downloading,
            },
            progress: (percent / 100.0).clamp(0.0, 1.0),
            size_bytes,
            // SABnzbd reports speed for the queue as a whole, never per
            // job. Attributing the total to one row would be a lie.
            speed_bytes: None,
            eta_seconds: parse_timeleft(&self.timeleft),
            save_path: None,
            detail: None,
        }
    }
}

impl SabHistorySlot {
    fn into_status(self) -> DownloadStatus {
        // Everything in the history has finished downloading; the
        // non-terminal statuses are post-processing (repair, extract,
        // move), which is still work in progress as far as brarr cares.
        let state = match self.status.as_str() {
            "Failed" => DownloadState::Failed,
            "Completed" => DownloadState::Completed,
            _ => DownloadState::Downloading,
        };
        DownloadStatus {
            state,
            progress: 1.0,
            size_bytes: None,
            speed_bytes: None,
            eta_seconds: None,
            save_path: Some(self.storage).filter(|p| !p.is_empty()),
            detail: Some(self.fail_message).filter(|m| !m.is_empty()),
        }
    }
}

/// SABnzbd reports sizes in megabytes as a quoted decimal (`"4096.5"`).
///
/// Only the whole megabytes are kept: this is a figure the UI renders as
/// "4.0 GB", and integer arithmetic avoids a float round-trip whose
/// failure modes (NaN, saturation) would have to be handled anyway.
fn megabytes_to_bytes(raw: &str) -> Option<u64> {
    let whole = raw.trim().split('.').next()?.trim();
    if whole.is_empty() {
        return None;
    }
    whole.parse::<u64>().ok()?.checked_mul(1024 * 1024)
}

/// Parse SABnzbd's `h:mm:ss` (or `mm:ss`) remaining time into seconds.
fn parse_timeleft(raw: &str) -> Option<u64> {
    let raw = raw.trim();
    if raw.is_empty() || raw == "0:00:00" {
        return None;
    }
    let mut seconds: u64 = 0;
    for part in raw.split(':') {
        seconds = seconds.checked_mul(60)?.checked_add(part.parse().ok()?)?;
    }
    Some(seconds)
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
