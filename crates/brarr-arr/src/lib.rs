//! `brarr-arr` — HTTP client for Sonarr / Radarr v3 REST APIs.
//!
//! Inverts brarr's relationship with the *arr suite. Today brarr is a
//! pull-target (Sonarr / Radarr scrape its Torznab feed periodically).
//! This crate gives brarr a way to **push**: when its rules engine
//! decides a release is worth grabbing, it can notify the matching
//! *arr instance directly via `POST /api/v3/release/push`. The *arr
//! client then parses the title, matches against its library, and
//! hands the download URL to its configured download client.
//!
//! Modeled after autobrr's "Actions → Push to *arr" surface. Brarr
//! retains all scoring authority; *arr is reduced to a download
//! conductor.
//!
//! # Wire shape
//!
//! Sonarr and Radarr share the same v3 endpoint family:
//!
//! | Endpoint                         | Verb | Used for                                  |
//! |----------------------------------|------|-------------------------------------------|
//! | `/api/v3/system/status`          | GET  | Healthcheck. Returns instance name+version. |
//! | `/api/v3/release/push`           | POST | Inject a release for *arr to consider.     |
//!
//! Auth is the `X-Api-Key` header — same key the *arr web UI generates
//! under Settings → General → Security.
//!
//! # Errors
//!
//! All HTTP / parse failures surface as [`ArrError`]. The crate
//! intentionally avoids panicking — every call returns a `Result`.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

use std::time::Duration;

use reqwest::Client as HttpClient;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use time::OffsetDateTime;
use tracing::{debug, warn};
use url::Url;

/// Which *arr flavour this client speaks to. Used to pick the protocol
/// default for outgoing pushes (Sonarr defaults to the user's mix,
/// Radarr is movies-only) and to route per-flavour log lines. The wire
/// shape is identical between the two.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ArrKind {
    /// Sonarr — TV.
    Sonarr,
    /// Radarr — movies.
    Radarr,
}

impl ArrKind {
    /// Human label (`"sonarr"` / `"radarr"`) used in logs + DB rows.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Sonarr => "sonarr",
            Self::Radarr => "radarr",
        }
    }
}

/// Static configuration for a single *arr instance brarr talks to.
///
/// Borrowed-style: the orchestrator owns the canonical config and
/// hands a snapshot to each [`ArrClient`]. Cloning is cheap (a few
/// `String`s + a parsed [`Url`]).
#[derive(Debug, Clone)]
pub struct ArrInstance {
    /// Operator-chosen display name (e.g. `"radarr-main"`). Used in
    /// admin UI and log lines.
    pub name: String,
    /// Sonarr vs Radarr.
    pub kind: ArrKind,
    /// Base URL of the *arr instance (e.g. `https://radarr.internal/`).
    /// Trailing slash is normalised by [`ArrClient::endpoint`].
    pub base_url: Url,
    /// API key from *arr Settings → General → Security.
    pub api_key: String,
}

