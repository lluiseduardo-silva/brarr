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
use crate::db::{arr_instances, decisions, providers, push_history, searches};
#[allow(
    unused_imports,
    reason = "re-exported for downstream tests that still call it"
)]
use crate::search::run_tmdb_search;
use crate::web::render::html;
use crate::web::templates::{
    ArrInstanceView, ArrInstancesListPartial, ArrInstancesTemplate, DashboardTemplate,
    DecisionView, LoginTemplate, ProviderView, ProvidersListPartial, ProvidersTemplate,
    PushHistoryView, PushesTemplate, RecentSearchView, ReleasesTemplate, SearchDetailTemplate,
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
        .route("/providers/{id}/test", post(providers_test))
        .route("/providers/{id}/probe", get(providers_probe))
        .route("/providers/{id}/toggle", post(providers_toggle))
        .route(
            "/arr-instances",
            get(arr_instances_index).post(arr_instances_create),
        )
        .route("/arr-instances/{id}", delete(arr_instances_delete))
        .route("/arr-instances/{id}/test", post(arr_instances_test))
        .route("/arr-instances/{id}/poll-now", post(arr_instances_poll_now))
        .route(
            "/arr-instances/{id}/threshold",
            post(arr_instances_update_threshold),
        )
        .route("/decisions/{id}/push/{arr_id}", post(decisions_push))
        .route("/pushes", get(pushes_index))
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

/// `GET /providers/{id}/probe?imdb=X&tmdb=Y` — diagnostic dump.
///
/// Runs a real search against the upstream provider and returns the
/// raw response body plus a per-item breakdown of every
/// `<newznab:attr>` (or UNIT3D JSON field) alongside brarr's parsed
/// `Release` snapshot. Lets operators audit which fields an indexer
/// actually exposes versus what the scoring/ranking rules consume.
///
/// Returns `application/json`. Pass at least one of `imdb` / `tmdb`;
/// `imdb` accepts the `tt0123456` form. Only `newznab` / `torznab`
/// providers are supported today — `unit3d` probes return a stub
/// pointing the operator at a future enhancement (the JSON envelope
/// from UNIT3D would be similarly useful but isn't wrapped yet).
#[derive(Debug, Deserialize)]
struct ProbeQuery {
    #[serde(default)]
    imdb: Option<String>,
    #[serde(default)]
    tmdb: Option<String>,
}

async fn providers_probe(
    State(state): State<AppState>,
    Path(id): Path<String>,
    axum::extract::Query(q): axum::extract::Query<ProbeQuery>,
) -> Result<Response, AppError> {
    let uuid = Uuid::parse_str(&id)
        .map_err(|e| AppError::InvalidInput(format!("invalid provider id: {e}")))?;
    let row = providers::get_by_id(state.pool(), uuid).await?;
    let source = brarr_core::TrackerSource::new(row.name.clone(), row.base_url.clone())
        .map_err(|e| AppError::InvalidInput(format!("invalid base_url: {e}")))?;

    let imdb = parse_optional_imdb(q.imdb.as_deref())?;
    let tmdb = parse_optional_tmdb(q.tmdb.as_deref())?;
    if imdb.is_none() && tmdb.is_none() {
        return Err(AppError::InvalidInput(
            "informe ?imdb= ou ?tmdb=".to_string(),
        ));
    }

    let kind = row.kind.to_ascii_lowercase();
    if kind != "newznab" && kind != "torznab" {
        let body = serde_json::json!({
            "provider": { "name": row.name, "kind": row.kind, "base_url": row.base_url.to_string() },
            "note": "probe inspection only implemented for newznab/torznab providers today. \
                    Adding UNIT3D + plugin support is a separate enhancement.",
        });
        return json_response(StatusCode::OK, &body);
    }

    let client = brarr_tracker_newznab::NewznabClient::new(source, &row.api_token)
        .map_err(|e| AppError::InvalidInput(format!("client build failed: {e}")))?;

    let inspect = if let Some(imdb) = imdb {
        client.inspect_movie_by_imdb(imdb).await
    } else if let Some(tmdb) = tmdb {
        client.inspect_movie_by_tmdb(tmdb).await
    } else {
        unreachable!("validated above");
    };

    let inspect = inspect.map_err(|e| AppError::InvalidInput(format!("upstream: {e}")))?;
    let payload = serde_json::json!({
        "provider": {
            "id": row.id.to_string(),
            "name": row.name,
            "kind": row.kind,
            "base_url": row.base_url.to_string(),
        },
        "request": { "imdb": q.imdb, "tmdb": q.tmdb },
        "inspect": inspect,
    });
    json_response(StatusCode::OK, &payload)
}

fn json_response<T: serde::Serialize>(status: StatusCode, body: &T) -> Result<Response, AppError> {
    let bytes = serde_json::to_vec_pretty(body)?;
    let mut resp = (status, bytes).into_response();
    resp.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json; charset=utf-8"),
    );
    Ok(resp)
}

