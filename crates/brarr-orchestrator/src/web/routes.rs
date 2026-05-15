//! Axum routes for the admin UI.
//!
//! Layout:
//! - `GET  /`                    → dashboard
//! - `GET  /trackers`            → tracker list
//! - `POST /trackers`            → add tracker (HTMX form → partial)
//! - `DELETE /trackers/{id}`     → remove tracker (HTMX → empty body)
//! - `GET  /releases`            → decisions history
//! - `GET  /searches/{id}`       → search detail (kept + rejected)
//! - `POST /searches`            → kick off a `TMDb` search (HTMX → redirect)
//! - `GET  /healthz`             → liveness probe
//! - `GET  /static/*path`        → static assets (htmx, custom CSS)

use std::net::SocketAddr;

use axum::Router;
use axum::extract::{Form, Path, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use serde::Deserialize;
use time::OffsetDateTime;
use time::format_description::well_known::Iso8601;
use tower_http::services::ServeDir;
use tower_http::trace::TraceLayer;
use tracing::info;
use uuid::Uuid;

use crate::db::{decisions, searches, trackers};
use crate::search::run_tmdb_search;
use crate::web::render::html;
use crate::web::templates::{
    DashboardTemplate, DecisionView, RecentSearchView, ReleasesTemplate, SearchDetailTemplate,
    TrackerView, TrackersListPartial, TrackersTemplate,
};
use crate::{AppError, AppState};
use brarr_core::TmdbId;

/// Build the Axum router with `state` as shared state.
pub fn router(state: AppState, static_dir: &std::path::Path) -> Router {
    Router::new()
        .route("/", get(dashboard))
        .route("/healthz", get(healthz))
        .route("/trackers", get(trackers_index).post(trackers_create))
        .route("/trackers/{id}", delete(trackers_delete))
        .route("/releases", get(releases_index))
        .route("/searches", post(searches_create))
        .route("/searches/{id}", get(search_detail))
        .nest_service("/static", ServeDir::new(static_dir))
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}

/// Bind to `addr` and serve the router until the future is dropped.
///
/// # Errors
///
/// Surfaces I/O errors (bind failure, accept loop crash).
pub async fn serve(
    state: AppState,
    addr: SocketAddr,
    static_dir: &std::path::Path,
) -> Result<(), AppError> {
    info!(target: "brarr_orchestrator::web", %addr, "starting HTTP server");
    let app = router(state, static_dir);
    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await.map_err(AppError::Io)?;
    Ok(())
}

async fn healthz() -> &'static str {
    "ok"
}

async fn dashboard(State(state): State<AppState>) -> Result<Response, AppError> {
    let pool = state.pool();
    let tracker_rows = trackers::list_all(pool).await?;
    let recent_search_rows = searches::recent(pool, 10).await?;
    let recent_decision_rows = decisions::recent(pool, 10).await?;

    let recent_searches = recent_search_rows
        .into_iter()
        .map(|s| RecentSearchView {
            id: s.id.to_string(),
            tmdb_id: s.tmdb_id.map_or_else(|| "-".to_string(), |v| v.to_string()),
            submitted_at: format_ts(s.submitted_at),
            result_count: s.result_count,
        })
        .collect();

    let recent_decisions = recent_decision_rows
        .into_iter()
        .filter(|d| !d.rejected)
        .map(decision_view)
        .collect();

    let tmpl = DashboardTemplate {
        tracker_count: tracker_rows.len(),
        search_count: searches::recent(pool, 200).await?.len(),
        recent_searches,
        recent_decisions,
    };
    html(&tmpl)
}

async fn trackers_index(State(state): State<AppState>) -> Result<Response, AppError> {
    let rows = trackers::list_all(state.pool()).await?;
    let trackers = rows.into_iter().map(tracker_view).collect();
    html(&TrackersTemplate { trackers })
}

#[derive(Debug, Deserialize)]
struct CreateTrackerForm {
    name: String,
    base_url: String,
    api_token: String,
    #[serde(default)]
    kind: Option<String>,
    /// Optional filesystem path to a `.wasm`/`.wat` plugin module.
    /// When supplied, this tracker is served by the WASM plugin host
    /// instead of the built-in UNIT3D client.
    #[serde(default)]
    plugin_path: Option<String>,
}