/// Errors a [`ArrClient`] call can surface.
#[derive(Debug, Error)]
pub enum ArrError {
    /// `reqwest` failed to build the request or the response failed
    /// mid-flight (DNS, TLS, connection reset, body decode).
    #[error("transport error talking to {kind:?}: {source}")]
    Transport {
        /// Which flavour was being contacted.
        kind: ArrKind,
        /// Underlying `reqwest` error.
        #[source]
        source: reqwest::Error,
    },
    /// Server returned a non-2xx status. The wire body is captured so
    /// callers can surface the *arr-side rejection reason
    /// (e.g. "Unknown movie", "Indexer disabled").
    #[error("{kind:?} returned HTTP {status}: {body}")]
    Http {
        /// Flavour.
        kind: ArrKind,
        /// HTTP status code.
        status: u16,
        /// Body verbatim (limited to first 1KiB to avoid log spam).
        body: String,
    },
    /// Response body parsed but didn't match the expected shape. Rare
    /// in practice — Sonarr/Radarr keep the v3 schema stable across
    /// minor versions.
    #[error("{kind:?} returned malformed JSON: {source}")]
    Decode {
        /// Flavour.
        kind: ArrKind,
        /// Underlying serde error.
        #[source]
        source: serde_json::Error,
    },
    /// `base_url` was invalid. Surfaced from [`ArrClient::endpoint`].
    #[error("invalid URL: {0}")]
    InvalidUrl(#[from] url::ParseError),
}

/// `release/push` payload. Mirrors the v3 `ReleaseResource` schema but
/// only carries fields *arr actually consults when injecting a release.
/// Unknown fields are skipped on serialize via `Option::is_none`.
///
/// Wire shape is `camelCase` (matches *arr v3); Rust field names stay
/// `snake_case` via `rename_all`.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PushReleasePayload {
    /// Release title — the *arr title parser munches this to find the
    /// movie/series + quality + language. MUST match a scene-style or
    /// release-group-style name; *arr drops the push otherwise.
    pub title: String,
    /// HTTPS URL the *arr's download client should grab. Brarr emits
    /// its own `/torznab/download/{id}` (or `/newznab/download/{id}`)
    /// proxy URL here so the upstream apikey doesn't leak into *arr's
    /// DB.
    pub download_url: String,
    /// `"torrent"` or `"usenet"` — *arr routes to the matching
    /// download-client family based on this.
    pub protocol: ArrProtocol,
    /// Release publish date (upload timestamp on the tracker). Drives
    /// *arr's "Age" filter in profile rules.
    #[serde(with = "time::serde::rfc3339")]
    pub publish_date: OffsetDateTime,
    /// Total bytes — *arr quality profiles can require min/max size.
    pub size: u64,
    /// Display name of the source indexer ("brarr"). Shown in the *arr
    /// activity / history view.
    pub indexer: String,
    /// Optional details / info page URL. Surfaced as the "Comments"
    /// link in *arr's interactive search view.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub info_url: Option<String>,
    /// Optional seeders count — only meaningful when
    /// `protocol == Torrent`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub seeders: Option<u32>,
    /// Optional leechers count — only meaningful when
    /// `protocol == Torrent`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub leechers: Option<u32>,
}

/// Wire form of the `protocol` field on a push payload. Matches *arr's
/// internal `DownloadProtocol` enum.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ArrProtocol {
    /// `BitTorrent` download.
    Torrent,
    /// Newznab / Usenet download.
    Usenet,
}

/// Minimal slice of `/api/v3/system/status`. *arr returns ~40 fields
/// per call; only [`SystemStatus::app_name`] and
/// [`SystemStatus::version`] go into brarr's log line + admin UI badge.
#[derive(Debug, Clone, Deserialize)]
pub struct SystemStatus {
    /// `"Sonarr"` / `"Radarr"`.
    #[serde(rename = "appName")]
    pub app_name: String,
    /// Semver-ish version string.
    pub version: String,
}

/// Slice of one Radarr `/api/v3/movie` row that brarr's poller needs
/// to drive a search. Radarr returns ~60 fields per movie; we keep
/// the ones that map directly to brarr's search axis (`TMDb` / `IMDb`)
/// plus a couple of book-keeping flags so the poller can skip
/// already-grabbed entries cheaply.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WantedMovie {
    /// Radarr-side numeric id (used to dedup poll runs).
    pub id: u64,
    /// Movie title (logging only — brarr searches by id).
    pub title: String,
    /// `TMDb` id. `0` means "not linked" — Radarr stores zero rather
    /// than null on movies the user added by `IMDb` only.
    pub tmdb_id: u32,
    /// `IMDb` id with the `tt` prefix (e.g. `"tt0133093"`). Empty when
    /// Radarr couldn't resolve one.
    #[serde(default)]
    pub imdb_id: String,
    /// Whether the user actually wants Radarr to keep grabbing this
    /// title. Disabled rows are skipped.
    #[serde(default)]
    pub monitored: bool,
    /// `true` if Radarr already has a file on disk for this movie.
    /// The poller skips these.
    #[serde(default)]
    pub has_file: bool,
}

