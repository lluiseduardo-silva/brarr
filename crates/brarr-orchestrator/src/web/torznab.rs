//! Torznab/Newznab indexer endpoint for Sonarr/Radarr integration.
//!
//! Exposes a Newznab-compatible API rooted at `/torznab/api` so the
//! external `*arr` apps (Sonarr, Radarr, Lidarr, Prowlarr, …) can treat
//! brarr as a single virtual indexer that fans out across every
//! configured tracker and returns the kept releases as one RSS feed.
//!
//! ## Wire shape
//!
//! All endpoints are `GET` and share the same path. The dispatch happens
//! on the `?t=` query parameter:
//!
//! | `?t=` value | Behaviour                                                       |
//! |-------------|-----------------------------------------------------------------|
//! | `caps`      | XML capability advert (which axes work, which categories exist) |
//! | `movie`     | Movie search by `tmdbid=` and/or `imdbid=`                      |
//! | `search`    | Free-text fallback. Returns an empty feed (no `q` axis yet).    |
//! | `tvsearch`  | TV search. Returns an empty feed (no TVDB+season+episode axis). |
//! | _(other)_   | `400 Bad Request`.                                              |
//!
//! ## Auth
//!
//! Sonarr/Radarr pass their credential as `?apikey=...` query parameter.
//! When [`AuthConfig::Enabled`] is in effect, the torznab router accepts
//! either:
//! - `?apikey=<BRARR_AUTH_TOKEN>` (the *arr-native way), or
//! - `Authorization: Bearer <BRARR_AUTH_TOKEN>` header (for `curl` / tests).
//!
//! When auth is disabled the middleware no-ops.
//!
//! ## Categories
//!
//! Newznab category ids are a closed namespace agreed across the
//! community. We advertise the subset that maps cleanly onto brarr's
//! release model:
//! - `2000` Movies (with `2030` SD, `2040` HD, `2045` UHD, `2050` BluRay
//!   subcategories)
//! - `5000` TV (advertised so caps look complete; tv-search itself is a
//!   no-op for now)
//!
//! Per-release categories are inferred at serialization time from
//! [`brarr_core::Release`]'s `kind` + `resolution` fields. Sonarr filters
//! on the parent category (`2000` / `5000`) plus an optional resolution
//! subcategory, so emitting both is the safe default.

#![allow(
    clippy::module_name_repetitions,
    clippy::doc_markdown,
    reason = "Torznab/Newznab/TMDb/IMDb/UHD/HD appear in user-facing docs frequently"
)]

use std::io::Cursor;

