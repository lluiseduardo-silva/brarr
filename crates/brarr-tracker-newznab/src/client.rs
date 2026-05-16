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

/// User-Agent advertised on every outgoing request. NZBGeek and a few
/// other Newznab indexers reject requests carrying reqwest's default
/// `reqwest/X.Y.Z` UA with `<error code="109" description="Invalid User
/// Agent"/>`, treating it as an unsanctioned scraper. We send a stable
/// `brarr/<crate-version>` string so the indexer logs us as a known
/// client and operators can see traffic by UA in their dashboards.
const USER_AGENT: &str = concat!("brarr/", env!("CARGO_PKG_VERSION"));

/// Raw + parsed snapshot of a single search call, returned by the
/// `inspect_*` methods. Surfaced through the orchestrator's
/// `/providers/{id}/probe` admin route so operators can audit what
/// `<newznab:attr>` keys an indexer actually exposes and verify which
/// ones the scoring rules consume.
#[derive(Debug, Clone, serde::Serialize)]
pub struct InspectResult {
    /// The full upstream URL that was hit, with `apikey=` redacted.
    pub request_url: String,
    /// HTTP status code returned by the upstream.
    pub http_status: u16,
    /// Length of the response body in bytes.
    pub body_bytes: usize,
    /// Raw response body (XML), verbatim, so the operator can grep for
    /// fields the parser ignores today.
    pub raw_body: String,
    /// Per-`<item>` debug dump. Each entry preserves the full attr map
    /// (no projection or filtering) so missing scoring inputs are
    /// obvious.
    pub items: Vec<ItemDebug>,
    /// Releases as brarr would feed them to the decision engine,
    /// projected into a Serialize-friendly shadow. Cross-reference
    /// against `items` to see what the converter drops or normalizes.
    pub releases: Vec<ReleaseSnapshot>,
}

/// Serialize-friendly snapshot of a [`brarr_core::Release`]. Only the
/// fields that matter for diagnosing scoring/ranking decisions are
/// included; this is a debug surface, not a wire format.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ReleaseSnapshot {
    /// Full release title as the indexer reported it.
    pub title: String,
    /// Tracker-side opaque release id.
    pub tracker_release_id: String,
    /// Total size in bytes.
    pub size_bytes: u64,
    /// Seeders / leechers / completed grabs snapshot.
    pub seeders: u32,
    /// Leechers count.
    pub leechers: u32,
    /// Times the release was completed/snatched.
    pub snatches: u32,
    /// Normalized resolution label (`"1080p"`, `"2160p"`, etc.).
    pub resolution: String,
    /// Normalized release kind (`"WEB-DL"`, `"BluRay"`, etc.).
    pub kind: String,
    /// External media ids surfaced by the indexer.
    pub external_ids: ExternalIdsSnapshot,
    /// Enrichment (audio/sub language tags, HDR flag) when present.
    pub enrichment: Option<EnrichmentSnapshot>,
    /// `.torrent` / `.nzb` download URL, if exposed.
    pub download_url: Option<String>,
    /// Details page URL, if exposed.
    pub details_url: Option<String>,
}

/// Per-id external mapping snapshot.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ExternalIdsSnapshot {
    /// TMDb id.
    pub tmdb: Option<u32>,
    /// IMDb id (numeric, no `tt`).
    pub imdb: Option<u32>,
    /// TVDB id.
    pub tvdb: Option<u32>,
    /// MyAnimeList id.
    pub mal: Option<u32>,
}

/// Enrichment snapshot — what brarr would feed the rules engine for
/// audio/subtitle scoring.
#[derive(Debug, Clone, serde::Serialize)]
pub struct EnrichmentSnapshot {
    /// Audio track languages (e.g. `"en"`, `"pt-BR"`).
    pub audio_languages: Vec<String>,
    /// Subtitle languages.
    pub subtitle_languages: Vec<String>,
    /// `true` if the title or container hinted at HDR.
    pub has_hdr: bool,
    /// `true` if any subtitle track is flagged forced.
    pub has_forced_subs: bool,
}