async fn trackers_create(
    State(state): State<AppState>,
    Form(form): Form<CreateTrackerForm>,
) -> Result<Response, AppError> {
    let url = url::Url::parse(form.base_url.trim())
        .map_err(|e| AppError::InvalidInput(format!("invalid base_url: {e}")))?;
    let plugin_path_buf: Option<std::path::PathBuf> = form
        .plugin_path
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(std::path::PathBuf::from);
    let kind = form
        .kind
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| {
            if plugin_path_buf.is_some() {
                "plugin"
            } else {
                "unit3d"
            }
        });
    trackers::insert(
        state.pool(),
        trackers::NewTracker {
            name: form.name.trim(),
            base_url: &url,
            api_token: form.api_token.trim(),
            kind,
            plugin_path: plugin_path_buf.as_deref(),
        },
    )
    .await?;

    let rows = trackers::list_all(state.pool()).await?;
    let trackers = rows.into_iter().map(tracker_view).collect();
    html(&TrackersListPartial { trackers })
}

async fn trackers_delete(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Response, AppError> {
    let uuid = Uuid::parse_str(&id)
        .map_err(|e| AppError::InvalidInput(format!("invalid tracker id: {e}")))?;
    let removed = trackers::delete_by_id(state.pool(), uuid).await?;
    if !removed {
        return Err(AppError::NotFound(format!("tracker {uuid}")));
    }
    // HTMX expects the targeted element to be replaced; returning an
    // empty 200 lets `hx-target=closest tr` + `hx-swap=outerHTML` wipe
    // the row without re-rendering the whole list.
    Ok((StatusCode::OK, "").into_response())
}

async fn releases_index(State(state): State<AppState>) -> Result<Response, AppError> {
    let rows = decisions::recent(state.pool(), 50).await?;
    let decisions = rows.into_iter().map(decision_view).collect();
    html(&ReleasesTemplate { decisions })
}

#[derive(Debug, Deserialize)]
struct CreateSearchForm {
    tmdb_id: u32,
}

async fn searches_create(
    State(state): State<AppState>,
    Form(form): Form<CreateSearchForm>,
) -> Result<Response, AppError> {
    let tmdb = TmdbId::new(form.tmdb_id)
        .map_err(|e| AppError::InvalidInput(format!("invalid tmdb_id: {e}")))?;
    let outcome = run_tmdb_search(&state, tmdb).await?;
    // HTMX picks up `HX-Redirect` and performs a client-side redirect.
    let mut headers = HeaderMap::new();
    let location = format!("/searches/{}", outcome.search.id);
    headers.insert(
        "HX-Redirect",
        HeaderValue::from_str(&location).unwrap_or_else(|_| HeaderValue::from_static("/")),
    );
    headers.insert(
        header::LOCATION,
        HeaderValue::from_str(&location).unwrap_or_else(|_| HeaderValue::from_static("/")),
    );
    Ok((StatusCode::SEE_OTHER, headers, "").into_response())
}

async fn search_detail(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Response, AppError> {
    let uuid = Uuid::parse_str(&id)
        .map_err(|e| AppError::InvalidInput(format!("invalid search id: {e}")))?;
    let search = searches::get_by_id(state.pool(), uuid).await?;
    let decisions_rows = decisions::list_for_search(state.pool(), uuid).await?;
    let decisions = decisions_rows.into_iter().map(decision_view).collect();
    let tmpl = SearchDetailTemplate {
        id: search.id.to_string(),
        tmdb_id: search
            .tmdb_id
            .map_or_else(|| "-".to_string(), |v| v.to_string()),
        submitted_at: format_ts(search.submitted_at),
        decisions,
        failures: Vec::new(), // live failures aren't persisted today
    };
    html(&tmpl)
}

fn tracker_view(t: crate::db::trackers::TrackerRow) -> TrackerView {
    TrackerView {
        id: t.id.to_string(),
        name: t.name,
        base_url: t.base_url.to_string(),
        kind: t.kind,
        created_at: format_ts(t.created_at),
    }
}

fn decision_view(d: crate::db::decisions::DecisionRow) -> DecisionView {
    DecisionView {
        id: d.id.to_string(),
        tracker_name: d.tracker_name,
        release_name: d.release_name,
        score: d.score,
        rejected: d.rejected,
        tags: d.tags.join(", "),
        resolution: d.resolution,
        kind: d.kind,
        seeders: d.seeders,
        size_human: humanize_bytes(d.size_bytes),
    }
}

fn format_ts(ts: OffsetDateTime) -> String {
    ts.format(&Iso8601::DEFAULT)
        .unwrap_or_else(|_| ts.unix_timestamp().to_string())
}

fn humanize_bytes(b: u64) -> String {
    const UNITS: &[&str] = &["B", "KiB", "MiB", "GiB", "TiB"];
    #[allow(
        clippy::cast_precision_loss,
        reason = "byte counts are several orders of magnitude below the f64 mantissa limit"
    )]
    let mut value = b as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{b} B")
    } else {
        format!("{value:.2} {}", UNITS[unit])
    }
}