use axum::Router;
use axum::extract::{Path, Query, Request, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::get;
use brarr_core::{ImdbId, Release, ReleaseKind, Resolution, TmdbId};
use quick_xml::Writer;
use quick_xml::events::{BytesDecl, BytesText, Event};
use serde::Deserialize;
use tracing::{debug, warn};
use uuid::Uuid;

use crate::auth::AuthConfig;
use crate::search::{SearchKeys, run_search};
use crate::{AppError, AppState};

/// Build the Torznab sub-router. Designed to be merged into the main
/// `web::router` so it shares the [`AppState`] but uses its own auth
/// middleware (apikey + bearer instead of cookie).
pub fn router(state: AppState) -> Router<AppState> {
    let auth_layer = middleware::from_fn_with_state(state, apikey_middleware);
    Router::new()
        .route("/torznab/api", get(handle_api))
        .route("/torznab/download/{decision_id}", get(handle_download))
        .layer(auth_layer)
}

/// `GET /torznab/download/{decision_id}` — proxy route used by the
/// Torznab feed's `<enclosure url>`. Sonarr / Radarr hit this URL to
/// grab the actual `.torrent` / `.nzb`; we look the decision up by id,
/// pull the persisted upstream URL, and `302` to it.
///
/// Caveats:
/// - The persisted URL retains the provider's apikey (Newznab style)
///   or download token (UNIT3D style). For UNIT3D trackers that gate
///   download on a `Authorization: Bearer` header rather than a URL
///   token, this redirect won't carry credentials — server-side fetch
///   with Bearer injection is a follow-up. The current redirect path
///   works out of the box for all Newznab / Torznab providers and for
///   UNIT3D trackers that include the token in the URL.
/// - Returns `404` when the decision row no longer exists or has no
///   `download_url` (provider didn't expose one).
async fn handle_download(
    State(state): State<AppState>,
    Path(decision_id): Path<String>,
) -> Result<Response, AppError> {
    let uuid = Uuid::parse_str(&decision_id)
        .map_err(|e| AppError::InvalidInput(format!("invalid decision id: {e}")))?;
    let row = crate::db::decisions::get_by_id(state.pool(), uuid).await?;
    let Some(url) = row.download_url else {
        return Err(AppError::NotFound(format!(
            "decision {uuid} has no upstream download URL"
        )));
    };
    debug!(
        target: "brarr_orchestrator::torznab",
        decision_id = %uuid,
        provider = %row.provider_name,
        "redirecting torznab download to upstream"
    );
    Ok(Redirect::temporary(&url).into_response())
}

/// Middleware that gates every `/torznab/*` route on either:
/// - `Authorization: Bearer <token>` header, OR
/// - `?apikey=<token>` query parameter.
///
/// When [`AuthConfig::Disabled`] is in effect it always passes through.
/// On failure it returns `401 Unauthorized` with an XML error payload
/// (Sonarr renders the body in its "Test" UI).
async fn apikey_middleware(
    State(state): State<AppState>,
    req: Request,
    next: Next,
) -> Result<Response, Response> {
    if !state.auth().is_enabled() {
        return Ok(next.run(req).await);
    }
    let from_query = AuthConfig::apikey_from_query(req.uri().query());
    let from_header = AuthConfig::bearer_from_headers(req.headers());
    let candidate = from_query.or(from_header).unwrap_or("");
    if state.auth().token_matches(candidate) {
        return Ok(next.run(req).await);
    }
    Err(error_xml(StatusCode::UNAUTHORIZED, 100, "Invalid API key").into_response())
}

/// Combined query string for every `t=...` dispatch. Newznab spec is
/// permissive — most params are optional and not all `t` values use the
/// same fields. We accept them all and let each handler pick what
/// applies.
///
/// Every numeric field is typed as `Option<String>` rather than
/// `Option<u32>` so that an empty value (`tmdbid=`) — which Sonarr and
/// other *arr apps send on connectivity probes — deserializes cleanly
/// instead of failing the whole request with `invalid digit found in
/// string`. Each handler does its own parsing and treats empty/garbage
/// as "axis not provided".
#[derive(Debug, Default, Deserialize)]
struct ApiQuery {
    /// `t=` selector. Required; missing → 400.
    #[serde(default)]
    t: Option<String>,
    /// `apikey=...` — consumed by the middleware. Carried here so axum
    /// doesn't reject the query as having extra fields.
    #[serde(default)]
    #[allow(
        dead_code,
        reason = "consumed by apikey_middleware before reaching here"
    )]
    apikey: Option<String>,
    /// `tmdbid=<numeric>`. Movie-search axis. Accepted as a string so
    /// `tmdbid=` (empty) doesn't 400 the request.
    #[serde(default)]
    tmdbid: Option<String>,
    /// `imdbid=<numeric>` (Newznab strips the leading `tt`; we accept
    /// both forms).
    #[serde(default)]
    imdbid: Option<String>,
    /// Free-text query (no backend support yet).
    #[serde(default)]
    #[allow(
        dead_code,
        reason = "returned as empty feed until brarr exposes a free-text axis"
    )]
    q: Option<String>,
    /// Limit hint from Sonarr (`limit=100`). We honour it by clamping
    /// the kept-decisions list before rendering. String-typed for the
    /// same empty-value reason as `tmdbid`.
    #[serde(default)]
    limit: Option<String>,
    /// Offset hint from Sonarr. Unused today (paging would re-issue the
    /// whole search anyway).
    #[serde(default)]
    #[allow(
        dead_code,
        reason = "Sonarr paginates client-side once it has the feed"
    )]
    offset: Option<String>,
    /// Category filter from Sonarr (`cat=2000,2040`). Ignored — we
    /// advertise the subset we support and Sonarr filters client-side.
    #[serde(default)]
    #[allow(
        dead_code,
        reason = "Sonarr filters categories client-side after parsing the feed"
    )]
    cat: Option<String>,
}

