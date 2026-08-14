//! The HTTP client.

use std::sync::Arc;
use std::time::Duration;

use reqwest::StatusCode;
use tokio::sync::Mutex;
use tracing::debug;
use url::Url;

use crate::dto;
use crate::error::TvdbError;
use crate::model::{Episode, SeasonType, SeriesEpisodes};
use crate::retry::{RetryConfig, run_with_retry};

/// Default API root.
pub const DEFAULT_BASE_URL: &str = "https://api4.thetvdb.com/v4/";

/// Ceiling on one request, matching `brarr-tmdb`.
///
/// The builder used to configure nothing but a `user_agent`, so a socket
/// that accepted and never answered held the call for as long as the
/// process lived. Survivable while this crate was a background sweep on
/// a 12-hour cadence; not once it owns the episode tree and sits in the
/// path of `/library/add`, where the wait is a person.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(15);

/// Ceiling on pages walked in one call.
///
/// A `links.next` that points at itself would otherwise loop forever
/// against someone else's API. 200 pages at the documented page size is
/// far past any real series — Yu-Gi-Oh! is 224 episodes, one page.
const MAX_PAGES: usize = 200;

/// The shared HTTP client shape, so the constructor and
/// [`TvdbClient::with_timeout`] cannot drift apart.
fn build_http(timeout: Duration) -> Result<reqwest::Client, TvdbError> {
    reqwest::Client::builder()
        .user_agent(concat!("brarr/", env!("CARGO_PKG_VERSION")))
        .timeout(timeout)
        .build()
        .map_err(TvdbError::ClientBuild)
}

/// Credentials for one project key.
#[derive(Debug, Clone)]
pub struct TvdbAuth {
    /// The project's v4 API key.
    pub api_key: String,
    /// The end user's subscriber PIN.
    ///
    /// **Only for a user-supported key, and it must be absent
    /// otherwise** — the API documentation is explicit: "If you have a
    /// user-supported key, also provide your subscriber PIN as `pin`.
    /// Otherwise completely remove `pin` from your call." brarr's key is
    /// funded by the revenue tier, so this is `None` in practice and
    /// exists so a user-supported key is not a code change.
    pub pin: Option<String>,
}

/// Async client for the TVDB v4 API.
///
/// One login per month rather than one per call: the token is valid for
/// a month and is cached behind a `Mutex` so a burst of series performs
/// one login, not one each — the same shape `brarr-download-client`
/// gives qBittorrent's `SID`.
#[derive(Debug, Clone)]
pub struct TvdbClient {
    http: reqwest::Client,
    base: Url,
    auth: TvdbAuth,
    token: Arc<Mutex<Option<String>>>,
    retry: RetryConfig,
}

impl TvdbClient {
    /// Build a client against the default API root.
    ///
    /// # Errors
    ///
    /// [`TvdbError::ClientBuild`] when the TLS backend cannot be built,
    /// [`TvdbError::BadUrl`] when the base URL will not parse.
    pub fn new(auth: TvdbAuth) -> Result<Self, TvdbError> {
        Self::with_base_url(auth, DEFAULT_BASE_URL)
    }

    /// Build a client against an explicit API root, for tests.
    ///
    /// # Errors
    ///
    /// As [`Self::new`].
    pub fn with_base_url(auth: TvdbAuth, base: &str) -> Result<Self, TvdbError> {
        // A base without a trailing slash makes `Url::join` drop the last
        // segment, which turns `/v4/` into `/` and every call into a 404.
        let normalised = if base.ends_with('/') {
            base.to_owned()
        } else {
            format!("{base}/")
        };
        Ok(Self {
            http: build_http(DEFAULT_TIMEOUT)?,
            base: Url::parse(&normalised)?,
            auth,
            token: Arc::new(Mutex::new(None)),
            retry: RetryConfig::default(),
        })
    }

    /// Replace the retry policy.
    #[must_use]
    pub const fn with_retry(mut self, retry: RetryConfig) -> Self {
        self.retry = retry;
        self
    }

    /// Replace the per-request timeout.
    #[must_use]
    pub fn with_timeout(self, timeout: Duration) -> Self {
        // A failed rebuild keeps the client it already has rather than
        // returning `Result` from a builder method: the caller asked for
        // a shorter ceiling, and refusing the whole client over it would
        // trade a working default for nothing.
        match build_http(timeout) {
            Ok(http) => Self { http, ..self },
            Err(e) => {
                debug!(target: "brarr_tvdb", error = %e, "keeping the default timeout");
                self
            }
        }
    }

    /// Prove the credentials work, returning nothing useful on success.
    ///
    /// What `/settings` calls to tell the operator their key is good
    /// before a background sweep discovers otherwise.
    ///
    /// # Errors
    ///
    /// [`TvdbError::Unauthorized`] when the key (or the missing PIN) is
    /// refused; transport errors otherwise.
    pub async fn verify(&self) -> Result<(), TvdbError> {
        run_with_retry(self.retry, "verify", || self.login())
            .await
            .map(|_| ())
    }