/// One wanted-missing episode row from Sonarr `/api/v3/wanted/missing`.
/// Brarr's poller iterates this to drive per-episode TV searches.
///
/// The endpoint paginates by default (`pageSize=10`); brarr uses
/// `pageSize=200` and passes `includeSeries=true` so each row carries
/// its nested series object — Sonarr v4 omits it by default and
/// brarr's poller has no other path to the TVDB id without a second
/// API call.
///
/// Some Sonarr versions still omit `series` even with the include flag.
/// [`ArrClient::wanted_episodes`] handles that by fetching the series
/// list once and joining by `series_id` in-memory.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WantedEpisode {
    /// Sonarr-side episode id (debug + dedup).
    pub id: u64,
    /// FK to the parent series row. Used to backfill [`Self::series`]
    /// when the inline projection isn't present.
    #[serde(default)]
    pub series_id: u64,
    /// Episode title (logging only — brarr searches by tvdb+season+ep).
    #[serde(default)]
    pub title: String,
    /// Season number (1-based per Sonarr convention).
    pub season_number: u16,
    /// Episode number within the season.
    pub episode_number: u16,
    /// Whether Sonarr wants this episode grabbed.
    #[serde(default)]
    pub monitored: bool,
    /// `true` if Sonarr already has a file. Always `false` on
    /// `/wanted/missing` rows; the field stays here for symmetry with
    /// [`WantedMovie`] and to defend against future Sonarr versions
    /// shifting the endpoint semantics.
    #[serde(default)]
    pub has_file: bool,
    /// Nested series data. Optional because Sonarr v4 omits it by
    /// default — [`ArrClient::wanted_episodes`] backfills via a
    /// separate `/api/v3/series` fetch when missing.
    #[serde(default)]
    pub series: Option<WantedEpisodeSeries>,
}

/// Minimal series projection embedded inside [`WantedEpisode`].
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WantedEpisodeSeries {
    /// Sonarr-side series id.
    pub id: u64,
    /// Series title (logging).
    pub title: String,
    /// `TVDB` id — required for the search axis. `0` means "not
    /// linked" (skip).
    #[serde(default)]
    pub tvdb_id: u32,
    /// Whether Sonarr is monitoring this series overall. Episode
    /// `monitored` can be `true` while the series-level is `false`
    /// when the user toggled individual episodes; brarr poller respects
    /// the episode-level flag.
    #[serde(default)]
    pub monitored: bool,
}

/// Sonarr's pagination envelope around any `records:` endpoint.
/// Surfaced for [`ArrClient::wanted_episodes`] only.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WantedEpisodesPage {
    /// 1-based page index Sonarr returned for this call.
    #[serde(default)]
    pub page: u32,
    /// Echo of the requested `pageSize`.
    #[serde(default)]
    pub page_size: u32,
    /// Total rows that match the query across all pages — drives the
    /// stop condition in [`ArrClient::wanted_episodes`].
    #[serde(default)]
    pub total_records: u32,
    /// Episode rows for this page.
    #[serde(default)]
    pub records: Vec<WantedEpisode>,
}

/// HTTP client for one *arr instance. Cheap to clone (wraps a shared
/// `reqwest::Client` Arc internally). One instance per *arr in the
/// orchestrator pool.
#[derive(Debug, Clone)]
pub struct ArrClient {
    instance: ArrInstance,
    http: HttpClient,
}

impl ArrClient {
    /// Build a client over `instance`. Returns an error if the TLS
    /// backend cannot be constructed.
    ///
    /// # Errors
    ///
    /// - [`ArrError::Transport`] if `reqwest::Client::builder` fails to
    ///   instantiate the TLS backend (rare; system-level).
    pub fn new(instance: ArrInstance) -> Result<Self, ArrError> {
        let http = HttpClient::builder()
            .user_agent(concat!("brarr/", env!("CARGO_PKG_VERSION")))
            .timeout(Duration::from_secs(15))
            .build()
            .map_err(|source| ArrError::Transport {
                kind: instance.kind,
                source,
            })?;
        Ok(Self { instance, http })
    }

    /// The instance config this client was built for.
    #[must_use]
    pub fn instance(&self) -> &ArrInstance {
        &self.instance
    }

