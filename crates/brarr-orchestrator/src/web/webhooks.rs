//! Inbound Radarr / Sonarr "Connect → Webhook" receiver.
//!
//! Each configured *arr instance gets its own URL — the path-bound
//! `{arr_instance_id}` tells the handler which arr fired the event
//! and which arr to push the resulting decision back to (when
//! auto-push is configured on the row).
//!
//! ## Auth
//!
//! Same model as the Torznab endpoint:
//!
//! - `?apikey=<BRARR_AUTH_TOKEN>` query param (the *arr-native shape
//!   — *arr lets the operator append it to the webhook URL), OR
//! - any request from a peer matching the
//!   [`crate::auth::BypassConfig`] allowlist.
//!
//! ## Event routing
//!
//! Only events that name a release brarr can search are honored:
//!
//! - Radarr `MovieAdded` → `SearchKeys { tmdb, imdb, .. }`.
//! - Sonarr `EpisodeAdded` → one [`SearchKeys`] per episode in the
//!   payload (capped at [`MAX_EPISODES_PER_PAYLOAD`] to bound damage
//!   from a runaway payload).
//! - Sonarr `SeriesAdded` → series-wide TVDB search (no season/episode).
//! - `Test` → 200 OK, no search. So the *arr "Test" button passes.
//! - Anything else → 202 Accepted, no search, audit row still
//!   persisted so the operator can see what arrived.
//!
//! ## Async model
//!
//! The handler synchronously validates the payload + IDs and persists
//! a [`crate::db::webhook_events`] row, then returns **202 Accepted**.
//! The actual search + (optional) auto-push runs in a background
//! `tokio::spawn` so *arr's retry-on-slow-response logic never
//! triggers. The audit row's `triggered_search_id` is back-filled by
//! the spawned task once the search completes.
//!
//! ## Idempotency
//!
//! Not implemented in v1. *arr may retry on transient errors; each
//! retry will create a duplicate search row. Cheap, no functional
//! harm — the next poll cycle would have searched anyway. Revisit if
//! the audit page gets noisy.

use std::borrow::Cow;