impl ReleaseSnapshot {
    fn from_release(r: &Release) -> Self {
        let resolution = match &r.resolution {
            brarr_core::Resolution::Sd => "SD".to_string(),
            brarr_core::Resolution::P720 => "720p".to_string(),
            brarr_core::Resolution::P1080 => "1080p".to_string(),
            brarr_core::Resolution::P2160 => "2160p".to_string(),
            brarr_core::Resolution::Other(s) => s.clone(),
        };
        let kind = match &r.kind {
            brarr_core::ReleaseKind::WebDl => "WEB-DL".to_string(),
            brarr_core::ReleaseKind::BluRay => "BluRay".to_string(),
            brarr_core::ReleaseKind::Encode => "Encode".to_string(),
            brarr_core::ReleaseKind::HdTv => "HDTV".to_string(),
            brarr_core::ReleaseKind::Dvd => "DVD".to_string(),
            brarr_core::ReleaseKind::Other(s) => s.clone(),
        };
        let enrichment = r.enrichment.as_ref().map(|e| EnrichmentSnapshot {
            audio_languages: e.audio_languages.iter().map(|l| format!("{l:?}")).collect(),
            subtitle_languages: e
                .subtitle_languages
                .iter()
                .map(|l| format!("{l:?}"))
                .collect(),
            has_hdr: e.has_hdr,
            has_forced_subs: e.has_forced_subs,
        });
        Self {
            title: r.title.clone(),
            tracker_release_id: r.tracker_release_id.clone(),
            size_bytes: r.size_bytes,
            seeders: r.seeders,
            leechers: r.leechers,
            snatches: r.snatches,
            resolution,
            kind,
            external_ids: ExternalIdsSnapshot {
                tmdb: r.external_ids.tmdb.map(brarr_core::TmdbId::get),
                imdb: r.external_ids.imdb.map(brarr_core::ImdbId::get),
                tvdb: r.external_ids.tvdb.map(brarr_core::TvdbId::get),
                mal: r.external_ids.mal.map(brarr_core::MalId::get),
            },
            enrichment,
            download_url: r.urls.download.as_ref().map(url::Url::to_string),
            details_url: r.urls.details.as_ref().map(url::Url::to_string),
        }
    }
}

/// Per-item debug projection of a [`crate::dto::RawItem`]. Exposed only
/// through the inspect path — production search returns
/// `Vec<Release>` and discards this.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ItemDebug {
    /// `<title>` text.
    pub title: String,
    /// `<guid>` text.
    pub guid: String,
    /// `<size>` value (or `length=` from `<enclosure>`).
    pub size_bytes: u64,
    /// Download URL (from `<link>` or `<enclosure url=...>`).
    pub download_url: Option<String>,
    /// Details/comments URL.
    pub details_url: Option<String>,
    /// `<category>` element value.
    pub category: Option<String>,
    /// Every `<newznab:attr name=X value=Y/>` entry, grouped by name.
    /// Repeated attrs (multiple `audio`, multiple `subs`) are preserved.
    pub attrs: std::collections::HashMap<String, Vec<String>>,
}

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
            .user_agent(USER_AGENT)
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

    /// Debug helper: run a movie search by IMDb id and return the raw
    /// upstream body alongside the parsed [`Release`] list and a
    /// per-item dump of all `<newznab:attr>` keys/values. Used by the
    /// admin `/providers/{id}/probe` route so operators can see what
    /// attributes an indexer actually exposes and audit which ones the
    /// scoring rules consume.
    ///
    /// Bypasses the regular WARN-on-zero-items log — the probe is the
    /// operator's own diagnostic call.
    ///
    /// # Errors
    ///
    /// Returns [`ClientError`] on transport failure, HTTP non-2xx, or
    /// XML parse error.
    pub async fn inspect_movie_by_imdb(&self, imdb: ImdbId) -> Result<InspectResult, ClientError> {
        let url = self.build_url(
            "movie",
            &[
                ("imdbid", &format!("{:07}", imdb.get())),
                ("cat", MOVIE_CATEGORIES),
            ],
        )?;
        self.fetch_raw(url).await
    }

    /// Debug counterpart to [`Self::search_movie_by_tmdb`].
    ///
    /// # Errors
    ///
    /// See [`Self::inspect_movie_by_imdb`].
    pub async fn inspect_movie_by_tmdb(&self, tmdb: TmdbId) -> Result<InspectResult, ClientError> {
        let url = self.build_url(
            "movie",
            &[
                ("tmdbid", &tmdb.get().to_string()),
                ("cat", MOVIE_CATEGORIES),
            ],
        )?;
        self.fetch_raw(url).await
    }

    async fn fetch_raw(&self, url: Url) -> Result<InspectResult, ClientError> {
        let resp = self
            .http
            .get(url.clone())
            .send()
            .await?
            .error_for_status()?;
        let status = resp.status().as_u16();
        let body = resp.text().await?;
        let feed = parse_feed(&body)?;
        let mut releases = Vec::with_capacity(feed.items.len());
        let mut items_debug = Vec::with_capacity(feed.items.len());
        for item in &feed.items {
            items_debug.push(ItemDebug {
                title: item.title.clone(),
                guid: item.guid.clone(),
                size_bytes: item.size_bytes,
                download_url: item.download_url.clone(),
                details_url: item.details_url.clone(),
                category: item.category.clone(),
                attrs: item.attrs.clone(),
            });
            if let Ok(r) = item_to_release(item, self.tracker.clone()) {
                releases.push(ReleaseSnapshot::from_release(&r));
            }
        }
        Ok(InspectResult {
            request_url: redact_apikey(&url),
            http_status: status,
            body_bytes: body.len(),
            raw_body: body,
            items: items_debug,
            releases,
        })
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