    /// Build the absolute URL for an *arr API path. `path` should NOT
    /// start with a `/` — we always append to the v3 root. A missing
    /// trailing slash on `base_url` is normalised so `Url::join`
    /// appends instead of replacing the last segment (which would
    /// route `https://radarr.example/radarr` → `.../api` instead of
    /// `.../radarr/api`).
    fn endpoint(&self, path: &str) -> Result<Url, ArrError> {
        let mut base = self.instance.base_url.clone();
        if !base.path().ends_with('/') {
            let mut new_path = base.path().to_string();
            new_path.push('/');
            base.set_path(&new_path);
        }
        let joined = base.join("api/v3/")?.join(path)?;
        Ok(joined)
    }

    /// `GET /api/v3/system/status` — healthcheck. Returns the *arr
    /// instance name + version so the admin UI can show a green badge.
    ///
    /// # Errors
    ///
    /// - [`ArrError::Transport`] on network / TLS failure.
    /// - [`ArrError::Http`] on non-2xx status (401 means bad apikey).
    /// - [`ArrError::Decode`] if the JSON body isn't shaped like
    ///   [`SystemStatus`].
    pub async fn ping(&self) -> Result<SystemStatus, ArrError> {
        let url = self.endpoint("system/status")?;
        debug!(
            target: "brarr_arr",
            kind = self.instance.kind.label(),
            name = %self.instance.name,
            url = %url,
            "ping"
        );
        let resp = self
            .http
            .get(url)
            .header("X-Api-Key", &self.instance.api_key)
            .send()
            .await
            .map_err(|source| ArrError::Transport {
                kind: self.instance.kind,
                source,
            })?;
        let status = resp.status();
        let body = resp.text().await.map_err(|source| ArrError::Transport {
            kind: self.instance.kind,
            source,
        })?;
        if !status.is_success() {
            warn!(
                target: "brarr_arr",
                kind = self.instance.kind.label(),
                name = %self.instance.name,
                status = status.as_u16(),
                "ping returned non-2xx"
            );
            return Err(ArrError::Http {
                kind: self.instance.kind,
                status: status.as_u16(),
                body: truncate_body(&body),
            });
        }
        serde_json::from_str(&body).map_err(|source| ArrError::Decode {
            kind: self.instance.kind,
            source,
        })
    }

    /// `GET /api/v3/movie` — fetch every movie configured in this
    /// Radarr instance. Brarr's poller filters to `monitored=true`
    /// and `has_file=false` to drive search. Returns the raw list;
    /// caller decides which entries warrant a search.
    ///
    /// Only meaningful for [`ArrKind::Radarr`]. Calling this against
    /// a Sonarr instance returns an [`ArrError::Http`] (Sonarr's
    /// `/api/v3/movie` is a 404) — callers should branch on
    /// [`ArrInstance::kind`] before invoking.
    ///
    /// # Errors
    ///
    /// - [`ArrError::Transport`] on network failure.
    /// - [`ArrError::Http`] on non-2xx (401 bad apikey, 404 wrong flavour).
    /// - [`ArrError::Decode`] if the JSON body doesn't shape like
    ///   `Vec<WantedMovie>`.
    pub async fn monitored_movies(&self) -> Result<Vec<WantedMovie>, ArrError> {
        let url = self.endpoint("movie")?;
        debug!(
            target: "brarr_arr",
            kind = self.instance.kind.label(),
            name = %self.instance.name,
            url = %url,
            "fetch monitored movies"
        );
        let resp = self
            .http
            .get(url)
            .header("X-Api-Key", &self.instance.api_key)
            .send()
            .await
            .map_err(|source| ArrError::Transport {
                kind: self.instance.kind,
                source,
            })?;
        let status = resp.status();
        let body = resp.text().await.map_err(|source| ArrError::Transport {
            kind: self.instance.kind,
            source,
        })?;
        if !status.is_success() {
            warn!(
                target: "brarr_arr",
                kind = self.instance.kind.label(),
                status = status.as_u16(),
                "monitored_movies returned non-2xx"
            );
            return Err(ArrError::Http {
                kind: self.instance.kind,
                status: status.as_u16(),
                body: truncate_body(&body),
            });
        }
        serde_json::from_str(&body).map_err(|source| ArrError::Decode {
            kind: self.instance.kind,
            source,
        })
    }

