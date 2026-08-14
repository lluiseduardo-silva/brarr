//! The HTTP client.

use std::sync::Arc;
use std::time::Duration;

use reqwest::StatusCode;
use tokio::sync::Mutex;
use tracing::debug;
use url::Url;

use crate::dto;
use crate::error::TvdbError;
use crate::model::{
    Episode, SeasonType, SeriesDescription, SeriesEpisodes, SeriesTranslation, non_blank,
    parse_date,
};
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
    /// Languages to try for episode names, in order, before falling back
    /// to the original. Empty means "ask for no language", which is what
    /// this client did before [`Self::with_languages`] existed and what
    /// made every Frieren episode read `冒険の終わり`.
    languages: Vec<String>,
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
            languages: Vec::new(),
        })
    }

    /// The languages episode names are asked for, in order.
    ///
    /// The original is always the last resort and is never listed here —
    /// it is what the untranslated request returns, and it is a fallback
    /// rather than a preference. See [`Self::series_episodes_in`].
    #[must_use]
    pub fn with_languages<I, S>(mut self, languages: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.languages = languages.into_iter().map(Into::into).collect();
        self
    }

    /// The configured chain, for the impl that dispatches on it.
    #[must_use]
    pub fn languages(&self) -> Vec<&str> {
        self.languages.iter().map(String::as_str).collect()
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
        self.walk_episodes(series_id, season_type, season, None)
            .await
    }

    /// A series' episodes, named in the first language that has a name
    /// for each one.
    ///
    /// **The fallback is per episode, and it has to be.** Measured live
    /// on 2026-08-14: Frieren has 0 of 66 episodes translated into
    /// Portuguese and 65 of 66 into English, while Doctor Who has 154 of
    /// 322 in Portuguese — so a series-level "does it have Portuguese?"
    /// would either leave half of Doctor Who in English or all of
    /// Frieren in Japanese. Without any of this brarr stored the raw
    /// `name`, which is the **original** language: `冒険の終わり`.
    ///
    /// The join key is [`Episode::id`], stable across season types and
    /// therefore across languages — the same property that lets two
    /// orderings of one series be joined by identity.
    ///
    /// `languages` is tried in order and the untranslated request is
    /// always last, because the original is a fallback and never a
    /// preference. **A walk is skipped the moment no gap is left**: a
    /// fully translated series costs one request, not three, which over
    /// 180 series is the difference between a refresh and a rate limit.
    ///
    /// # Errors
    ///
    /// As [`Self::series_episodes`]. A language the series has no
    /// translation for is not an error — it answers with null names.
    pub async fn series_episodes_in(
        &self,
        series_id: i64,
        season_type: SeasonType,
        season: Option<i32>,
        languages: &[&str],
    ) -> Result<SeriesEpisodes, TvdbError> {
        let mut out: Option<SeriesEpisodes> = None;

        for language in languages.iter().map(Some).chain(std::iter::once(None)) {
            // Nothing left to name: stop before spending the request.
            if out
                .as_ref()
                .is_some_and(|found| found.episodes.iter().all(|e| e.name.is_some()))
            {
                break;
            }
            let page = self
                .walk_episodes(series_id, season_type, season, language.copied())
                .await?;
            match out.as_mut() {
                // The first walk establishes the set — coordinates, air
                // dates, ids. Later ones only supply names, so a
                // language that answers with a shorter list cannot drop
                // an episode from the tree.
                None => out = Some(page),
                Some(found) => {
                    let named: std::collections::HashMap<i64, String> = page
                        .episodes
                        .into_iter()
                        .filter_map(|e| e.name.map(|n| (e.id, n)))
                        .collect();
                    for episode in &mut found.episodes {
                        if episode.name.is_none()
                            && let Some(name) = named.get(&episode.id)
                        {
                            episode.name = Some(name.clone());
                        }
                    }
                }
            }
        }

        out.ok_or(TvdbError::RunawayPagination(0))
    }

    async fn walk_episodes(
        &self,
        series_id: i64,
        season_type: SeasonType,
        season: Option<i32>,
        language: Option<&str>,
    ) -> Result<SeriesEpisodes, TvdbError> {
        let path = match language {
            Some(lang) => format!(
                "series/{series_id}/episodes/{}/{lang}",
                season_type.as_str()
            ),
            None => format!("series/{series_id}/episodes/{}", season_type.as_str()),
        };
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

    /// A series' descriptive record, in the original language.
    ///
    /// The translated title and synopsis come from
    /// [`Self::series_translation`] and are layered on top by the caller
    /// — this is the base, and the only call that carries artwork,
    /// status and runtime.
    ///
    /// # Errors
    ///
    /// [`TvdbError::NotFound`] for an unknown series id, and the
    /// transport and decoding errors of [`TvdbError`].
    pub async fn series_extended(&self, series_id: i64) -> Result<SeriesDescription, TvdbError> {
        let url = self.base.join(&format!("series/{series_id}/extended"))?;
        let envelope: dto::Envelope<dto::SeriesExtendedDto> =
            self.get(url, &format!("series {series_id}")).await?;
        let data = envelope
            .data
            .ok_or_else(|| TvdbError::NotFound(format!("series {series_id}")))?;
        Ok(SeriesDescription {
            name: non_blank(data.name),
            overview: non_blank(data.overview),
            image: non_blank(data.image),
            year: non_blank(data.year).and_then(|y| y.parse::<i32>().ok()),
            runtime_minutes: data.average_runtime.and_then(|v| i32::try_from(v).ok()),
            original_language: non_blank(data.original_language),
            status: data.status.and_then(|s| non_blank(s.name)),
            next_aired: non_blank(data.next_aired).as_deref().and_then(parse_date),
        })
    }

    /// A series' title and synopsis in one language.
    ///
    /// `Ok(None)` when the series has no translation into it. **The API
    /// answers that with a 404**, not with null fields — the opposite of
    /// the episode endpoint — so absence is caught here rather than
    /// surfacing as an error the caller would have to re-classify.
    ///
    /// # Errors
    ///
    /// The transport and decoding errors of [`TvdbError`]. A 404 is not
    /// one of them.
    pub async fn series_translation(
        &self,
        series_id: i64,
        language: &str,
    ) -> Result<Option<SeriesTranslation>, TvdbError> {
        let url = self
            .base
            .join(&format!("series/{series_id}/translations/{language}"))?;
        let envelope: dto::Envelope<dto::TranslationDto> = match self
            .get(url, &format!("series {series_id} in {language}"))
            .await
        {
            Ok(envelope) => envelope,
            Err(TvdbError::NotFound(_)) => return Ok(None),
            Err(e) => return Err(e),
        };
        Ok(envelope.data.map(|d| SeriesTranslation {
            name: non_blank(d.name),
            overview: non_blank(d.overview),
        }))
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