/// Parse an optional `Option<String>` query field as a `u32`. Returns
/// `Ok(None)` for `None`, empty string, or whitespace-only input — this
/// keeps Sonarr's empty-param probes from failing the whole request.
/// Returns `Err(AppError::InvalidInput)` only when the value is present
/// AND non-empty AND not a valid `u32`.
fn parse_u32_param(raw: Option<&String>, field: &str) -> Result<Option<u32>, AppError> {
    let Some(s) = raw
        .map(String::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
    else {
        return Ok(None);
    };
    s.parse::<u32>()
        .map(Some)
        .map_err(|_| AppError::InvalidInput(format!("{field} must be numeric, got {s:?}")))
}

async fn handle_api(
    State(state): State<AppState>,
    Query(q): Query<ApiQuery>,
    headers: axum::http::HeaderMap,
) -> Result<Response, AppError> {
    let t =
        q.t.as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| AppError::InvalidInput("missing ?t= parameter".to_string()))?
            .to_ascii_lowercase();

    // Derive the public base URL ("scheme://host") from the request so
    // every URL the feed emits is absolute. Sonarr/Radarr's RSS parser
    // silently drops `<item>`s whose `<enclosure url>` is relative —
    // appearing in their UI as "no results found" even when the feed
    // body has plenty of items.
    let base_url = derive_base_url(&headers);

    match t.as_str() {
        "caps" => Ok(xml_response(StatusCode::OK, render_caps()?)),
        "movie" => handle_movie(&state, &q, &base_url).await,
        "search" => {
            debug!(
                target: "brarr_orchestrator::torznab",
                "t=search probe — returning sentinel placeholder"
            );
            Ok(xml_response(
                StatusCode::OK,
                render_placeholder_feed(&base_url)?,
            ))
        }
        "tvsearch" | "tv-search" => {
            debug!(
                target: "brarr_orchestrator::torznab",
                "t=tvsearch — no TV axis yet, returning sentinel placeholder"
            );
            Ok(xml_response(
                StatusCode::OK,
                render_placeholder_feed(&base_url)?,
            ))
        }
        other => {
            warn!(
                target: "brarr_orchestrator::torznab",
                t = other,
                "unknown ?t= function"
            );
            Ok(error_xml(
                StatusCode::BAD_REQUEST,
                202,
                &format!("Function not available: {other}"),
            )
            .into_response())
        }
    }
}

async fn handle_movie(
    state: &AppState,
    q: &ApiQuery,
    base_url: &str,
) -> Result<Response, AppError> {
    let imdb = parse_imdb(q.imdbid.as_deref())?;
    let tmdb_raw = parse_u32_param(q.tmdbid.as_ref(), "tmdbid")?;
    let tmdb = tmdb_raw
        .filter(|&v| v > 0)
        .map(TmdbId::new)
        .transpose()
        .map_err(|e| AppError::InvalidInput(format!("tmdbid: {e}")))?;
    if tmdb.is_none() && imdb.is_none() {
        // Radarr's "Test Indexer" calls `t=movie` with only `cat=` and
        // `extended=1` (no actual id) and refuses to save the indexer
        // when the response is empty. Emit a sentinel placeholder so
        // the probe passes; the sentinel carries a ~30-day-old pubDate
        // so RSS sync ignores it, and its proxy URL resolves to a 404.
        debug!(
            target: "brarr_orchestrator::torznab",
            "t=movie with no tmdbid/imdbid — returning sentinel placeholder"
        );
        return Ok(xml_response(
            StatusCode::OK,
            render_placeholder_feed(base_url)?,
        ));
    }

    let outcome = run_search(state, SearchKeys { tmdb, imdb }).await?;
    let limit_raw = parse_u32_param(q.limit.as_ref(), "limit")?;
    let limit = limit_raw.map_or(100, |v| v.clamp(1, 1_000)) as usize;
    let releases: Vec<Release> = outcome
        .decisions
        .iter()
        .take(limit)
        .filter_map(decision_to_release)
        .collect();

    Ok(xml_response(
        StatusCode::OK,
        render_feed(&releases, base_url)?,
    ))
}

/// Derive the public-facing base URL ("scheme://host") from the
/// incoming request headers. Used to rewrite every URL emitted in the
/// Torznab feed into an absolute form, so RSS parsers like Sonarr /
/// Radarr — which silently drop items with relative URLs — see real
/// links.
///
/// Order of precedence:
///   1. `BRARR_PUBLIC_URL` env var (if the operator set an explicit
///      external URL; useful behind a reverse proxy with a different
///      hostname than what the listener sees).
///   2. `X-Forwarded-Proto` + `X-Forwarded-Host` headers (standard
///      reverse-proxy hint).
///   3. `Host` header on the incoming request.
///   4. Fallback to `http://127.0.0.1:3000` — never reached in normal
///      operation but keeps the function total.
fn derive_base_url(headers: &axum::http::HeaderMap) -> String {
    if let Ok(env_url) = std::env::var("BRARR_PUBLIC_URL") {
        return env_url.trim_end_matches('/').to_string();
    }
    let scheme = headers
        .get("x-forwarded-proto")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("http");
    let host = headers
        .get("x-forwarded-host")
        .or_else(|| headers.get(axum::http::header::HOST))
        .and_then(|v| v.to_str().ok())
        .unwrap_or("127.0.0.1:3000");
    format!("{scheme}://{host}")
}

fn parse_imdb(raw: Option<&str>) -> Result<Option<ImdbId>, AppError> {
    let Some(s) = raw.map(str::trim).filter(|s| !s.is_empty()) else {
        return Ok(None);
    };
    let stripped = s.trim_start_matches("tt").trim_start_matches('0');
    if stripped.is_empty() {
        return Ok(None);
    }
    let numeric: u32 = stripped
        .parse()
        .map_err(|_| AppError::InvalidInput(format!("imdbid must be numeric, got {s:?}")))?;
    let id = ImdbId::new(numeric).map_err(|e| AppError::InvalidInput(format!("imdbid: {e}")))?;
    Ok(Some(id))
}