    /// `GET /api/v3/wanted/missing?page=N&pageSize=200&sortKey=airDateUtc` —
    /// walk every page until the total is consumed and return the
    /// flattened list. Brarr's poller filters to
    /// `monitored && tvdb_id > 0` to drive per-episode TV searches.
    ///
    /// Only meaningful for [`ArrKind::Sonarr`]. Calling against Radarr
    /// returns an [`ArrError::Http`] (404).
    ///
    /// # Errors
    ///
    /// - [`ArrError::Transport`] on network failure.
    /// - [`ArrError::Http`] on non-2xx.
    /// - [`ArrError::Decode`] if a page's JSON doesn't shape like
    ///   [`WantedEpisodesPage`].
    pub async fn wanted_episodes(&self) -> Result<Vec<WantedEpisode>, ArrError> {
        const PAGE_SIZE: u32 = 200;
        let mut all: Vec<WantedEpisode> = Vec::new();
        let mut page: u32 = 1;
        loop {
            let mut url = self.endpoint("wanted/missing")?;
            {
                let mut qp = url.query_pairs_mut();
                qp.append_pair("page", &page.to_string());
                qp.append_pair("pageSize", &PAGE_SIZE.to_string());
                qp.append_pair("sortKey", "airDateUtc");
                qp.append_pair("sortDirection", "descending");
                // Sonarr v4 omits the nested `series` projection by
                // default. Asking for it explicitly lets us avoid a
                // second `/api/v3/series` round trip on every poll.
                qp.append_pair("includeSeries", "true");
            }
            debug!(
                target: "brarr_arr",
                kind = self.instance.kind.label(),
                name = %self.instance.name,
                page,
                url = %url,
                "fetch wanted episodes"
            );
            let resp = self
                .http
                .get(url)
                .header("X-Api-Key", &self.instance.api_key)
                .send()
                .await
                .map_err(|source| ArrError::Transport {
                    kind: self.instance.kind,
                    source,
                })?;
            let status = resp.status();
            let body = resp.text().await.map_err(|source| ArrError::Transport {
                kind: self.instance.kind,
                source,
            })?;
            if !status.is_success() {
                warn!(
                    target: "brarr_arr",
                    kind = self.instance.kind.label(),
                    status = status.as_u16(),
                    "wanted_episodes returned non-2xx"
                );
                return Err(ArrError::Http {
                    kind: self.instance.kind,
                    status: status.as_u16(),
                    body: truncate_body(&body),
                });
            }
            let parsed: WantedEpisodesPage =
                serde_json::from_str(&body).map_err(|source| ArrError::Decode {
                    kind: self.instance.kind,
                    source,
                })?;
            let total = parsed.total_records;
            let got_records = parsed.records.len();
            all.extend(parsed.records);
            // Stop when we've seen everything or the page returned a
            // partial batch (defensive: avoids infinite loops if the
            // server lies about totalRecords).
            if u32::try_from(all.len()).unwrap_or(u32::MAX) >= total
                || got_records < PAGE_SIZE as usize
            {
                break;
            }
            page += 1;
        }

        // Backfill series for rows where Sonarr v4 ignored
        // `includeSeries=true` (older 4.x builds quietly drop the
        // flag). Fetch the full series list once and join by id —
        // one extra request beats N round-trips for an N-episode
        // poll.
        let needs_backfill = all.iter().any(|e| e.series.is_none() && e.series_id > 0);
        if needs_backfill {
            debug!(
                target: "brarr_arr",
                name = %self.instance.name,
                "some episodes missing nested series; fetching series list for backfill"
            );
            let series_list = self.list_series().await?;
            let by_id: std::collections::HashMap<u64, WantedEpisodeSeries> =
                series_list.into_iter().map(|s| (s.id, s)).collect();
            for ep in &mut all {
                if ep.series.is_none() {
                    ep.series = by_id.get(&ep.series_id).cloned();
                }
            }
        }
        Ok(all)
    }

