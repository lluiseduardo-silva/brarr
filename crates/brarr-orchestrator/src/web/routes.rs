//! Axum routes for the admin UI.
//!
//! Layout:
//! - `GET  /`                    → dashboard
//! - `GET  /providers`           → provider list
//! - `POST /providers`           → add provider (HTMX form → partial)
//! - `DELETE /providers/{id}`    → remove provider (HTMX → empty body)
//! - `GET  /releases`            → decisions history
//! - `GET  /searches/{id}`       → search detail (kept + rejected)
//! - `POST /searches`            → kick off a `TMDb` search (HTMX → redirect)
//! - `GET  /login` / `POST /login` → admin token login form
//! - `POST /logout`              → clear session cookie
//! - `GET  /healthz`             → liveness probe (always unauth)
//! - `GET  /static/*path`        → static assets (always unauth)
//!
//! All routes except `/healthz`, `/login`, and `/static/**` go through
//! the auth middleware. When [`crate::AuthConfig::Disabled`] is in
//! effect the middleware no-ops.

use std::net::SocketAddr;

use axum::Router;
use axum::extract::{Form, Path, Request, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::{delete, get, post};
use serde::Deserialize;
use time::OffsetDateTime;
use time::format_description::well_known::Iso8601;
use tower_http::services::ServeDir;
use tower_http::trace::TraceLayer;
use tracing::info;
use uuid::Uuid;

use crate::auth::{AuthConfig, SESSION_COOKIE};
use crate::db::{decisions, providers, searches};
use crate::search::run_tmdb_search;
use crate::web::render::html;
use crate::web::templates::{
    DashboardTemplate, DecisionView, LoginTemplate, ProviderView, ProvidersListPartial,
    ProvidersTemplate, RecentSearchView, ReleasesTemplate, SearchDetailTemplate,
};
use crate::{AppError, AppState};
use brarr_core::TmdbId;

/// Build the Axum router with `state` as shared state.
pub fn router(state: AppState, static_dir: &std::path::Path) -> Router {
    let auth_layer = middleware::from_fn_with_state(state.clone(), auth_middleware);

    // Routes that require auth — wrapped by the middleware below.
    let protected = Router::new()
        .route("/", get(dashboard))
        .route("/providers", get(providers_index).post(providers_create))
        .route("/providers/{id}", delete(providers_delete))
        .route("/releases", get(releases_index))
        .route("/searches", post(searches_create))
        .route("/searches/{id}", get(search_detail))
        .route("/logout", post(logout))
        .layer(auth_layer);

    // Torznab/Newznab endpoint for Sonarr/Radarr — same shared state,
    // but its own auth middleware (apikey query / bearer header instead
    // of the UI session cookie).
    let torznab = crate::web::torznab::router(state.clone());

    // Open routes — login form, health, static files.
    Router::new()
        .merge(protected)
        .merge(torznab)
        .route("/login", get(login_get).post(login_post))
        .route("/healthz", get(healthz))
        .nest_service("/static", ServeDir::new(static_dir))
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}

/// Middleware that gates every protected route on the auth cookie.
/// When `AuthConfig::Disabled` is in effect it always passes through.
async fn auth_middleware(
    State(state): State<AppState>,
    req: Request,
    next: Next,
) -> Result<Response, Response> {
    if !state.auth().is_enabled() {
        return Ok(next.run(req).await);
    }
    let cookie = AuthConfig::cookie_from_headers(req.headers());
    let ok = cookie
        .as_deref()
        .is_some_and(|tok| state.auth().token_matches(tok));
    if ok {
        return Ok(next.run(req).await);
    }
    Err(Redirect::to("/login").into_response())
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

async fn login_get(State(state): State<AppState>) -> Result<Response, AppError> {
    // Auth disabled → bounce to dashboard so the form doesn't dangle.
    if !state.auth().is_enabled() {
        return Ok(Redirect::to("/").into_response());
    }
    html(&LoginTemplate {
        error_message: None,
    })
}

#[derive(Debug, Deserialize)]
struct LoginForm {
    token: String,
}

async fn login_post(
    State(state): State<AppState>,
    Form(form): Form<LoginForm>,
) -> Result<Response, AppError> {
    if !state.auth().is_enabled() {
        return Ok(Redirect::to("/").into_response());
    }
    if !state.auth().token_matches(form.token.trim()) {
        let mut resp = html(&LoginTemplate {
            error_message: Some("Token inválido.".to_string()),
        })?;
        *resp.status_mut() = StatusCode::UNAUTHORIZED;
        return Ok(resp);
    }
    // Token is opaque; the cookie value IS the token. HttpOnly +
    // SameSite=Strict prevents JS exfil and CSRF on cross-site nav.
    // No Secure flag because the orchestrator binds 127.0.0.1 by
    // default; reverse proxies serving over HTTPS should set it on
    // their layer.
    let cookie_value = format!(
        "{SESSION_COOKIE}={token}; Path=/; HttpOnly; SameSite=Strict",
        token = form.token.trim()
    );
    let mut headers = HeaderMap::new();
    if let Ok(v) = HeaderValue::from_str(&cookie_value) {
        headers.insert(header::SET_COOKIE, v);
    }
    Ok((StatusCode::SEE_OTHER, headers, Redirect::to("/")).into_response())
}

async fn logout() -> Response {
    // Overwrite the cookie with an immediate expiry.
    let mut headers = HeaderMap::new();
    let expired = format!("{SESSION_COOKIE}=; Path=/; HttpOnly; SameSite=Strict; Max-Age=0");
    if let Ok(v) = HeaderValue::from_str(&expired) {
        headers.insert(header::SET_COOKIE, v);
    }
    (StatusCode::SEE_OTHER, headers, Redirect::to("/login")).into_response()
}

async fn healthz() -> &'static str {
    "ok"
}

async fn dashboard(State(state): State<AppState>) -> Result<Response, AppError> {
    let pool = state.pool();
    let provider_rows = providers::list_all(pool).await?;
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
        provider_count: provider_rows.len(),
        search_count: searches::recent(pool, 200).await?.len(),
        recent_searches,
        recent_decisions,
    };
    html(&tmpl)
}