/// `POST /providers/{id}/test` — kick the provider's connectivity probe
/// and return a short HTML fragment with a status badge. Used by the
/// "Testar" button on each row in `/providers`. HTMX target is the
/// `<span class="provider-test-result-{id}">` cell on the row.
async fn providers_test(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Response, AppError> {
    let uuid = Uuid::parse_str(&id)
        .map_err(|e| AppError::InvalidInput(format!("invalid provider id: {e}")))?;
    let row = providers::get_by_id(state.pool(), uuid).await?;
    let source = brarr_core::TrackerSource::new(row.name.clone(), row.base_url.clone())
        .map_err(|e| AppError::InvalidInput(format!("invalid base_url: {e}")))?;

    let badge = run_provider_ping(&row, source).await;
    let html_fragment = render_ping_badge(&row.id.to_string(), &badge);
    let mut resp = (StatusCode::OK, html_fragment).into_response();
    resp.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/html; charset=utf-8"),
    );
    Ok(resp)
}

async fn arr_instances_index(State(state): State<AppState>) -> Result<Response, AppError> {
    let rows = arr_instances::list_all(state.pool()).await?;
    let instances = rows.into_iter().map(arr_instance_view).collect();
    html(&ArrInstancesTemplate { instances })
}

#[derive(Debug, Deserialize)]
struct CreateArrInstanceForm {
    name: String,
    kind: String,
    base_url: String,
    api_key: String,
    #[serde(default)]
    push_threshold: Option<String>,
}

async fn arr_instances_create(
    State(state): State<AppState>,
    Form(form): Form<CreateArrInstanceForm>,
) -> Result<Response, AppError> {
    let kind = match form.kind.trim().to_ascii_lowercase().as_str() {
        "sonarr" => brarr_arr::ArrKind::Sonarr,
        "radarr" => brarr_arr::ArrKind::Radarr,
        other => {
            return Err(AppError::InvalidInput(format!(
                "kind must be sonarr or radarr, got {other:?}"
            )));
        }
    };
    let url = url::Url::parse(form.base_url.trim())
        .map_err(|e| AppError::InvalidInput(format!("invalid base_url: {e}")))?;
    let push_threshold = form
        .push_threshold
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::parse::<u32>)
        .transpose()
        .map_err(|e| AppError::InvalidInput(format!("push_threshold must be 0..=1000: {e}")))?;

    arr_instances::insert(
        state.pool(),
        arr_instances::NewArrInstance {
            name: form.name.trim(),
            kind,
            base_url: &url,
            api_key: form.api_key.trim(),
            push_threshold,
            enabled: Some(true),
        },
    )
    .await?;

    let rows = arr_instances::list_all(state.pool()).await?;
    let instances = rows.into_iter().map(arr_instance_view).collect();
    html(&ArrInstancesListPartial { instances })
}