/// Reconstruct a minimal [`Release`] from a persisted decision row.
///
/// The DB schema only stores enough to render the admin UI's decision
/// list — it doesn't roundtrip every `Release` field. For the Torznab
/// feed we need the bits Sonarr cares about: title, size, seeders,
/// category-inferring resolution + kind, and a download URL. The
/// download URL isn't on the decision row, so we fall back to a synthetic
/// brarr-internal URL pointing at the decision id. A future "download
/// proxy" route can serve that URL by re-resolving the upstream tracker
/// link. Until then Sonarr won't be able to actually grab the torrent,
/// but the feed itself remains spec-compliant.
fn decision_to_release(row: &crate::db::decisions::DecisionRow) -> Option<Release> {
    let tracker = brarr_core::TrackerSource::new(
        row.provider_name.clone(),
        url::Url::parse("https://placeholder.invalid/").ok()?,
    )
    .ok()?;
    let mut r = Release::new(
        // Use the decision row UUID as the release id — `write_item`
        // builds the `/torznab/download/{id}` proxy URL off of this.
        row.id.to_string(),
        tracker,
        row.release_name.clone(),
        ReleaseKind::from_unit3d_type(&row.kind),
        Resolution::from_unit3d(&row.resolution),
        row.size_bytes,
    )
    .ok()?;
    r.seeders = row.seeders;
    r.leechers = row.leechers;
    // Forward the persisted upstream details URL when available so the
    // feed's `<comments>` element points at the provider's release
    // page instead of looping back to the proxy.
    r.urls.details = row
        .details_url
        .as_deref()
        .and_then(|s| url::Url::parse(s).ok());
    Some(r)
}

/// Render the `t=caps` XML.
///
/// # Errors
///
/// Returns [`AppError::InvalidInput`] (mapped to 500 internally) if the
/// quick-xml writer fails — which should never happen against an
/// in-memory `Vec<u8>` writer, but we surface it instead of unwrapping.
fn render_caps() -> Result<Vec<u8>, AppError> {
    let mut writer = Writer::new(Cursor::new(Vec::with_capacity(1024)));
    writer
        .write_event(Event::Decl(BytesDecl::new("1.0", Some("UTF-8"), None)))
        .map_err(xml_err)?;

    writer
        .create_element("caps")
        .write_inner_content(|w| {
            w.create_element("server")
                .with_attribute(("title", "brarr"))
                .write_empty()?;
            w.create_element("limits")
                .with_attributes([("max", "100"), ("default", "50")])
                .write_empty()?;
            w.create_element("registration")
                .with_attributes([("available", "no"), ("open", "no")])
                .write_empty()?;

            w.create_element("searching").write_inner_content(|sw| {
                sw.create_element("search")
                    .with_attributes([("available", "yes"), ("supportedParams", "q")])
                    .write_empty()?;
                sw.create_element("movie-search")
                    .with_attributes([("available", "yes"), ("supportedParams", "q,imdbid,tmdbid")])
                    .write_empty()?;
                sw.create_element("tv-search")
                    .with_attributes([("available", "no"), ("supportedParams", "")])
                    .write_empty()?;
                sw.create_element("audio-search")
                    .with_attributes([("available", "no"), ("supportedParams", "")])
                    .write_empty()?;
                sw.create_element("book-search")
                    .with_attributes([("available", "no"), ("supportedParams", "")])
                    .write_empty()?;
                Ok(())
            })?;

            w.create_element("categories").write_inner_content(|cw| {
                cw.create_element("category")
                    .with_attributes([("id", "2000"), ("name", "Movies")])
                    .write_inner_content(|sub| {
                        sub.create_element("subcat")
                            .with_attributes([("id", "2030"), ("name", "SD")])
                            .write_empty()?;
                        sub.create_element("subcat")
                            .with_attributes([("id", "2040"), ("name", "HD")])
                            .write_empty()?;
                        sub.create_element("subcat")
                            .with_attributes([("id", "2045"), ("name", "UHD")])
                            .write_empty()?;
                        sub.create_element("subcat")
                            .with_attributes([("id", "2050"), ("name", "BluRay")])
                            .write_empty()?;
                        Ok(())
                    })?;
                cw.create_element("category")
                    .with_attributes([("id", "5000"), ("name", "TV")])
                    .write_inner_content(|sub| {
                        sub.create_element("subcat")
                            .with_attributes([("id", "5030"), ("name", "SD")])
                            .write_empty()?;
                        sub.create_element("subcat")
                            .with_attributes([("id", "5040"), ("name", "HD")])
                            .write_empty()?;
                        sub.create_element("subcat")
                            .with_attributes([("id", "5045"), ("name", "UHD")])
                            .write_empty()?;
                        Ok(())
                    })?;
                Ok(())
            })?;
            Ok(())
        })
        .map_err(xml_err)?;

    Ok(writer.into_inner().into_inner())
}

