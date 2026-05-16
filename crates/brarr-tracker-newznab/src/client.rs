//! Async HTTP client for a single Newznab/Torznab indexer.

use std::time::Duration;

use brarr_core::{
    ImdbId, ProviderError, ProviderFuture, Release, TmdbId, TrackerProvider, TrackerSource,
};
use reqwest::Client;
use tracing::{info, warn};
use url::Url;

use crate::convert::item_to_release;
use crate::dto::parse_feed;
use crate::error::ClientError;

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);

/// Structured outcome of a connectivity probe. Mirrors the Unit3D
/// `PingReport` shape so the orchestrator can render either one through
/// the same template without owning the type.
#[derive(Debug, Clone)]
pub struct PingReport {
    /// `true` iff URL is reachable, status was 2xx, body did not look
    /// like a Newznab `<error>` payload, and a `<caps>` root element
    /// was detected.
    pub ok: bool,
    /// HTTP status. `0` if the request never reached the server.
    pub http_status: u16,
    /// Round-trip time in milliseconds.
    pub elapsed_ms: u32,
    /// Short human-readable detail.
    pub detail: String,
}

/// Newznab category ids for the "movies" tree. Comma-separated so it
/// passes through `query_pairs_mut` as a single `cat=` value the way
/// Sonarr/Radarr send it. Covers the SD/HD/UHD/BluRay subcats brarr's
/// outbound Torznab endpoint advertises.
const MOVIE_CATEGORIES: &str = "2000,2010,2020,2030,2040,2045,2050,2060";

/// Maximum number of body characters emitted in the WARN-level
/// diagnostic log when an indexer returns zero items. Truncated to
/// avoid spamming logs when an indexer responds with a huge HTML
/// error page.
const BODY_PREVIEW_BYTES: usize = 600;

/// Replace `apikey=...` in a URL's query string with `apikey=REDACTED`
/// so it's safe to log at INFO. Other params are preserved verbatim.
fn redact_apikey(url: &Url) -> String {
    let Some(q) = url.query() else {
        return url.to_string();
    };
    let pairs: Vec<String> = q
        .split('&')
        .map(|kv| {
            if let Some(rest) = kv.strip_prefix("apikey=") {
                // Keep first 4 chars of the apikey so the operator can
                // visually tell which key was used without leaking it.
                let prefix: String = rest.chars().take(4).collect();
                format!("apikey={prefix}…REDACTED")
            } else {
                kv.to_string()
            }
        })
        .collect();
    let mut out = url.to_string();
    if let Some(pos) = out.find('?') {
        out.truncate(pos + 1);
        out.push_str(&pairs.join("&"));
    }
    out
}

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

    /// Connectivity probe. Hits `?t=caps&apikey=X`. The Newznab
    /// capability endpoint requires a valid apikey on most
    /// implementations, so a 2xx response with a body containing
    /// `<caps>` (or `<error>`, depending on auth) is enough to tell
    /// URL + apikey health apart.
    ///
    /// # Errors
    ///
    /// Returns [`ClientError`] on transport failure.
    pub async fn ping(&self) -> Result<PingReport, ClientError> {
        let url = self.build_url("caps", &[])?;
        let started = std::time::Instant::now();
        let resp = self.http.get(url).send().await?;
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
                    body.chars()
                        .take(160)
                        .collect::<String>()
                        .replace('\n', " ")
                ),
            });
        }
        // Newznab error payloads come back with HTTP 200 but body
        // `<error code="100" description="..."/>`. Detect them.
        if body.contains("<error") {
            let snippet: String = body
                .chars()
                .take(160)
                .collect::<String>()
                .replace('\n', " ");
            return Ok(PingReport {
                ok: false,
                http_status: 200,
                elapsed_ms,
                detail: format!("provider returned error payload: {snippet}"),
            });
        }
        let ok = body.contains("<caps");
        Ok(PingReport {
            ok,
            http_status: 200,
            elapsed_ms,
            detail: if ok {
                format!("status 200, caps document OK ({} bytes)", body.len())
            } else {
                "status 200 but body did not contain <caps>".to_string()
            },
        })
    }

    /// `GET /api?t=movie&imdbid=<id>&cat=<movie-cats>&apikey=<key>`.
    /// NZBGeek and other Newznab servers want the IMDb id with the
    /// leading `tt` stripped, zero-padded to 7 digits.
    ///
    /// The `cat=` parameter narrows the search to the movie category
    /// tree (`2000-2060` per the Newznab category spec). Some indexers
    /// — NZBGeek among them — return zero items for an unfiltered
    /// `t=movie` query, so we always supply this filter. Callers that
    /// need anime/audio/etc. should add their own dedicated wrappers.
    ///
    /// # Errors
    ///
    /// See [`ClientError`].
    pub async fn search_movie_by_imdb(&self, imdb: ImdbId) -> Result<Vec<Release>, ClientError> {
        let url = self.build_url(
            "movie",
            &[
                ("imdbid", &format!("{:07}", imdb.get())),
                ("cat", MOVIE_CATEGORIES),
            ],
        )?;
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
        let url = self.build_url(
            "movie",
            &[
                ("tmdbid", &tmdb.get().to_string()),
                ("cat", MOVIE_CATEGORIES),
            ],
        )?;
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
        let redacted = redact_apikey(&url);
        info!(
            target: "brarr_tracker_newznab",
            tracker = %self.tracker.name,
            url = %redacted,
            "newznab request"
        );
        let resp = self.http.get(url).send().await?.error_for_status()?;
        let body = resp.text().await?;
        let feed = parse_feed(&body)?;
        info!(
            target: "brarr_tracker_newznab",
            tracker = %self.tracker.name,
            items = feed.items.len(),
            body_bytes = body.len(),
            "newznab response"
        );
        if feed.items.is_empty() {
            // Most "no hits" cases are legit (provider really doesn't
            // have the title), but a zero-items response is also what
            // you get for: wrong apikey, missing category param, wrong
            // host, exceeded daily quota, and a few other operator
            // errors. Log a body preview so the operator can tell
            // these apart without enabling DEBUG.
            let preview: String = body
                .chars()
                .take(BODY_PREVIEW_BYTES)
                .collect::<String>()
                .replace('\n', " ");
            warn!(
                target: "brarr_tracker_newznab",
                tracker = %self.tracker.name,
                preview = %preview,
                "newznab returned zero items — body preview follows for diagnosis"
            );
        }
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