async fn arr_instances_delete(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Response, AppError> {
    let uuid = Uuid::parse_str(&id)
        .map_err(|e| AppError::InvalidInput(format!("invalid arr_instance id: {e}")))?;
    let removed = arr_instances::delete_by_id(state.pool(), uuid).await?;
    if !removed {
        return Err(AppError::NotFound(format!("arr_instance {uuid}")));
    }
    Ok((StatusCode::OK, "").into_response())
}

/// `POST /arr-instances/{id}/test` — hits the *arr's `/system/status`
/// endpoint with the configured apikey and returns a status badge.
async fn arr_instances_test(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Response, AppError> {
    let uuid = Uuid::parse_str(&id)
        .map_err(|e| AppError::InvalidInput(format!("invalid arr_instance id: {e}")))?;
    let row = arr_instances::get_by_id(state.pool(), uuid).await?;
    let inst = row.to_arr_instance();
    let badge = match brarr_arr::ArrClient::new(inst) {
        Ok(client) => match client.ping().await {
            Ok(status) => PingBadge {
                ok: true,
                label: format!("{} v{}", status.app_name, status.version),
                detail: "ok".to_string(),
            },
            Err(e) => PingBadge {
                ok: false,
                label: "erro".to_string(),
                detail: format!("{e}"),
            },
        },
        Err(e) => PingBadge {
            ok: false,
            label: "config".to_string(),
            detail: format!("client build failed: {e}"),
        },
    };
    let html_fragment = render_arr_ping_badge(&row.id.to_string(), &badge);
    let mut resp = (StatusCode::OK, html_fragment).into_response();
    resp.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/html; charset=utf-8"),
    );
    Ok(resp)
}

#[derive(Debug, Deserialize)]
struct UpdateThresholdForm {
    push_threshold: String,
}

/// `POST /arr-instances/{id}/threshold` — update push_threshold in
/// place. Refreshes the entire list partial (cheap) so the new value
/// shows everywhere.
async fn arr_instances_update_threshold(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Form(form): Form<UpdateThresholdForm>,
) -> Result<Response, AppError> {
    let uuid = Uuid::parse_str(&id)
        .map_err(|e| AppError::InvalidInput(format!("invalid arr_instance id: {e}")))?;
    let threshold: u32 = form
        .push_threshold
        .trim()
        .parse()
        .map_err(|e| AppError::InvalidInput(format!("threshold must be 0..=1000: {e}")))?;
    arr_instances::update_threshold(state.pool(), uuid, threshold).await?;
    let rows = arr_instances::list_all(state.pool()).await?;
    let instances = rows.into_iter().map(arr_instance_view).collect();
    html(&ArrInstancesListPartial { instances })
}

/// `POST /arr-instances/{id}/poll-now` — manual trigger of one
/// poll cycle for a single *arr (mirrors the scheduled poller's
/// per-instance pass). Returns a small HTML fragment with the
/// counts so HTMX can swap it into the row.
async fn arr_instances_poll_now(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Response, AppError> {
    let uuid = Uuid::parse_str(&id)
        .map_err(|e| AppError::InvalidInput(format!("invalid arr_instance id: {e}")))?;
    let row = arr_instances::get_by_id(state.pool(), uuid).await?;
    let summary = crate::poll::run_once_for_instance(&state, &row).await?;
    let html_fragment = format!(
        r#"<span id="arr-ping-{aid}" class="badge bg-blue-100 text-blue-800" title="searched {searched} of {considered} monitored movies; pushed {pushed}; {errors} search errors">{pushed} push / {searched} buscas</span>"#,
        aid = crate::web::templates::escape(&row.id.to_string()),
        searched = summary.searched,
        considered = summary.considered,
        pushed = summary.pushed,
        errors = summary.search_errors,
    );
    let mut resp = (StatusCode::OK, html_fragment).into_response();
    resp.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/html; charset=utf-8"),
    );
    Ok(resp)
}

fn render_arr_ping_badge(arr_id: &str, b: &PingBadge) -> String {
    let (bg, fg) = if b.ok {
        ("bg-emerald-100", "text-emerald-800")
    } else {
        ("bg-red-100", "text-red-800")
    };
    let detail = crate::web::templates::escape(&b.detail);
    let label = crate::web::templates::escape(&b.label);
    let aid = crate::web::templates::escape(arr_id);
    format!(r#"<span id="arr-ping-{aid}" class="badge {bg} {fg}" title="{detail}">{label}</span>"#)
}

/// `POST /decisions/{id}/push/{arr_id}` — fire-and-record a manual
/// push of one decision to one *arr instance. Returns a small HTML
/// fragment Sonarr-style (status badge) for HTMX to drop into the row.
async fn decisions_push(
    State(state): State<AppState>,
    Path((decision_id, arr_id)): Path<(String, String)>,
    headers: HeaderMap,
) -> Result<Response, AppError> {
    let decision_uuid = Uuid::parse_str(&decision_id)
        .map_err(|e| AppError::InvalidInput(format!("invalid decision id: {e}")))?;
    let arr_uuid = Uuid::parse_str(&arr_id)
        .map_err(|e| AppError::InvalidInput(format!("invalid arr_instance id: {e}")))?;
    let decision = decisions::get_by_id(state.pool(), decision_uuid).await?;
    let arr_row = arr_instances::get_by_id(state.pool(), arr_uuid).await?;
    let base_url = crate::push::derive_request_base(&headers);
    let row = crate::push::push_decision(&state, &decision, &arr_row, &base_url).await?;
    let html_fragment = render_push_badge(&decision.id.to_string(), &arr_row.id.to_string(), &row);
    let mut resp = (StatusCode::OK, html_fragment).into_response();
    resp.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/html; charset=utf-8"),
    );
    Ok(resp)
}

fn render_push_badge(
    decision_id: &str,
    arr_id: &str,
    row: &crate::db::push_history::PushHistoryRow,
) -> String {
    let (bg, fg, label) = match row.status {
        crate::db::push_history::PushStatus::Ok => ("bg-emerald-100", "text-emerald-800", "ok"),
        crate::db::push_history::PushStatus::HttpError => ("bg-red-100", "text-red-800", "http"),
        crate::db::push_history::PushStatus::TransportError => {
            ("bg-amber-100", "text-amber-800", "net")
        }
    };
    let detail = row.response_body.as_deref().unwrap_or("pushed");
    let detail = crate::web::templates::escape(detail);
    let did = crate::web::templates::escape(decision_id);
    let aid = crate::web::templates::escape(arr_id);
    let http_status = row
        .http_status
        .map_or_else(String::new, |s| format!(" {s}"));
    format!(
        r#"<span id="push-{did}-{aid}" class="badge {bg} {fg}" title="{detail}">{label}{http_status}</span>"#
    )
}

async fn pushes_index(State(state): State<AppState>) -> Result<Response, AppError> {
    let rows = push_history::recent(state.pool(), 100).await?;
    let pushes = rows.into_iter().map(push_history_view).collect();
    html(&PushesTemplate { pushes })
}

fn push_history_view(row: crate::db::push_history::PushHistoryRow) -> PushHistoryView {
    let status_label = match row.status {
        crate::db::push_history::PushStatus::Ok => "ok",
        crate::db::push_history::PushStatus::HttpError => "http_error",
        crate::db::push_history::PushStatus::TransportError => "transport_error",
    };
    PushHistoryView {
        id: row.id.to_string(),
        decision_id: row.decision_id.to_string(),
        arr_instance_name: row.arr_instance_name,
        arr_kind: row.arr_kind.label().to_string(),
        pushed_at: row
            .pushed_at
            .format(&Iso8601::DEFAULT)
            .unwrap_or_else(|_| String::from("?")),
        status: status_label.to_string(),
        http_status: row.http_status,
        response_body: row.response_body.unwrap_or_default(),
        rejections: row.rejections.unwrap_or_default(),
    }
}

fn arr_instance_view(row: crate::db::arr_instances::ArrInstanceRow) -> ArrInstanceView {
    ArrInstanceView {
        id: row.id.to_string(),
        name: row.name,
        kind: row.kind.label().to_string(),
        base_url: row.base_url.to_string(),
        push_threshold: row.push_threshold,
        enabled: row.enabled,
        created_at: row
            .created_at
            .format(&Iso8601::DEFAULT)
            .unwrap_or_else(|_| String::from("?")),
    }
}

/// Outcome of a single provider ping, normalized across provider kinds
/// so the template doesn't need to switch on which client ran.
struct PingBadge {
    ok: bool,
    label: String,
    detail: String,
}

async fn run_provider_ping(
    row: &crate::db::providers::ProviderRow,
    source: brarr_core::TrackerSource,
) -> PingBadge {
    if row.is_plugin() {
        return PingBadge {
            ok: false,
            label: "n/d".to_string(),
            detail: "test connectivity not implemented for WASM plugins".to_string(),
        };
    }
    let kind = row.kind.to_ascii_lowercase();
    if kind == "newznab" || kind == "torznab" {
        match brarr_tracker_newznab::NewznabClient::new(source, &row.api_token) {
            Ok(client) => match client.ping().await {
                Ok(r) => PingBadge {
                    ok: r.ok,
                    label: format!("{} · {}ms", r.http_status, r.elapsed_ms),
                    detail: r.detail,
                },
                Err(e) => PingBadge {
                    ok: false,
                    label: "erro".to_string(),
                    detail: format!("transport: {e}"),
                },
            },
            Err(e) => PingBadge {
                ok: false,
                label: "config".to_string(),
                detail: format!("invalid apikey or builder: {e}"),
            },
        }
    } else {
        // Default to UNIT3D for `unit3d` and any unknown kind.
        match brarr_tracker_unit3d::Unit3dClient::new(source, &row.api_token) {
            Ok(client) => match client.ping().await {
                Ok(r) => PingBadge {
                    ok: r.ok,
                    label: format!("{} · {}ms", r.http_status, r.elapsed_ms),
                    detail: r.detail,
                },
                Err(e) => PingBadge {
                    ok: false,
                    label: "erro".to_string(),
                    detail: format!("transport: {e}"),
                },
            },
            Err(e) => PingBadge {
                ok: false,
                label: "config".to_string(),
                detail: format!("invalid token or builder: {e}"),
            },
        }
    }
}

fn render_ping_badge(provider_id: &str, b: &PingBadge) -> String {
    let (bg, fg) = if b.ok {
        ("bg-emerald-100", "text-emerald-800")
    } else {
        ("bg-red-100", "text-red-800")
    };
    // Inline HTML — small enough that pulling it through Askama would
    // add more ceremony than value. Detail is escaped to keep raw error
    // text from breaking the markup. Keeping the same `id` as the
    // initial cell so HTMX's `hx-target` resolves on every subsequent
    // click after the first swap.
    let detail = crate::web::templates::escape(&b.detail);
    let label = crate::web::templates::escape(&b.label);
    let pid = crate::web::templates::escape(provider_id);
    format!(r#"<span id="ping-{pid}" class="badge {bg} {fg}" title="{detail}">{label}</span>"#)
}

async fn releases_index(State(state): State<AppState>) -> Result<Response, AppError> {
    let rows = decisions::recent(state.pool(), 50).await?;
    let decisions = rows.into_iter().map(decision_view).collect();
    // Only show buttons for *arrs that are currently enabled. Disabled
    // rows still exist in the DB (drain mode) but pushing through them
    // would silently no-op the operator's click — we'd rather hide
    // them than confuse.
    let arr_rows = arr_instances::list_enabled(state.pool()).await?;
    let arr_instances = arr_rows.into_iter().map(arr_instance_view).collect();
    html(&ReleasesTemplate {
        decisions,
        arr_instances,
    })
}

/// Multi-id form: at least one of `tmdb_id` / `imdb_id` must be set.
/// Both fields are typed `Option<String>` so an empty input doesn't
/// trip serde's `u32` parser and the handler can apply its own
/// validation with a friendly error.
#[derive(Debug, Deserialize)]
struct CreateSearchForm {
    #[serde(default)]
    tmdb_id: Option<String>,
    #[serde(default)]
    imdb_id: Option<String>,
}

async fn searches_create(
    State(state): State<AppState>,
    Form(form): Form<CreateSearchForm>,
) -> Result<Response, AppError> {
    let tmdb = parse_optional_tmdb(form.tmdb_id.as_deref())?;
    let imdb = parse_optional_imdb(form.imdb_id.as_deref())?;
    if tmdb.is_none() && imdb.is_none() {
        return Err(AppError::InvalidInput(
            "informe TMDb id ou IMDb id (tt-prefixado ou numérico)".to_string(),
        ));
    }
    let outcome =
        crate::search::run_search(&state, crate::search::SearchKeys { tmdb, imdb }).await?;
    // Return 200 (not 3xx) so the browser doesn't auto-follow the
    // Location header before HTMX can read the response. HTMX picks up
    // `HX-Redirect` from a 2xx body and performs a client-side
    // window.location navigation. When the response is a 303, XHR
    // transparently follows it via Location, the resulting page is then
    // discarded by `hx-swap="none"`, and the user is left staring at
    // the dashboard wondering why nothing happened.
    let mut headers = HeaderMap::new();
    let location = format!("/searches/{}", outcome.search.id);
    headers.insert(
        "HX-Redirect",
        HeaderValue::from_str(&location).unwrap_or_else(|_| HeaderValue::from_static("/")),
    );
    Ok((StatusCode::OK, headers, "").into_response())
}

fn parse_optional_tmdb(raw: Option<&str>) -> Result<Option<TmdbId>, AppError> {
    let Some(s) = raw.map(str::trim).filter(|s| !s.is_empty()) else {
        return Ok(None);
    };
    let n: u32 = s
        .parse()
        .map_err(|_| AppError::InvalidInput(format!("tmdb_id deve ser numérico, recebi {s:?}")))?;
    TmdbId::new(n)
        .map(Some)
        .map_err(|e| AppError::InvalidInput(format!("tmdb_id inválido: {e}")))
}

fn parse_optional_imdb(raw: Option<&str>) -> Result<Option<brarr_core::ImdbId>, AppError> {
    let Some(s) = raw.map(str::trim).filter(|s| !s.is_empty()) else {
        return Ok(None);
    };
    let stripped = s.trim_start_matches("tt").trim_start_matches('0');
    if stripped.is_empty() {
        return Ok(None);
    }
    let n: u32 = stripped
        .parse()
        .map_err(|_| AppError::InvalidInput(format!("imdb_id deve ser numérico, recebi {s:?}")))?;
    brarr_core::ImdbId::new(n)
        .map(Some)
        .map_err(|e| AppError::InvalidInput(format!("imdb_id inválido: {e}")))
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
        enabled: p.enabled,
        created_at: format_ts(p.created_at),
    }
}

/// `POST /providers/{id}/toggle` — flip enabled flag. HTMX target is
/// the whole list (cheap refresh, no per-row mutation tracking).
async fn providers_toggle(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Response, AppError> {
    let uuid = Uuid::parse_str(&id)
        .map_err(|e| AppError::InvalidInput(format!("invalid provider id: {e}")))?;
    let current = providers::get_by_id(state.pool(), uuid).await?;
    providers::set_enabled(state.pool(), uuid, !current.enabled).await?;
    let rows = providers::list_all(state.pool()).await?;
    let providers = rows.into_iter().map(provider_view).collect();
    html(&ProvidersListPartial { providers })
}

fn decision_view(d: crate::db::decisions::DecisionRow) -> DecisionView {
    let matched_rules = d.matched_rules.join(", ");
    DecisionView {
        id: d.id.to_string(),
        provider_name: d.provider_name,
        release_name: d.release_name,
        score: d.score,
        rejected: d.rejected,
        tags: d.tags.join(", "),
        matched_rules,
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