async fn providers_index(State(state): State<AppState>) -> Result<Response, AppError> {
    let rows = providers::list_all(state.pool()).await?;
    let providers = rows.into_iter().map(provider_view).collect();
    html(&ProvidersTemplate { providers })
}

#[derive(Debug, Deserialize)]
struct CreateProviderForm {
    name: String,
    base_url: String,
    api_token: String,
    #[serde(default)]
    kind: Option<String>,
    /// Optional filesystem path to a `.wasm`/`.wat` plugin module.
    /// When supplied, this provider is served by the WASM plugin host
    /// instead of the built-in HTTP clients.
    #[serde(default)]
    plugin_path: Option<String>,
}

async fn providers_create(
    State(state): State<AppState>,
    Form(form): Form<CreateProviderForm>,
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
    providers::insert(
        state.pool(),
        providers::NewProvider {
            name: form.name.trim(),
            base_url: &url,
            api_token: form.api_token.trim(),
            kind,
            plugin_path: plugin_path_buf.as_deref(),
        },
    )
    .await?;

    let rows = providers::list_all(state.pool()).await?;
    let providers = rows.into_iter().map(provider_view).collect();
    html(&ProvidersListPartial { providers })
}

async fn providers_delete(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Response, AppError> {
    let uuid = Uuid::parse_str(&id)
        .map_err(|e| AppError::InvalidInput(format!("invalid provider id: {e}")))?;
    let removed = providers::delete_by_id(state.pool(), uuid).await?;
    if !removed {
        return Err(AppError::NotFound(format!("provider {uuid}")));
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

fn provider_view(p: crate::db::providers::ProviderRow) -> ProviderView {
    ProviderView {
        id: p.id.to_string(),
        name: p.name,
        base_url: p.base_url.to_string(),
        kind: p.kind,
        created_at: format_ts(p.created_at),
    }
}

fn decision_view(d: crate::db::decisions::DecisionRow) -> DecisionView {
    DecisionView {
        id: d.id.to_string(),
        provider_name: d.provider_name,
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
