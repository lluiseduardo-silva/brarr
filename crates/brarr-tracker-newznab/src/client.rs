//! Async HTTP client for a single Newznab/Torznab indexer.

use std::time::Duration;

use brarr_core::{
    ImdbId, ProviderError, ProviderFuture, Release, TmdbId, TrackerProvider, TrackerSource,
};
use reqwest::Client;
use tracing::{debug, info, warn};
use url::Url;

use crate::convert::item_to_release;
use crate::dto::parse_feed;
use crate::error::ClientError;

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);

/// HTTP client for a single Newznab indexer.
///
/// `Clone` is cheap — the inner `reqwest::Client` is `Arc`-shared.
#[derive(Debug, Clone)]
pub struct NewznabClient {
    http: Client,
    base_url: Url,
    tracker: TrackerSource,
    apikey: String,
}

impl NewznabClient {
    /// Build a new client.
    ///
    /// `tracker.base_url` should be the indexer's API root (e.g.
    /// `https://api.nzbgeek.info/`); the client appends `api` when
    /// composing request URLs.
    ///
    /// # Errors
    ///
    /// - [`ClientError::InvalidApiKey`] if `apikey` contains characters
    ///   that can't appear in a URL query parameter unencoded.
    /// - [`ClientError::ClientBuild`] when `reqwest::Client::builder()`
    ///   fails.
    pub fn new(tracker: TrackerSource, apikey: &str) -> Result<Self, ClientError> {
        if apikey.is_empty()
            || !apikey
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
        {
            return Err(ClientError::InvalidApiKey);
        }
        let http = Client::builder()
            .timeout(DEFAULT_TIMEOUT)
            .build()
            .map_err(ClientError::ClientBuild)?;
        Ok(Self {
            http,
            base_url: tracker.base_url.clone(),
            tracker,
            apikey: apikey.to_string(),
        })
    }

    /// Tracker source associated with this client.
    #[must_use]
    pub const fn tracker_source(&self) -> &TrackerSource {
        &self.tracker
    }

    /// `GET /api?t=movie&imdbid=<id>&apikey=<key>`. NZBGeek and other
    /// Newznab servers want the IMDb id with the leading `tt` stripped;
    /// this function does that.
    ///
    /// # Errors
    ///
    /// See [`ClientError`].
    pub async fn search_movie_by_imdb(&self, imdb: ImdbId) -> Result<Vec<Release>, ClientError> {
        let url = self.build_url("movie", &[("imdbid", &format!("{:07}", imdb.get()))])?;
        self.fetch_and_parse(url).await
    }

    /// `GET /api?t=movie&tmdbid=<id>&apikey=<key>` — non-standard but a
    /// handful of Newznab servers honour it. NZBGeek does **not**.
    /// Provided so we can try TMDb opportunistically.
    ///
    /// # Errors
    ///
    /// See [`ClientError`].
    pub async fn search_movie_by_tmdb(&self, tmdb: TmdbId) -> Result<Vec<Release>, ClientError> {
        let url = self.build_url("movie", &[("tmdbid", &tmdb.get().to_string())])?;
        self.fetch_and_parse(url).await
    }

    fn build_url(&self, t: &str, extra: &[(&str, &str)]) -> Result<Url, ClientError> {
        let mut url = self.base_url.join("api")?;
        {
            let mut qs = url.query_pairs_mut();
            qs.append_pair("t", t);
            qs.append_pair("apikey", &self.apikey);
            for (k, v) in extra {
                qs.append_pair(k, v);
            }
        }
        Ok(url)
    }

    async fn fetch_and_parse(&self, url: Url) -> Result<Vec<Release>, ClientError> {
        debug!(
            target: "brarr_tracker_newznab",
            tracker = %self.tracker.name,
            url = %url,
            "newznab request"
        );
        let resp = self.http.get(url).send().await?.error_for_status()?;
        let body = resp.text().await?;
        let feed = parse_feed(&body)?;
        info!(
            target: "brarr_tracker_newznab",
            tracker = %self.tracker.name,
            items = feed.items.len(),
            "newznab response"
        );
        let mut releases = Vec::with_capacity(feed.items.len());
        for item in &feed.items {
            match item_to_release(item, self.tracker.clone()) {
                Ok(r) => releases.push(r),
                Err(e) => {
                    warn!(
                        target: "brarr_tracker_newznab",
                        tracker = %self.tracker.name,
                        title = %item.title,
                        error = %e,
                        "skipping malformed newznab item"
                    );
                }
            }
        }
        Ok(releases)
    }
}

impl TrackerProvider for NewznabClient {
    fn name(&self) -> &str {
        &self.tracker.name
    }

    fn search_by_tmdb(
        &self,
        tmdb: TmdbId,
    ) -> ProviderFuture<'_, Result<Vec<Release>, ProviderError>> {
        let name = self.tracker.name.clone();
        Box::pin(async move {
            // Newznab's standard movie-search axis is IMDb. We still
            // accept TMDb here so callers don't have to special-case
            // this provider — but the path is best-effort. NZBGeek
            // returns an empty feed for unknown axes.
            self.search_movie_by_tmdb(tmdb)
                .await
                .map_err(|e| ProviderError::new(name, e.to_string()))
        })
    }

    fn search_by_imdb(
        &self,
        imdb: ImdbId,
    ) -> ProviderFuture<'_, Result<Vec<Release>, ProviderError>> {
        let name = self.tracker.name.clone();
        Box::pin(async move {
            self.search_movie_by_imdb(imdb)
                .await
                .map_err(|e| ProviderError::new(name, e.to_string()))
        })
    }
}