    /// `GET /api/v3/series` — full series list. Cheap call (one JSON
    /// page) used to backfill the nested `series` projection on
    /// [`WantedEpisode`] rows when Sonarr v4 omits it from
    /// `/wanted/missing`.
    ///
    /// # Errors
    ///
    /// Same shape as [`Self::wanted_episodes`].
    async fn list_series(&self) -> Result<Vec<WantedEpisodeSeries>, ArrError> {
        let url = self.endpoint("series")?;
        let resp = self
            .http
            .get(url)
            .header("X-Api-Key", &self.instance.api_key)
            .send()
            .await
            .map_err(|source| ArrError::Transport {
                kind: self.instance.kind,
                source,
            })?;
        let status = resp.status();
        let body = resp.text().await.map_err(|source| ArrError::Transport {
            kind: self.instance.kind,
            source,
        })?;
        if !status.is_success() {
            return Err(ArrError::Http {
                kind: self.instance.kind,
                status: status.as_u16(),
                body: truncate_body(&body),
            });
        }
        serde_json::from_str(&body).map_err(|source| ArrError::Decode {
            kind: self.instance.kind,
            source,
        })
    }

    /// `POST /api/v3/release/push` — inject a release for *arr to
    /// consider grabbing. Brarr calls this when its rules engine
    /// decides a release crosses the auto-grab threshold; *arr then
    /// parses the title, matches against its library, and grabs via
    /// the configured download client if the parse + library lookup
    /// succeed.
    ///
    /// Returns the **response body** verbatim (truncated to 1KiB) on
    /// a 2xx response. *arr returns rejection reasons inside the JSON
    /// body even when the HTTP status is 200 (e.g. "Unknown movie",
    /// "Quality profile rejected"). Callers should persist the body
    /// so operators can audit *why* an apparently-successful push
    /// didn't trigger a download.
    ///
    /// # Errors
    ///
    /// - [`ArrError::Transport`] on network / TLS failure.
    /// - [`ArrError::Http`] on non-2xx status (400 means malformed
    ///   payload, 401 means bad apikey).
    pub async fn push_release(&self, payload: &PushReleasePayload) -> Result<String, ArrError> {
        let url = self.endpoint("release/push")?;
        debug!(
            target: "brarr_arr",
            kind = self.instance.kind.label(),
            name = %self.instance.name,
            title = %payload.title,
            protocol = ?payload.protocol,
            "push release"
        );
        let resp = self
            .http
            .post(url)
            .header("X-Api-Key", &self.instance.api_key)
            .json(payload)
            .send()
            .await
            .map_err(|source| ArrError::Transport {
                kind: self.instance.kind,
                source,
            })?;
        let status = resp.status();
        let body = resp
            .text()
            .await
            .unwrap_or_else(|_| String::from("<no body>"));
        let truncated = truncate_body(&body);
        if status.is_success() {
            // *arr returns rejection reasons inside the 200 body. A
            // non-empty rejections array means "I accepted the request
            // but won't grab" — the operator needs the body to debug
            // why. An empty array `[]` is the happy path.
            debug!(
                target: "brarr_arr",
                kind = self.instance.kind.label(),
                name = %self.instance.name,
                body_excerpt = %truncated,
                "push accepted"
            );
            return Ok(truncated);
        }
        warn!(
            target: "brarr_arr",
            kind = self.instance.kind.label(),
            name = %self.instance.name,
            status = status.as_u16(),
            body_excerpt = %truncated,
            "push rejected"
        );
        Err(ArrError::Http {
            kind: self.instance.kind,
            status: status.as_u16(),
            body: truncated,
        })
    }
}