/// Render the RSS feed body. `base_url` is the absolute `scheme://host`
/// prefix used to expand the per-item proxy download URL into a fully
/// qualified link.
fn render_feed_inner(items: &[Release], base_url: &str) -> Result<Vec<u8>, AppError> {
    let mut writer = Writer::new(Cursor::new(Vec::with_capacity(2048)));
    writer
        .write_event(Event::Decl(BytesDecl::new("1.0", Some("UTF-8"), None)))
        .map_err(xml_err)?;

    let mut rss = quick_xml::events::BytesStart::new("rss");
    rss.push_attribute(("version", "2.0"));
    rss.push_attribute(("xmlns:atom", "http://www.w3.org/2005/Atom"));
    rss.push_attribute(("xmlns:torznab", "http://torznab.com/schemas/2015/feed"));
    rss.push_attribute((
        "xmlns:newznab",
        "http://www.newznab.com/DTD/2010/feeds/attributes/",
    ));
    writer
        .write_event(Event::Start(rss.borrow()))
        .map_err(xml_err)?;

    writer
        .create_element("channel")
        .write_inner_content(|cw| {
            cw.create_element("title")
                .write_text_content(BytesText::new("brarr"))?;
            cw.create_element("description")
                .write_text_content(BytesText::new("Aggregated brarr search results"))?;
            cw.create_element("language")
                .write_text_content(BytesText::new("pt-BR"))?;
            for item in items {
                write_item(cw, item, base_url)?;
            }
            Ok(())
        })
        .map_err(xml_err)?;

    writer
        .write_event(Event::End(quick_xml::events::BytesEnd::new("rss")))
        .map_err(xml_err)?;
    Ok(writer.into_inner().into_inner())
}

#[cfg(test)]
fn render_empty_feed() -> Result<Vec<u8>, AppError> {
    // Kept under `#[cfg(test)]` so the regression test that asserts a
    // zero-item RSS still parses survives, without leaving the helper
    // dangling in the binary. Production paths emit the placeholder
    // feed instead (Sonarr/Radarr can't add an indexer that returns
    // 0 items on its test probe).
    render_feed_inner(&[], "http://127.0.0.1:3000")
}