use axum::Router;
use axum::extract::{Path, Request, State};
use axum::http::{HeaderMap, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use brarr_arr::ArrKind;
use brarr_core::{ImdbId, TmdbId, TvdbId};
use serde::Deserialize;
use tracing::{info, warn};
use uuid::Uuid;

use crate::auth::AuthConfig;
use crate::db::arr_instances;
use crate::db::webhook_events::{self, NewWebhookEvent};
use crate::search::{self, SearchKeys, SearchRunOutcome};
use crate::{AppError, AppState};

/// Hard cap on the number of per-episode searches a single Sonarr
/// `EpisodeAdded` payload may spawn. Real payloads have 1-2 entries;
/// the cap bounds damage from a malformed or hostile request.
const MAX_EPISODES_PER_PAYLOAD: usize = 10;

/// Build the webhook sub-router. Merged at the top level alongside
/// `/torznab/*` so the auth model (`?apikey=` + bypass) stays
/// consistent across machine-facing surfaces.
pub fn router(state: AppState) -> Router<AppState> {
    let auth_layer = middleware::from_fn_with_state(state, webhook_apikey_middleware);
    Router::new()
        .route("/webhooks/radarr/{arr_instance_id}", post(radarr_webhook))
        .route("/webhooks/sonarr/{arr_instance_id}", post(sonarr_webhook))
        .layer(auth_layer)
}

/// Auth middleware shared by both webhook routes. Mirrors the Torznab
/// middleware: bypass first (so trusted LAN peers can wire the
/// webhook without an apikey), then `?apikey=` / bearer fallback.
async fn webhook_apikey_middleware(
    State(state): State<AppState>,
    req: Request,
    next: Next,
) -> Result<Response, Response> {
    if !state.auth().is_enabled() {
        return Ok(next.run(req).await);
    }
    let bypass = state.bypass();
    if !bypass.peers.is_empty()
        && let Some(ip) = crate::web::ip::caller_ip(&req, &bypass.proxies)
        && bypass.peers.contains(ip)
    {
        info!(
            target: "brarr_orchestrator::auth",
            peer = %ip,
            "webhook apikey bypass via trusted peer"
        );
        return Ok(next.run(req).await);
    }
    let candidate = AuthConfig::apikey_from_query(req.uri().query())
        .or_else(|| AuthConfig::bearer_from_headers(req.headers()).map(Cow::Borrowed))
        .unwrap_or_default();
    if state.auth().token_matches(&candidate) {
        return Ok(next.run(req).await);
    }
    Err((StatusCode::UNAUTHORIZED, "invalid apikey").into_response())
}

// ---- Radarr payload shapes -----------------------------------------

/// Subset of the Radarr Connect webhook payload brarr understands.
/// Other event types deserialize via the `#[serde(other)]` catch-all
/// so adding a new *arr event doesn't 400 the request.
#[derive(Debug, Deserialize)]
#[serde(tag = "eventType")]
enum RadarrEvent {
    /// Sent by the "Test" button in Radarr's UI.
    Test,
    /// A movie was added to Radarr's library — the canonical trigger
    /// for an immediate brarr search.
    MovieAdded {
        /// Movie metadata block. May be absent on partial Radarr
        /// builds; treated as "no usable ids" downstream.
        movie: Option<RadarrMovie>,
    },
    /// Any other event — persisted to the audit log, no search.
    #[serde(other)]
    Other,
}

#[derive(Debug, Deserialize)]
struct RadarrMovie {
    #[serde(rename = "tmdbId", default)]
    tmdb_id: Option<u32>,
    #[serde(rename = "imdbId", default)]
    imdb_id: Option<String>,
}

// ---- Sonarr payload shapes -----------------------------------------

/// Subset of the Sonarr Connect webhook payload brarr understands.
#[derive(Debug, Deserialize)]
#[serde(tag = "eventType")]
enum SonarrEvent {
    /// "Test" button.
    Test,
    /// New series added to the library — series-wide TVDB search.
    ///
    /// Sonarr's actual `eventType` is `SeriesAdd` (no "ed") — unlike
    /// Radarr's `MovieAdded`. We rename to match it and keep
    /// `SeriesAdded` as an alias in case a fork/version differs.
    #[serde(rename = "SeriesAdd", alias = "SeriesAdded")]
    SeriesAdded {
        /// Series metadata.
        series: Option<SonarrSeries>,
    },
    /// New episode added to the library — per-episode TVDB search.
    EpisodeAdded {
        /// Series metadata.
        series: Option<SonarrSeries>,
        /// Episode list. Sonarr may bundle multiple episodes per
        /// payload (e.g. season pack import).
        #[serde(default)]
        episodes: Vec<SonarrEpisode>,
    },
    /// Anything else — persisted to audit, no search.
    #[serde(other)]
    Other,
}

#[derive(Debug, Deserialize)]
struct SonarrSeries {
    #[serde(rename = "tvdbId", default)]
    tvdb_id: Option<u32>,
}

#[derive(Debug, Deserialize)]
struct SonarrEpisode {
    #[serde(rename = "seasonNumber", default)]
    season_number: Option<u16>,
    #[serde(rename = "episodeNumber", default)]
    episode_number: Option<u16>,
}

// ---- HTTP handlers --------------------------------------------------

async fn radarr_webhook(
    State(state): State<AppState>,
    Path(arr_id): Path<Uuid>,
    headers: HeaderMap,
    body: String,
) -> Result<Response, AppError> {
    handle_webhook(state, arr_id, ArrKind::Radarr, headers, body).await
}

async fn sonarr_webhook(
    State(state): State<AppState>,
    Path(arr_id): Path<Uuid>,
    headers: HeaderMap,
    body: String,
) -> Result<Response, AppError> {
    handle_webhook(state, arr_id, ArrKind::Sonarr, headers, body).await
}

/// Shared dispatch for both webhook kinds. Validates the arr id,
/// parses the payload (loosely — unknown event types are persisted
/// but not searched), and spawns the search.
async fn handle_webhook(
    state: AppState,
    arr_id: Uuid,
    expected_kind: ArrKind,
    headers: HeaderMap,
    body: String,
) -> Result<Response, AppError> {
    let arr = arr_instances::get_by_id(state.pool(), arr_id).await?;
    if arr.kind != expected_kind {
        warn!(
            target: "brarr_orchestrator::webhook",
            arr_id = %arr_id,
            expected = ?expected_kind,
            actual = ?arr.kind,
            "webhook kind mismatch — refusing"
        );
        return Ok((
            StatusCode::BAD_REQUEST,
            "wrong webhook kind for this arr instance",
        )
            .into_response());
    }

    // Extract event type for the audit row before we deserialize into
    // the typed enum — this way we still record events even if the
    // typed parse later fails on a future Radarr/Sonarr schema bump.
    let event_type = parse_event_type(&body).unwrap_or_else(|| "Unknown".to_string());

    let event_row = webhook_events::insert(
        state.pool(),
        NewWebhookEvent {
            arr_instance_id: arr_id,
            kind: arr.kind,
            event_type: &event_type,
            payload_json: &body,
        },
    )
    .await?;

    info!(
        target: "brarr_orchestrator::webhook",
        arr_id = %arr_id,
        kind = ?arr.kind,
        event = %event_type,
        event_id = %event_row.id,
        "webhook received"
    );

    let keys_list = match expected_kind {
        ArrKind::Radarr => parse_radarr_keys(&body)?,
        ArrKind::Sonarr => parse_sonarr_keys(&body)?,
    };

    if keys_list.is_empty() {
        return Ok((StatusCode::OK, "ok").into_response());
    }

    let base_url = crate::push::derive_request_base(&state, &headers);
    for keys in keys_list {
        let state = state.clone();
        let arr = arr.clone();
        let event_id = event_row.id;
        let base_url = base_url.clone();
        tokio::spawn(async move {
            spawn_search_and_push(state, arr, keys, event_id, base_url).await;
        });
    }

    Ok((StatusCode::ACCEPTED, "accepted").into_response())
}

/// Body of the spawned task: run the search, link it back to the
/// audit row, then attempt auto-push using the same gating logic the
/// poller applies.
async fn spawn_search_and_push(
    state: AppState,
    arr: crate::db::arr_instances::ArrInstanceRow,
    keys: SearchKeys,
    event_id: Uuid,
    base_url: String,
) {
    if !keys.has_any() {
        return;
    }
    let outcome = match search::run_search(&state, keys).await {
        Ok(o) => o,
        Err(e) => {
            warn!(
                target: "brarr_orchestrator::webhook",
                event_id = %event_id,
                error = %e,
                "webhook-triggered search failed"
            );
            return;
        }
    };
    if let Err(e) = webhook_events::link_search(state.pool(), event_id, outcome.search.id).await {
        warn!(
            target: "brarr_orchestrator::webhook",
            event_id = %event_id,
            error = %e,
            "failed to back-fill triggered_search_id"
        );
    }

    if !arr.enabled {
        return;
    }
    try_auto_push(&state, &arr, &outcome, &base_url).await;
}

async fn try_auto_push(
    state: &AppState,
    arr: &crate::db::arr_instances::ArrInstanceRow,
    outcome: &SearchRunOutcome,
    base_url: &str,
) {
    let decision = match crate::poll::pick_pushable(state, arr, &outcome.decisions).await {
        Ok(Some(d)) => d,
        Ok(None) => return,
        Err(e) => {
            warn!(
                target: "brarr_orchestrator::webhook",
                arr = %arr.name,
                error = %e,
                "pick_pushable failed"
            );
            return;
        }
    };
    if let Err(e) = crate::push::push_decision(state, decision, arr, base_url).await {
        warn!(
            target: "brarr_orchestrator::webhook",
            arr = %arr.name,
            error = %e,
            "auto-push failed"
        );
    }
}

// ---- Parsing helpers -----------------------------------------------

/// Peek at the `eventType` field without fully typing the payload.
/// Used so the audit row records a useful label even when the typed
/// enum parse later fails on a future *arr schema change.
fn parse_event_type(body: &str) -> Option<String> {
    #[derive(Deserialize)]
    struct OnlyEventType {
        #[serde(rename = "eventType")]
        event_type: Option<String>,
    }
    let parsed: OnlyEventType = serde_json::from_str(body).ok()?;
    parsed.event_type
}

/// Returns the list of search keys to run for a Radarr payload. Empty
/// vec means "nothing to search" (Test, Other, or MovieAdded with no
/// usable ids).
fn parse_radarr_keys(body: &str) -> Result<Vec<SearchKeys>, AppError> {
    let event: RadarrEvent = serde_json::from_str(body)
        .map_err(|e| AppError::InvalidInput(format!("radarr webhook payload: {e}")))?;
    let RadarrEvent::MovieAdded { movie: Some(movie) } = event else {
        return Ok(Vec::new());
    };
    let tmdb = movie.tmdb_id.and_then(|n| TmdbId::new(n).ok());
    let imdb = movie.imdb_id.as_deref().and_then(parse_imdb_string);
    if tmdb.is_none() && imdb.is_none() {
        return Ok(Vec::new());
    }
    Ok(vec![SearchKeys {
        tmdb,
        imdb,
        ..SearchKeys::default()
    }])
}

/// Returns the list of search keys to run for a Sonarr payload. One
/// entry per episode for `EpisodeAdded`, one entry for `SeriesAdded`.
fn parse_sonarr_keys(body: &str) -> Result<Vec<SearchKeys>, AppError> {
    let event: SonarrEvent = serde_json::from_str(body)
        .map_err(|e| AppError::InvalidInput(format!("sonarr webhook payload: {e}")))?;
    match event {
        SonarrEvent::SeriesAdded { series: Some(s) } => {
            let Some(tvdb) = s.tvdb_id.and_then(|n| TvdbId::new(n).ok()) else {
                return Ok(Vec::new());
            };
            Ok(vec![SearchKeys::from_tvdb(tvdb, None, None)])
        }
        SonarrEvent::EpisodeAdded {
            series: Some(s),
            episodes,
        } => {
            let Some(tvdb) = s.tvdb_id.and_then(|n| TvdbId::new(n).ok()) else {
                return Ok(Vec::new());
            };
            let mut out = Vec::new();
            for ep in episodes.into_iter().take(MAX_EPISODES_PER_PAYLOAD) {
                out.push(SearchKeys::from_tvdb(
                    tvdb,
                    ep.season_number,
                    ep.episode_number,
                ));
            }
            Ok(out)
        }
        _ => Ok(Vec::new()),
    }
}

/// Parse the Radarr-flavored IMDb id (`"tt0133093"` or `"0133093"`)
/// into [`ImdbId`].
fn parse_imdb_string(raw: &str) -> Option<ImdbId> {
    let trimmed = raw.trim_start_matches("tt").trim_start_matches('0');
    if trimmed.is_empty() {
        return None;
    }
    let n: u32 = trimmed.parse().ok()?;
    ImdbId::new(n).ok()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[test]
    fn parse_event_type_handles_test_payload() {
        let body = r#"{"eventType":"Test","movie":{"tmdbId":603}}"#;
        assert_eq!(parse_event_type(body).as_deref(), Some("Test"));
    }

    #[test]
    fn parse_event_type_returns_none_on_garbage() {
        assert_eq!(parse_event_type("not json").as_deref(), None);
        assert_eq!(parse_event_type("{}").as_deref(), None);
    }

    #[test]
    fn radarr_test_event_yields_no_keys() {
        let body = r#"{"eventType":"Test"}"#;
        assert!(parse_radarr_keys(body).unwrap().is_empty());
    }

    #[test]
    fn radarr_movie_added_extracts_tmdb_and_imdb() {
        let body = r#"{
            "eventType":"MovieAdded",
            "movie":{"tmdbId":603,"imdbId":"tt0133093"}
        }"#;
        let keys = parse_radarr_keys(body).unwrap();
        assert_eq!(keys.len(), 1);
        assert_eq!(keys[0].tmdb.map(TmdbId::get), Some(603));
        assert_eq!(keys[0].imdb.map(ImdbId::get), Some(133_093));
    }

    #[test]
    fn radarr_movie_added_without_ids_is_skipped() {
        let body = r#"{"eventType":"MovieAdded","movie":{}}"#;
        assert!(parse_radarr_keys(body).unwrap().is_empty());
    }

    #[test]
    fn radarr_unknown_event_yields_no_keys() {
        let body = r#"{"eventType":"Grab","movie":{"tmdbId":603}}"#;
        assert!(parse_radarr_keys(body).unwrap().is_empty());
    }

    #[test]
    fn radarr_garbage_payload_returns_invalid_input() {
        let err = parse_radarr_keys("not json").unwrap_err();
        assert!(matches!(err, AppError::InvalidInput(_)));
    }

    #[test]
    fn sonarr_test_event_yields_no_keys() {
        let body = r#"{"eventType":"Test"}"#;
        assert!(parse_sonarr_keys(body).unwrap().is_empty());
    }

    #[test]
    fn sonarr_series_added_carries_tvdb_only() {
        let body = r#"{"eventType":"SeriesAdded","series":{"tvdbId":81189}}"#;
        let keys = parse_sonarr_keys(body).unwrap();
        assert_eq!(keys.len(), 1);
        assert_eq!(keys[0].tvdb.map(TvdbId::get), Some(81189));
        assert_eq!(keys[0].season, None);
        assert_eq!(keys[0].episode, None);
    }

    #[test]
    fn sonarr_episode_added_spawns_one_key_per_episode() {
        let body = r#"{
            "eventType":"EpisodeAdded",
            "series":{"tvdbId":81189},
            "episodes":[
                {"seasonNumber":1,"episodeNumber":1},
                {"seasonNumber":1,"episodeNumber":2}
            ]
        }"#;
        let keys = parse_sonarr_keys(body).unwrap();
        assert_eq!(keys.len(), 2);
        assert_eq!(keys[0].season, Some(1));
        assert_eq!(keys[0].episode, Some(1));
        assert_eq!(keys[1].episode, Some(2));
    }

    #[test]
    fn sonarr_episode_added_caps_at_max_per_payload() {
        use std::fmt::Write as _;
        let mut episodes =
            String::from(r#"{"eventType":"EpisodeAdded","series":{"tvdbId":81189},"episodes":["#);
        for i in 1..=25 {
            if i > 1 {
                episodes.push(',');
            }
            let _ = write!(episodes, r#"{{"seasonNumber":1,"episodeNumber":{i}}}"#);
        }
        episodes.push_str("]}");
        let keys = parse_sonarr_keys(&episodes).unwrap();
        assert_eq!(keys.len(), MAX_EPISODES_PER_PAYLOAD);
    }

    #[test]
    fn sonarr_unknown_event_yields_no_keys() {
        let body = r#"{"eventType":"Rename","series":{"tvdbId":81189}}"#;
        assert!(parse_sonarr_keys(body).unwrap().is_empty());
    }

    #[test]
    fn parse_imdb_handles_tt_prefix_and_leading_zeros() {
        assert_eq!(
            parse_imdb_string("tt0133093").map(ImdbId::get),
            Some(133_093)
        );
        assert_eq!(parse_imdb_string("0000133").map(ImdbId::get), Some(133));
        assert_eq!(parse_imdb_string("tt"), None);
        assert_eq!(parse_imdb_string(""), None);
        assert_eq!(parse_imdb_string("bad"), None);
    }
}