    /// A series' episodes under one season type.
    ///
    /// Walks every page. `season` narrows the request to one season of
    /// the requested type, which is what a per-season refresh wants.
    ///
    /// # Errors
    ///
    /// [`TvdbError::NotFound`] for an unknown series id, and the
    /// transport and decoding errors of [`TvdbError`].
    pub async fn series_episodes(
        &self,
        series_id: i64,
        season_type: SeasonType,
        season: Option<i32>,
    ) -> Result<SeriesEpisodes, TvdbError> {
        let path = format!("series/{series_id}/episodes/{}", season_type.as_str());
        let mut out = SeriesEpisodes::default();
        let mut seen: std::collections::HashSet<i64> = std::collections::HashSet::new();

        for page in 0..MAX_PAGES {
            let mut url = self.base.join(&path)?;
            {
                let mut q = url.query_pairs_mut();
                q.append_pair("page", &page.to_string());
                if let Some(season) = season {
                    q.append_pair("season", &season.to_string());
                }
            }
            let envelope: dto::Envelope<dto::SeriesEpisodesDto> = self
                .get(url, &format!("series {series_id} episodes"))
                .await?;
            let Some(data) = envelope.data else { break };

            if out.series_id.is_none() {
                out.series_id = data.series.as_ref().and_then(|s| s.id);
                out.series_name = data.series.as_ref().and_then(|s| s.name.clone());
            }
            // Deduplicated by TheTVDB's episode id, which serves twice
            // over: a paginated API that repeats a record across a page
            // boundary does not double it, and a `links.next` pointing
            // at itself contributes nothing and ends the walk below.
            let before = out.episodes.len();
            for episode in data.episodes.iter().filter_map(Episode::from_dto) {
                if seen.insert(episode.id) {
                    out.episodes.push(episode);
                }
            }

            // Terminate on the cursor, but also on a page that added
            // nothing new: a `next` that repeats itself would otherwise
            // spin until MAX_PAGES doing real requests each time.
            let links = envelope.links.unwrap_or_default();
            let reported = links.total_items;
            if links.next.is_none() || out.episodes.len() == before {
                debug!(
                    target: "brarr_tvdb",
                    series_id, season_type = season_type.as_str(),
                    pages = page + 1, episodes = out.episodes.len(),
                    // Logged rather than trusted: a count that disagrees
                    // with what was collected is how a silently truncated
                    // walk gets noticed, and refusing on it would fail a
                    // series over a stale counter upstream.
                    reported,
                    "walked a series' episodes"
                );
                return Ok(out);
            }
        }
        Err(TvdbError::RunawayPagination(MAX_PAGES))
    }

    /// The `TheTVDB` series behind an external id — an `IMDb` `ttNNNNNNN`,
    /// or a TMDB id.
    ///
    /// # Errors
    ///
    /// As [`Self::series_episodes`].
    pub async fn series_by_remote_id(&self, remote_id: &str) -> Result<Option<i64>, TvdbError> {
        let url = self.base.join(&format!("search/remoteid/{remote_id}"))?;
        let envelope: dto::Envelope<Vec<dto::RemoteIdMatchDto>> =
            self.get(url, &format!("remote id {remote_id}")).await?;
        Ok(envelope
            .data
            .unwrap_or_default()
            .iter()
            .find_map(|m| m.series.as_ref().and_then(|s| s.id)))
    }

    /// One authenticated GET, with a single re-login on a rejected token.
    ///
    /// A month-long token outlives most processes but not all of them,
    /// and a key revoked upstream looks the same. One retry distinguishes
    /// "the token aged out" from "the key is gone" without a loop.
    /// Retried **per page**, not per walk: a failure on page three of a
    /// 224-episode series must not restart the walk, which would both
    /// re-fetch what already succeeded and re-enter the dedup set.
    async fn get<T: serde::de::DeserializeOwned>(
        &self,
        url: Url,
        what: &str,
    ) -> Result<T, TvdbError> {
        run_with_retry(self.retry, "get", || self.get_once(url.clone(), what)).await
    }

    async fn get_once<T: serde::de::DeserializeOwned>(
        &self,
        url: Url,
        what: &str,
    ) -> Result<T, TvdbError> {
        let token = self.login().await?;
        let response = self.send(url.clone(), &token).await?;
        let response = if response.status() == StatusCode::UNAUTHORIZED {
            self.token.lock().await.take();
            let fresh = self.login().await?;
            self.send(url, &fresh).await?
        } else {
            response
        };

        match response.status() {
            StatusCode::NOT_FOUND => return Err(TvdbError::NotFound(what.to_owned())),
            StatusCode::TOO_MANY_REQUESTS => return Err(TvdbError::RateLimited),
            StatusCode::UNAUTHORIZED => return Err(TvdbError::TokenRejected),
            _ => {}
        }
        let body = response.error_for_status()?.text().await?;
        serde_json::from_str(&body).map_err(TvdbError::BadJson)
    }

    async fn send(&self, url: Url, token: &str) -> Result<reqwest::Response, reqwest::Error> {
        self.http.get(url).bearer_auth(token).send().await
    }

    /// The cached bearer token, logging in if there is none.
    async fn login(&self) -> Result<String, TvdbError> {
        let mut slot = self.token.lock().await;
        if let Some(token) = slot.as_ref() {
            return Ok(token.clone());
        }

        // The PIN is omitted rather than sent empty: the documentation
        // says to remove it entirely for a project key, and an empty
        // string is not the same as absent to this API.
        let mut payload = serde_json::json!({ "apikey": self.auth.api_key });
        if let Some(pin) = self.auth.pin.as_deref().filter(|p| !p.trim().is_empty()) {
            payload["pin"] = serde_json::Value::String(pin.to_owned());
        }

        let response = self
            .http
            .post(self.base.join("login")?)
            .json(&payload)
            .send()
            .await?;
        if matches!(
            response.status(),
            StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN
        ) {
            return Err(TvdbError::Unauthorized);
        }
        let body = response.error_for_status()?.text().await?;
        let envelope: dto::Envelope<dto::TokenDto> =
            serde_json::from_str(&body).map_err(TvdbError::BadJson)?;
        let token = envelope
            .data
            .and_then(|d| d.token)
            .filter(|t| !t.trim().is_empty())
            .ok_or(TvdbError::Unauthorized)?;

        *slot = Some(token.clone());
        Ok(token)
    }
}