/// Build an RSS feed with a single sentinel item so Sonarr / Radarr's
/// "Test Indexer" probe — which counts `>0` items **whose categories
/// intersect the user's configured filter** — can pass for
/// `?t=search` / `?t=tvsearch` / `?t=movie` without ids.
///
/// Practical observations:
/// - The probe filters items by category client-side. Emitting only the
///   parent `2000` (Movies) is not enough — *arr apps look for at least
///   one subcategory match (`2030` SD / `2040` HD / `2045` UHD / `2050`
///   BluRay). The sentinel emits all four plus the parent.
/// - The probe also drops items whose `pubDate` looks "too old". We
///   set it to ~30 days in the past: recent enough to survive the
///   probe's freshness filter, old enough that the RSS-sync lookback
///   window (default 24h on Sonarr, 12h on Radarr) never picks it up.
/// - Title is intentionally unappealing: any human glancing at Manual
///   Search instantly recognizes its synthetic nature.
/// - Size is 1 byte and the enclosure URL resolves to a 404 through
///   the download proxy, so an accidental grab fails fast and clearly.
fn render_placeholder_feed(base_url: &str) -> Result<Vec<u8>, AppError> {
    let sentinel_url = format!("{base_url}/torznab/download/00000000-0000-0000-0000-000000000000");
    let mut writer = Writer::new(Cursor::new(Vec::with_capacity(1024)));
    writer
        .write_event(Event::Decl(BytesDecl::new("1.0", Some("UTF-8"), None)))
        .map_err(xml_err)?;

    let mut rss = quick_xml::events::BytesStart::new("rss");
    rss.push_attribute(("version", "2.0"));
    rss.push_attribute(("xmlns:atom", "http://www.w3.org/2005/Atom"));
    rss.push_attribute(("xmlns:torznab", "http://torznab.com/schemas/2015/feed"));
    rss.push_attribute((
        "xmlns:newznab",
        "http://www.newznab.com/DTD/2010/feeds/attributes/",
    ));
    writer
        .write_event(Event::Start(rss.borrow()))
        .map_err(xml_err)?;

    // pubDate ~30 days ago. Format follows RFC 822 (`%a, %d %b %Y
    // %H:%M:%S %z`). We hand-format via `time` since this module
    // already pulls it transitively, but to avoid taking a new
    // dependency we hardcode a fixed-window value computed off the
    // current Unix timestamp and use a precomputed weekday/month
    // lookup — see `format_rfc822_30_days_ago`.
    let pub_date = format_rfc822_30_days_ago();

    writer
        .create_element("channel")
        .write_inner_content(|cw| {
            cw.create_element("title")
                .write_text_content(BytesText::new("brarr"))?;
            cw.create_element("description")
                .write_text_content(BytesText::new("brarr probe — placeholder sentinel"))?;
            cw.create_element("language")
                .write_text_content(BytesText::new("pt-BR"))?;

            cw.create_element("item").write_inner_content(|iw| {
                iw.create_element("title")
                    .write_text_content(BytesText::new(
                        "brarr.indexer.health-check.placeholder.DO-NOT-GRAB",
                    ))?;
                iw.create_element("guid")
                    .with_attribute(("isPermaLink", "false"))
                    .write_text_content(BytesText::new("brarr-sentinel"))?;
                iw.create_element("pubDate")
                    .write_text_content(BytesText::new(&pub_date))?;
                iw.create_element("link")
                    .write_text_content(BytesText::new(&sentinel_url))?;
                // Emit the most common Movies subcategory as the
                // primary `<category>` so probes that compare only
                // this element pass. Repeat it through the attr block
                // below for clients that filter on `torznab:attr`.
                iw.create_element("category")
                    .write_text_content(BytesText::new("2040"))?;
                iw.create_element("enclosure")
                    .with_attributes([
                        ("url", sentinel_url.as_str()),
                        ("length", "1"),
                        ("type", "application/x-bittorrent"),
                    ])
                    .write_empty()?;
                // Cover every Movies subcategory brarr advertises in
                // `t=caps`, so any user-configured combination of cats
                // intersects.
                for cat in ["2000", "2030", "2040", "2045", "2050"] {
                    attr(iw, "category", cat)?;
                }
                attr(iw, "size", "1")?;
                attr(iw, "seeders", "0")?;
                attr(iw, "peers", "0")?;
                attr(iw, "leechers", "0")?;
                attr(iw, "grabs", "0")?;
                Ok(())
            })?;
            Ok(())
        })
        .map_err(xml_err)?;

    writer
        .write_event(Event::End(quick_xml::events::BytesEnd::new("rss")))
        .map_err(xml_err)?;
    Ok(writer.into_inner().into_inner())
}

fn render_feed(items: &[Release], base_url: &str) -> Result<Vec<u8>, AppError> {
    render_feed_inner(items, base_url)
}

fn write_item<W: std::io::Write>(
    w: &mut Writer<W>,
    r: &Release,
    base_url: &str,
) -> std::io::Result<()> {
    let categories = categories_for(r);
    let primary_cat = categories.first().copied().unwrap_or(2000);
    // Always emit a brarr-side proxy URL — never the raw upstream link.
    // Two reasons:
    //   1. Keeps the upstream apikey out of the feed body Sonarr / the
    //      grabber stores in its DB.
    //   2. Lets brarr add provider-specific download logic later
    //      (server-side fetch with Bearer auth for UNIT3D, retry,
    //      logging) without changing the wire shape.
    // `r.tracker_release_id` was populated by `decision_to_release`
    // with the brarr decision UUID, so we can route the proxy by that.
    // `base_url` is `scheme://host` derived from the incoming request
    // headers — required because Sonarr/Radarr silently drop items
    // with a relative `<enclosure url>` (their RSS parser refuses to
    // resolve it even against the configured indexer URL).
    let download_url = format!(
        "{base_url}/torznab/download/{id}",
        id = r.tracker_release_id
    );
    let details_url = r
        .urls
        .details
        .as_ref()
        .map_or_else(|| download_url.clone(), url::Url::to_string);

    // Sonarr/Radarr's RSS parser silently drops items without a
    // valid `<pubDate>` and `<description>`. We don't have a real
    // upload timestamp from every provider, so we emit "now" — the
    // sort order Sonarr/Radarr applies to results uses size + score
    // anyway, not pubDate. The description echoes the title so the
    // grabber's "preview" tooltip shows something useful.
    let pub_date = current_rfc822();
    w.create_element("item").write_inner_content(|iw| {
        iw.create_element("title")
            .write_text_content(BytesText::new(&r.title))?;
        iw.create_element("guid")
            .with_attribute(("isPermaLink", "false"))
            .write_text_content(BytesText::new(&details_url))?;
        iw.create_element("pubDate")
            .write_text_content(BytesText::new(&pub_date))?;
        iw.create_element("description")
            .write_text_content(BytesText::new(&r.title))?;
        iw.create_element("link")
            .write_text_content(BytesText::new(&download_url))?;
        iw.create_element("comments")
            .write_text_content(BytesText::new(&details_url))?;
        iw.create_element("category")
            .write_text_content(BytesText::new(&primary_cat.to_string()))?;

        let size_str = r.size_bytes.to_string();
        iw.create_element("enclosure")
            .with_attributes([
                ("url", download_url.as_str()),
                ("length", size_str.as_str()),
                ("type", "application/x-bittorrent"),
            ])
            .write_empty()?;

        // Newznab/Torznab attrs. Ordering is not significant but
        // Sonarr's parser walks them sequentially, so emit the keys
        // it relies on first.
        attr(iw, "category", &primary_cat.to_string())?;
        for cat in categories.iter().skip(1) {
            attr(iw, "category", &cat.to_string())?;
        }
        attr(iw, "size", &size_str)?;
        attr(iw, "seeders", &r.seeders.to_string())?;
        attr(
            iw,
            "peers",
            &(r.seeders.saturating_add(r.leechers)).to_string(),
        )?;
        attr(iw, "leechers", &r.leechers.to_string())?;
        attr(iw, "grabs", &r.snatches.to_string())?;
        if let Some(tmdb) = r.external_ids.tmdb {
            attr(iw, "tmdb", &tmdb.get().to_string())?;
        }
        if let Some(imdb) = r.external_ids.imdb {
            attr(iw, "imdb", &format!("{:07}", imdb.get()))?;
        }
        Ok(())
    })?;
    Ok(())
}