/// Cap the body slice we keep in errors / log lines so a chatty *arr
/// instance doesn't blow the log volume on a 5xx page. 8 KiB fits the
/// full `/release/push` response (typically ~1.5 KiB of release fields
/// plus a `rejections` array at the end) — the rejections array is the
/// part operators need to see when a 200 push silently produced no
/// grab, so the cap must sit *past* the trailing fields rather than
/// inside the verbose middle.
fn truncate_body(body: &str) -> String {
    const MAX: usize = 8192;
    if body.len() <= MAX {
        return body.to_string();
    }
    let mut out: String = body.chars().take(MAX).collect();
    out.push_str("…[truncated]");
    out
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    fn instance(kind: ArrKind, base: &str) -> ArrInstance {
        ArrInstance {
            name: "x".into(),
            kind,
            base_url: Url::parse(base).unwrap(),
            api_key: "k".into(),
        }
    }

    #[test]
    fn arr_kind_serializes_lowercase() {
        let s = serde_json::to_string(&ArrKind::Sonarr).unwrap();
        assert_eq!(s, "\"sonarr\"");
        let r = serde_json::to_string(&ArrKind::Radarr).unwrap();
        assert_eq!(r, "\"radarr\"");
    }

    #[test]
    fn arr_protocol_serializes_lowercase() {
        let t = serde_json::to_string(&ArrProtocol::Torrent).unwrap();
        assert_eq!(t, "\"torrent\"");
        let u = serde_json::to_string(&ArrProtocol::Usenet).unwrap();
        assert_eq!(u, "\"usenet\"");
    }

    #[test]
    fn endpoint_joins_base_url_with_trailing_slash() {
        let c = ArrClient::new(instance(ArrKind::Radarr, "https://r.example/")).unwrap();
        let url = c.endpoint("release/push").unwrap();
        assert_eq!(url.as_str(), "https://r.example/api/v3/release/push");
    }

    #[test]
    fn endpoint_normalises_missing_trailing_slash() {
        // No trailing slash + a path that would normally cause
        // `Url::join` to replace the last segment.
        let c = ArrClient::new(instance(ArrKind::Radarr, "https://r.example/radarr")).unwrap();
        let url = c.endpoint("system/status").unwrap();
        assert_eq!(
            url.as_str(),
            "https://r.example/radarr/api/v3/system/status",
            "missing trailing slash must be normalised so join() appends rather than replaces",
        );
    }

    #[test]
    fn push_payload_skips_none_optional_fields() {
        let payload = PushReleasePayload {
            title: "t".into(),
            download_url: "https://x".into(),
            protocol: ArrProtocol::Torrent,
            publish_date: OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap(),
            size: 1,
            indexer: "brarr".into(),
            info_url: None,
            seeders: None,
            leechers: None,
        };
        let v: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(&payload).unwrap()).unwrap();
        assert!(v.get("infoUrl").is_none());
        assert!(v.get("seeders").is_none());
        assert!(v.get("leechers").is_none());
    }

    #[test]
    fn push_payload_uses_camelcase_keys() {
        // *arr v3 endpoints are camelCase. Pin the wire shape so a
        // refactor that drops `rename_all` doesn't silently break.
        let payload = PushReleasePayload {
            title: "t".into(),
            download_url: "https://x".into(),
            protocol: ArrProtocol::Torrent,
            publish_date: OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap(),
            size: 1,
            indexer: "brarr".into(),
            info_url: Some("https://y".into()),
            seeders: Some(42),
            leechers: Some(1),
        };
        let v: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(&payload).unwrap()).unwrap();
        assert!(v.get("downloadUrl").is_some(), "{v}");
        assert!(v.get("publishDate").is_some(), "{v}");
        assert!(v.get("infoUrl").is_some(), "{v}");
        assert_eq!(v.get("protocol").and_then(|p| p.as_str()), Some("torrent"));
    }

    #[test]
    fn truncate_body_caps_long_strings() {
        let s = "x".repeat(20_000);
        let t = truncate_body(&s);
        assert!(t.len() < s.len());
        assert!(t.ends_with("[truncated]"));
    }

    #[test]
    fn truncate_body_passes_through_when_under_cap() {
        let s = "x".repeat(2000);
        let t = truncate_body(&s);
        assert_eq!(t, s, "2KB body must not be truncated");
    }
}