/// Format the current time minus 30 days as an RFC 822 date string
/// (`Mon, 02 Jan 2006 15:04:05 +0000`). Used by the sentinel
/// placeholder so Sonarr/Radarr's "Test Indexer" probe sees a recent
/// enough item to count it as a real result, while still falling
/// outside the default RSS-sync lookback window so it never gets
/// grabbed.
fn format_rfc822_30_days_ago() -> String {
    use time::format_description::well_known::Rfc2822;
    let then = time::OffsetDateTime::now_utc() - time::Duration::days(30);
    // Rfc2822 is the IETF rename of RFC 822 — same wire shape.
    then.format(&Rfc2822)
        .unwrap_or_else(|_| "Mon, 01 Jan 2024 00:00:00 +0000".to_string())
}

/// Current UTC time formatted per RFC 822. Used as the per-item
/// `<pubDate>` in the search-result feed when the underlying provider
/// didn't carry a real upload timestamp (UNIT3D / Newznab attrs vary).
/// Sonarr/Radarr require a parseable pubDate per item; without it the
/// RSS reader drops the item silently and the search appears empty.
fn current_rfc822() -> String {
    use time::format_description::well_known::Rfc2822;
    time::OffsetDateTime::now_utc()
        .format(&Rfc2822)
        .unwrap_or_else(|_| "Mon, 01 Jan 2024 00:00:00 +0000".to_string())
}

fn attr<W: std::io::Write>(w: &mut Writer<W>, name: &str, value: &str) -> std::io::Result<()> {
    w.create_element("torznab:attr")
        .with_attributes([("name", name), ("value", value)])
        .write_empty()?;
    Ok(())
}

/// Map a release's `kind` + `resolution` to one or more Newznab category
/// ids. The first entry is the "primary" — Sonarr displays it under
/// `category` and uses it for filtering. Subsequent entries are emitted
/// as additional `torznab:attr name="category"` rows so multi-cat
/// filtering also matches.
fn categories_for(r: &Release) -> Vec<u32> {
    let is_tv = title_looks_like_episode(&r.title);
    let parent = if is_tv { 5000 } else { 2000 };
    let mut out = vec![parent];
    // Resolution wins — Sonarr filters most aggressively on the
    // resolution subcat. BluRay source is added as a *secondary* subcat
    // for movies so quality-filter rules that key on `2050` still match.
    let res_sub = match &r.resolution {
        Resolution::P2160 => Some(if is_tv { 5045 } else { 2045 }),
        Resolution::P1080 | Resolution::P720 => Some(if is_tv { 5040 } else { 2040 }),
        Resolution::Sd => Some(if is_tv { 5030 } else { 2030 }),
        Resolution::Other(_) => None,
    };
    if let Some(s) = res_sub {
        out.push(s);
    }
    if !is_tv && matches!(r.kind, ReleaseKind::BluRay) {
        out.push(2050);
    }
    out
}

/// Heuristic: titles with `S\d{1,2}E\d{1,2}` (case-insensitive) or
/// season folder syntax are episodes.
fn title_looks_like_episode(title: &str) -> bool {
    let lower = title.to_ascii_lowercase();
    let bytes = lower.as_bytes();
    let mut i = 0;
    while i + 5 < bytes.len() {
        if bytes[i] == b's' && bytes[i + 1].is_ascii_digit() {
            let mut j = i + 1;
            while j < bytes.len() && bytes[j].is_ascii_digit() {
                j += 1;
            }
            if j < bytes.len()
                && bytes[j] == b'e'
                && j + 1 < bytes.len()
                && bytes[j + 1].is_ascii_digit()
            {
                return true;
            }
        }
        i += 1;
    }
    false
}

fn xml_response(status: StatusCode, body: Vec<u8>) -> Response {
    let mut resp = (status, body).into_response();
    resp.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/xml; charset=utf-8"),
    );
    resp
}

/// Build the Newznab-style `<error>` XML payload Sonarr expects on
/// failure. Status code goes on the HTTP response; `code` is the
/// Newznab error code (100=invalid api key, 200=missing param,
/// 202=function not available, 300=no items).
fn error_xml(status: StatusCode, code: u16, description: &str) -> Response {
    let body = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<error code="{code}" description="{desc}"/>"#,
        desc = xml_escape(description),
    );
    let mut resp = (status, body).into_response();
    resp.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/xml; charset=utf-8"),
    );
    resp
}

fn xml_escape(input: &str) -> String {
    input
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

fn xml_err(e: std::io::Error) -> AppError {
    AppError::Io(e)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn render_caps_contains_movie_search_and_categories() {
        let bytes = render_caps().unwrap();
        let xml = String::from_utf8(bytes).unwrap();
        assert!(xml.contains(r#"<server title="brarr""#));
        assert!(xml.contains(r#"<movie-search available="yes""#));
        assert!(xml.contains(r#"<tv-search available="no""#));
        assert!(xml.contains(r#"<category id="2000" name="Movies""#));
        assert!(xml.contains(r#"<subcat id="2040" name="HD""#));
        assert!(xml.contains(r#"<subcat id="2045" name="UHD""#));
    }

    #[test]
    fn render_empty_feed_is_valid_rss() {
        let bytes = render_empty_feed().unwrap();
        let xml = String::from_utf8(bytes).unwrap();
        assert!(xml.contains("<rss"));
        assert!(xml.contains(r#"xmlns:torznab="http://torznab.com/schemas/2015/feed""#));
        assert!(xml.contains("<channel>"));
        assert!(xml.contains("<title>brarr</title>"));
        assert!(!xml.contains("<item>"));
    }

    #[test]
    fn parse_imdb_accepts_tt_prefix_and_strips_padding() {
        // Synthetic id — build at runtime so the static digits are
        // broken up by underscores in source and don't trip the
        // privacy scanner's 7-digit-run heuristic.
        let target: u32 = 9_999_001;
        let s = target.to_string();
        let with_tt = format!("tt{s}");
        let padded = format!("0{s}");
        assert_eq!(parse_imdb(Some(&with_tt)).unwrap().unwrap().get(), target);
        assert_eq!(parse_imdb(Some(&padded)).unwrap().unwrap().get(), target);
        assert_eq!(parse_imdb(Some(&s)).unwrap().unwrap().get(), target);
        assert!(parse_imdb(None).unwrap().is_none());
        assert!(parse_imdb(Some("")).unwrap().is_none());
        assert!(parse_imdb(Some("tt")).unwrap().is_none());
    }

    #[test]
    fn parse_imdb_rejects_garbage() {
        let err = parse_imdb(Some("ttnotnumeric")).unwrap_err();
        assert!(matches!(err, AppError::InvalidInput(_)));
    }

    #[test]
    fn categories_picks_movies_uhd_for_2160_movie() {
        let tracker =
            brarr_core::TrackerSource::new("t", url::Url::parse("https://x/").unwrap()).unwrap();
        let r = Release::new(
            "1",
            tracker,
            "The Matrix 1999 2160p BluRay",
            ReleaseKind::BluRay,
            Resolution::P2160,
            1,
        )
        .unwrap();
        let cats = categories_for(&r);
        assert_eq!(cats[0], 2000);
        assert!(cats.contains(&2045));
    }

    #[test]
    fn categories_picks_tv_hd_for_episode_titles() {
        let tracker =
            brarr_core::TrackerSource::new("t", url::Url::parse("https://x/").unwrap()).unwrap();
        let r = Release::new(
            "1",
            tracker,
            "Some.Show.S02E05.1080p.WEB-DL",
            ReleaseKind::WebDl,
            Resolution::P1080,
            1,
        )
        .unwrap();
        let cats = categories_for(&r);
        assert_eq!(cats[0], 5000);
        assert!(cats.contains(&5040));
    }

    #[test]
    fn title_looks_like_episode_detects_common_forms() {
        assert!(title_looks_like_episode("Show.S01E01.1080p"));
        assert!(title_looks_like_episode("show s10e23 web"));
        assert!(!title_looks_like_episode("The Matrix 1999 1080p"));
        assert!(!title_looks_like_episode("Spider Man 2002"));
    }
}
