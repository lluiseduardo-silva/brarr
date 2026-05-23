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

use std::fmt::Write as _;
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
use crate::db::quality_profiles;
use crate::db::{arr_instances, decisions, providers, push_history, searches};
#[allow(
    unused_imports,
    reason = "re-exported for downstream tests that still call it"
)]
use crate::search::run_tmdb_search;
use crate::web::render::html;
use crate::web::templates::{
    ArrInstanceView, ArrInstancesListPartial, ArrInstancesTemplate, DashboardTemplate,
    DecisionView, ErrorTemplate, LoginTemplate, NewProfileModalPartial, NewSearchModalPartial,
    ProfileEditorTemplate, ProfileView, ProfilesTemplate, ProviderView, ProvidersListPartial,
    ProvidersTemplate, PushGroupView, PushHistoryView, PushesTemplate, RecentSearchView,
    ReleasesTemplate, SearchDetailTemplate,
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
        .route("/profiles", get(profiles_index).post(profiles_create))
        .route("/profiles/new", get(profiles_new_modal))
        .route(
            "/profiles/{id}",
            delete(profiles_delete).put(profiles_update),
        )
        .route("/profiles/{id}/edit", get(profiles_edit))
        .route("/profiles/{id}/preview", post(profiles_preview))
        .route("/releases", get(releases_index))
        .route("/searches", post(searches_create))
        .route("/searches/new", get(new_search_modal))
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
        // Branded 404. Without this axum returns a bare `Nothing
        // matched` text body; the fallback lets us reuse the same
        // template that powers other error surfaces.
        .fallback(not_found)
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}

/// 404 handler — wired as the router's `.fallback`. Returns the
/// branded error template with HTTP 404.
async fn not_found() -> Result<Response, AppError> {
    let mut resp = html(&ErrorTemplate {
        code: "404".to_string(),
        title: "Página não encontrada".to_string(),
        message: "A rota que você acessou não existe ou foi movida.\n\
                  Talvez você esteja procurando uma busca antiga que já foi limpa do histórico."
            .to_string(),
    })?;
    *resp.status_mut() = StatusCode::NOT_FOUND;
    Ok(resp)
}

/// Middleware that gates every protected route on the auth cookie.
/// When `AuthConfig::Disabled` is in effect it always passes through.
///
/// Bypass: if the caller IP (direct peer, or the original client when
/// the peer is a trusted reverse proxy) matches a rule in
/// `BypassConfig::peers`, auth is skipped. This is logged at `info!`
/// so the bypass is auditable.
async fn auth_middleware(
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
            "auth bypass via trusted peer"
        );
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
    // `into_make_service_with_connect_info` attaches a
    // `ConnectInfo<SocketAddr>` extension to every request so the
    // bypass middleware can see the actual peer (or, when wired with a
    // trusted proxy, the original client via `X-Forwarded-For`).
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .await
    .map_err(AppError::Io)?;
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

    let profile_names = profile_name_map(pool).await?;
    let recent_decisions = recent_decision_rows
        .into_iter()
        .filter(|d| !d.rejected)
        .map(|d| decision_view(d, &profile_names))
        .collect();

    let (push_total, push_ok) = push_history::success_rate(pool).await?;

    let tmpl = DashboardTemplate {
        provider_count: provider_rows.len(),
        search_count: searches::recent(pool, 200).await?.len(),
        push_total,
        push_ok,
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
    let profile_rows = quality_profiles::list_all(state.pool()).await?;
    let profile_by_id: std::collections::HashMap<
        Uuid,
        &crate::db::quality_profiles::QualityProfileRow,
    > = profile_rows.iter().map(|p| (p.id, p)).collect();
    let instances = rows
        .iter()
        .map(|r| arr_instance_view_with_profile(r, &profile_by_id))
        .collect();
    let profiles = profile_rows
        .iter()
        .map(|p| ProfileView {
            id: p.id.to_string(),
            name: p.name.clone(),
            description: p.description.clone(),
            push_threshold: p.push_threshold,
            is_preset: p.is_preset,
        })
        .collect();
    html(&ArrInstancesTemplate {
        instances,
        profiles,
    })
}

fn arr_instance_view_with_profile(
    row: &crate::db::arr_instances::ArrInstanceRow,
    profiles: &std::collections::HashMap<Uuid, &crate::db::quality_profiles::QualityProfileRow>,
) -> ArrInstanceView {
    let mut v = arr_instance_view(row.clone());
    if let Some(pid) = row.profile_id {
        if let Some(p) = profiles.get(&pid) {
            v.profile_name = Some(p.name.clone());
            v.profile_threshold = Some(p.push_threshold);
        }
    }
    v
}

#[derive(Debug, Deserialize)]
struct CreateArrInstanceForm {
    name: String,
    kind: String,
    base_url: String,
    api_key: String,
    #[serde(default)]
    push_threshold: Option<String>,
    #[serde(default)]
    profile_id: Option<String>,
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

    let profile_id = form
        .profile_id
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(Uuid::parse_str)
        .transpose()
        .map_err(|e| AppError::InvalidInput(format!("profile_id deve ser uuid: {e}")))?;

    arr_instances::insert(
        state.pool(),
        arr_instances::NewArrInstance {
            name: form.name.trim(),
            kind,
            base_url: &url,
            api_key: form.api_key.trim(),
            push_threshold,
            profile_id,
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
    let rows = push_history::recent(state.pool(), 500).await?;
    let groups = group_pushes(rows);
    html(&PushesTemplate { groups })
}

/// Group flat push_history rows by (release_name, arr_instance_name)
/// so repeat attempts on the same content cluster under one header
/// in the UI. Order: groups by latest attempt DESC, attempts inside
/// each group newest-first.
fn group_pushes(rows: Vec<crate::db::push_history::PushHistoryRow>) -> Vec<PushGroupView> {
    use std::collections::BTreeMap;

    // Key preserves first-seen order; we rebuild order at the end by
    // the freshest attempt timestamp per group.
    let mut by_key: BTreeMap<(String, String), Vec<crate::db::push_history::PushHistoryRow>> =
        BTreeMap::new();
    for row in rows {
        let key = (row.release_name.clone(), row.arr_instance_name.clone());
        by_key.entry(key).or_default().push(row);
    }
    let mut groups: Vec<PushGroupView> = by_key
        .into_iter()
        .map(|((release_name, arr_name), mut attempts)| {
            // Newest attempt first inside the group.
            attempts.sort_by_key(|a| std::cmp::Reverse(a.pushed_at));
            let attempt_count = attempts.len();
            let latest = attempts
                .iter()
                .map(|a| a.pushed_at)
                .max()
                .unwrap_or_else(brarr_core::OffsetDateTime::now_utc);
            let any_ok = attempts
                .iter()
                .any(|a| matches!(a.status, crate::db::push_history::PushStatus::Ok));
            let provider_name = attempts
                .first()
                .map(|a| a.provider_name.clone())
                .unwrap_or_default();
            let arr_kind = attempts
                .first()
                .map(|a| a.arr_kind.label().to_string())
                .unwrap_or_default();
            let attempts = attempts.into_iter().map(push_history_view).collect();
            PushGroupView {
                release_name,
                provider_name,
                arr_name,
                arr_kind,
                attempt_count,
                latest_at: latest
                    .format(&Iso8601::DEFAULT)
                    .unwrap_or_else(|_| String::from("?")),
                latest_at_unix: latest.unix_timestamp(),
                any_ok,
                attempts,
            }
        })
        .collect();
    // Freshest group first.
    groups.sort_by_key(|g| std::cmp::Reverse(g.latest_at_unix));
    groups
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
        profile_name: None,
        profile_threshold: None,
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
    let profile_names = profile_name_map(state.pool()).await?;
    let decisions = rows
        .into_iter()
        .map(|d| decision_view(d, &profile_names))
        .collect();
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

/// Multi-id form: at least one of `tmdb_id` / `imdb_id` / `tvdb_id`
/// must be set. Every field is typed `Option<String>` so an empty
/// input doesn't trip serde's `u32` parser and the handler can apply
/// its own validation with a friendly error.
#[derive(Debug, Deserialize)]
struct CreateSearchForm {
    #[serde(default)]
    tmdb_id: Option<String>,
    #[serde(default)]
    imdb_id: Option<String>,
    #[serde(default)]
    tvdb_id: Option<String>,
    #[serde(default)]
    season: Option<String>,
    #[serde(default)]
    episode: Option<String>,
    /// Optional Quality Profile UUID — when set, the post-search
    /// redirect carries `?profile=<uuid>` so the detail page renders
    /// scores under that profile's engine. Empty string = baseline.
    #[serde(default)]
    profile_id: Option<String>,
}

#[allow(
    clippy::similar_names,
    reason = "tmdb/imdb/tvdb are the canonical 4-letter ID names; renaming any one of them \
              would obscure which provider axis the value comes from"
)]
async fn searches_create(
    State(state): State<AppState>,
    Form(form): Form<CreateSearchForm>,
) -> Result<Response, AppError> {
    let tmdb = parse_optional_tmdb(form.tmdb_id.as_deref())?;
    let imdb = parse_optional_imdb(form.imdb_id.as_deref())?;
    let tvdb = parse_optional_tvdb(form.tvdb_id.as_deref())?;
    let season = parse_optional_u16(form.season.as_deref(), "season")?;
    let episode = parse_optional_u16(form.episode.as_deref(), "episode")?;
    if tmdb.is_none() && imdb.is_none() && tvdb.is_none() {
        return Err(AppError::InvalidInput(
            "informe TMDb id, IMDb id (tt-prefixado ou numérico) ou TVDB id".to_string(),
        ));
    }
    let outcome = crate::search::run_search(
        &state,
        crate::search::SearchKeys {
            tmdb,
            imdb,
            tvdb,
            season,
            episode,
        },
    )
    .await?;
    // Return 200 (not 3xx) so the browser doesn't auto-follow the
    // Location header before HTMX can read the response. HTMX picks up
    // `HX-Redirect` from a 2xx body and performs a client-side
    // window.location navigation. When the response is a 303, XHR
    // transparently follows it via Location, the resulting page is then
    // discarded by `hx-swap="none"`, and the user is left staring at
    // the dashboard wondering why nothing happened.
    let mut headers = HeaderMap::new();
    let profile_qs = form
        .profile_id
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .and_then(|s| Uuid::parse_str(s).ok())
        .map(|u| format!("?profile={u}"))
        .unwrap_or_default();
    let location = format!("/searches/{}{}", outcome.search.id, profile_qs);
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

fn parse_optional_tvdb(raw: Option<&str>) -> Result<Option<brarr_core::TvdbId>, AppError> {
    let Some(s) = raw.map(str::trim).filter(|s| !s.is_empty()) else {
        return Ok(None);
    };
    let n: u32 = s
        .parse()
        .map_err(|_| AppError::InvalidInput(format!("tvdb_id deve ser numérico, recebi {s:?}")))?;
    brarr_core::TvdbId::new(n)
        .map(Some)
        .map_err(|e| AppError::InvalidInput(format!("tvdb_id inválido: {e}")))
}

fn parse_optional_u16(raw: Option<&str>, label: &str) -> Result<Option<u16>, AppError> {
    let Some(s) = raw.map(str::trim).filter(|s| !s.is_empty()) else {
        return Ok(None);
    };
    s.parse::<u16>().map(Some).map_err(|_| {
        AppError::InvalidInput(format!(
            "{label} deve ser numérico (0..=65535), recebi {s:?}"
        ))
    })
}

/// Returns the Nova Busca modal partial — swapped into the
/// `#modal-target` slot by HTMX on the Dashboard/Releases CTA. The
/// partial is just a <dialog> + form; modal.js calls `.showModal()`
/// once the swap lands. The form posts back to `/searches`, which
/// already issues `HX-Redirect` to the new detail page.
async fn new_search_modal(State(state): State<AppState>) -> Result<Response, AppError> {
    let provider_count = providers::list_all(state.pool())
        .await?
        .into_iter()
        .filter(|p| p.enabled)
        .count();
    let profiles = quality_profiles::list_all(state.pool())
        .await?
        .into_iter()
        .map(|p| ProfileView {
            id: p.id.to_string(),
            name: p.name,
            description: p.description,
            push_threshold: p.push_threshold,
            is_preset: p.is_preset,
        })
        .collect();
    html(&NewSearchModalPartial {
        provider_count,
        profiles,
    })
}

// ─── Quality Profiles ─────────────────────────────────────────────

async fn profiles_index(State(state): State<AppState>) -> Result<Response, AppError> {
    let rows = quality_profiles::list_all(state.pool()).await?;
    let profiles = rows
        .into_iter()
        .map(|p| ProfileView {
            id: p.id.to_string(),
            name: p.name,
            description: p.description,
            push_threshold: p.push_threshold,
            is_preset: p.is_preset,
        })
        .collect();
    html(&ProfilesTemplate { profiles })
}

/// Returns the create-profile dialog partial. Modal.js opens it once
/// HTMX swaps it into `#modal-target`.
async fn profiles_new_modal() -> Result<Response, AppError> {
    html(&NewProfileModalPartial)
}

#[derive(Debug, Deserialize)]
struct CreateProfileForm {
    name: String,
    description: Option<String>,
    push_threshold: u32,
}

/// Create a new profile. On success returns an empty body + a
/// `HX-Redirect: /profiles` header so HTMX reloads the index with the
/// new row visible.
async fn profiles_create(
    State(state): State<AppState>,
    Form(form): Form<CreateProfileForm>,
) -> Result<Response, AppError> {
    let description = form
        .description
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    quality_profiles::insert(
        state.pool(),
        quality_profiles::NewQualityProfile {
            name: form.name.trim(),
            description,
            push_threshold: form.push_threshold,
        },
    )
    .await?;
    let mut resp = Response::new(axum::body::Body::empty());
    // `HeaderValue::from_static` is infallible for ASCII string
    // literals at runtime; the compiler still emits a const-eval
    // assertion. Avoids the `.expect` / `.unwrap` lints.
    resp.headers_mut()
        .insert("HX-Redirect", HeaderValue::from_static("/profiles"));
    Ok(resp)
}

async fn profiles_delete(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Response, AppError> {
    let uuid = Uuid::parse_str(&id)
        .map_err(|e| AppError::InvalidInput(format!("invalid profile id: {e}")))?;
    quality_profiles::delete_by_id(state.pool(), uuid).await?;
    // Empty body — HTMX swaps the row's #profile-{id} card out, which
    // visually removes the card without a full page reload.
    Ok(Response::new(axum::body::Body::empty()))
}

/// `GET /profiles/{id}/edit` — full-page editor (no HTMX modal). Shows
/// identity + threshold + rule JSON textarea so an operator can tweak
/// scoring without leaving the admin UI.
async fn profiles_edit(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Response, AppError> {
    let uuid = Uuid::parse_str(&id)
        .map_err(|e| AppError::InvalidInput(format!("invalid profile id: {e}")))?;
    let row = quality_profiles::get_by_id(state.pool(), uuid).await?;
    let rules_json = serde_json::to_string_pretty(&row.rules)?;
    html(&ProfileEditorTemplate {
        id: row.id.to_string(),
        name: row.name,
        description: row.description.unwrap_or_default(),
        push_threshold: row.push_threshold,
        is_preset: row.is_preset,
        rules_json,
        error_message: None,
        preview_html: "Clique avaliar pra ver o score que o engine produziria.".to_string(),
    })
}

#[derive(Debug, Deserialize)]
struct UpdateProfileForm {
    name: String,
    description: Option<String>,
    push_threshold: u32,
    rules_json: String,
}

#[derive(Debug, Deserialize)]
struct PreviewProfileForm {
    /// Only the rules textarea content matters for preview — the rest
    /// of the form is intentionally ignored so the operator can iterate
    /// on rules without committing identity/threshold changes.
    #[serde(default)]
    rules_json: String,
}

/// `POST /profiles/{id}/preview` — evaluate the in-flight rule list
/// (from the form, **not** the persisted row) against three reference
/// release fixtures and return an HTML breakdown. Lets the operator
/// see the score impact of a rule change before clicking Save.
///
/// Fixtures cover the canonical brarr use cases:
///   1. **PT-BR Dub 1080p WEB-DL** — the bread-and-butter Radarr push.
///   2. **Anime original JP + legenda PT-BR 1080p** — the case that
///      motivated the rule builder in the first place.
///   3. **EN-only 2160p HDR BluRay** — Premium tier without dub;
///      surfaces whether the rules accidentally over-reward HDR.
async fn profiles_preview(
    State(_state): State<AppState>,
    Path(_id): Path<String>,
    Form(form): Form<PreviewProfileForm>,
) -> Result<Response, AppError> {
    let rules_result: Result<brarr_decision_service::RuleSet, _> =
        serde_json::from_str(&form.rules_json);
    let engine = match rules_result {
        Ok(r) => brarr_decision_service::Engine::from_profile_rules(r),
        Err(e) => {
            return Ok(html_string(format!(
                r#"<p class="text-sm text-danger-soft-fg">JSON inválido: {}</p>"#,
                crate::web::templates::escape(&e.to_string())
            )));
        }
    };

    let fixtures = preview_fixtures()?;
    let mut buf = String::new();
    buf.push_str(r#"<div class="flex flex-col gap-3">"#);
    for (label, release) in fixtures {
        let outcome = engine.evaluate(&release);
        let badge_class = if outcome.rejected {
            "bg-danger-soft text-danger-soft-fg"
        } else if outcome.score.get() >= 150 {
            "bg-success-soft text-success-soft-fg"
        } else {
            "bg-bg-muted text-fg-secondary"
        };
        let verdict = if outcome.rejected { "rejected" } else { "kept" };
        let mut rules_block = String::new();
        if outcome.matched_rules.is_empty() {
            rules_block
                .push_str(r#"<span class="italic text-fg-muted">— nenhuma regra casou</span>"#);
        } else {
            rules_block.push_str(r#"<ul class="mt-1 space-y-0.5">"#);
            for r in &outcome.matched_rules {
                let _ = write!(
                    rules_block,
                    r#"<li class="text-[11px] font-mono text-fg-secondary">{}</li>"#,
                    crate::web::templates::escape(r),
                );
            }
            rules_block.push_str("</ul>");
        }
        let _ = write!(
            buf,
            r#"<div class="rounded-md border border-border-default p-3 bg-bg-canvas">
                <div class="flex items-center justify-between gap-2 mb-1">
                    <span class="text-xs font-semibold text-fg-primary truncate">{label}</span>
                    <span class="inline-flex items-center gap-1.5 px-2 py-0.5 rounded text-[10px] font-semibold uppercase tracking-[0.06em] {badge_class}">{verdict} · {score}</span>
                </div>
                {rules_block}
            </div>"#,
            label = crate::web::templates::escape(label),
            score = outcome.score.get(),
        );
    }
    buf.push_str("</div>");
    Ok(html_string(buf))
}

fn html_string(body: String) -> Response {
    let mut resp = Response::new(axum::body::Body::from(body));
    resp.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/html; charset=utf-8"),
    );
    resp
}

/// Static set of release fixtures the live preview evaluates against.
/// Each entry mirrors a real-world brarr use case so an operator
/// editing rules sees concrete deltas instead of abstract numbers.
fn preview_fixtures() -> Result<Vec<(&'static str, brarr_core::Release)>, AppError> {
    use brarr_core::{
        Language, Release, ReleaseEnrichment, ReleaseKind, Resolution, TrackerSource,
    };
    let tracker = TrackerSource::new(
        "capybara",
        url::Url::parse("https://capybarabr.com/api/")
            .map_err(|e| AppError::InvalidInput(format!("preview tracker URL: {e}")))?,
    )
    .map_err(|e| AppError::InvalidInput(format!("preview tracker: {e}")))?;
    let make = |title: &'static str,
                kind: ReleaseKind,
                resolution: Resolution,
                audio: Vec<Language>,
                subtitle: Vec<Language>,
                has_hdr: bool,
                seeders: u32|
     -> Result<Release, AppError> {
        let mut r = Release::new("0", tracker.clone(), title, kind, resolution, 5_000_000_000)
            .map_err(|e| AppError::InvalidInput(format!("preview release {title}: {e}")))?;
        r.seeders = seeders;
        r.enrichment = Some(ReleaseEnrichment {
            audio_languages: audio,
            subtitle_languages: subtitle,
            has_hdr,
            ..ReleaseEnrichment::default()
        });
        Ok(r)
    };

    Ok(vec![
        (
            "PT-BR Dub · 1080p WEB-DL",
            make(
                "The Matrix 1999 1080p WEB-DL DD5.1 H.264-NeX",
                ReleaseKind::WebDl,
                Resolution::P1080,
                vec![Language::PtBr, Language::En],
                vec![Language::PtBr],
                false,
                40,
            )?,
        ),
        (
            "Anime JP + leg PT-BR · 1080p",
            make(
                "Steins;Gate S01E01 1080p BluRay x264-NIPPON",
                ReleaseKind::BluRay,
                Resolution::P1080,
                vec![Language::Jp],
                vec![Language::PtBr],
                false,
                12,
            )?,
        ),
        (
            "EN-only · 2160p HDR BluRay",
            make(
                "Dune 2021 2160p UHD BluRay x265 HDR-FraMeSToR",
                ReleaseKind::BluRay,
                Resolution::P2160,
                vec![Language::En],
                vec![Language::En],
                true,
                3,
            )?,
        ),
    ])
}

/// `PUT /profiles/{id}` — persist editor changes. Validates the rule
/// JSON against the `RuleSet` schema before the DB write so a typo
/// surfaces as a banner instead of corrupting the row.
async fn profiles_update(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Form(form): Form<UpdateProfileForm>,
) -> Result<Response, AppError> {
    let uuid = Uuid::parse_str(&id)
        .map_err(|e| AppError::InvalidInput(format!("invalid profile id: {e}")))?;
    // Parse-validate the JSON first so we don't half-commit the row
    // when the rule list is malformed.
    let rules: brarr_decision_service::RuleSet = match serde_json::from_str(&form.rules_json) {
        Ok(r) => r,
        Err(e) => {
            let row = quality_profiles::get_by_id(state.pool(), uuid).await?;
            return html(&ProfileEditorTemplate {
                id: row.id.to_string(),
                name: form.name,
                description: form.description.unwrap_or_default(),
                push_threshold: form.push_threshold,
                is_preset: row.is_preset,
                rules_json: form.rules_json,
                error_message: Some(format!("JSON inválido: {e}")),
                preview_html: String::new(),
            });
        }
    };
    let description = form
        .description
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    quality_profiles::update_basics(
        state.pool(),
        uuid,
        form.name.trim(),
        description,
        form.push_threshold,
    )
    .await?;
    quality_profiles::update_rules(state.pool(), uuid, &rules).await?;
    let mut resp = Response::new(axum::body::Body::empty());
    resp.headers_mut()
        .insert("HX-Redirect", HeaderValue::from_static("/profiles"));
    Ok(resp)
}

#[derive(Debug, Deserialize)]
struct SearchDetailQuery {
    /// Optional profile UUID — when supplied, decision_view scores
    /// against this profile's persisted score map instead of taking
    /// the max-across-all-profiles default. Carries baseline as a
    /// secondary annotation so the operator can compare deltas.
    #[serde(default)]
    profile: Option<String>,
}

async fn search_detail(
    State(state): State<AppState>,
    Path(id): Path<String>,
    axum::extract::Query(q): axum::extract::Query<SearchDetailQuery>,
) -> Result<Response, AppError> {
    let uuid = Uuid::parse_str(&id)
        .map_err(|e| AppError::InvalidInput(format!("invalid search id: {e}")))?;
    let search = searches::get_by_id(state.pool(), uuid).await?;
    let decisions_rows = decisions::list_for_search(state.pool(), uuid).await?;
    let profile_names = profile_name_map(state.pool()).await?;
    let preferred_profile: Option<Uuid> = q
        .profile
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .and_then(|s| Uuid::parse_str(s).ok());
    let decisions = decisions_rows
        .into_iter()
        .map(|d| decision_view_with_profile(d, &profile_names, preferred_profile))
        .collect();
    let arr_rows = arr_instances::list_enabled(state.pool()).await?;
    let arr_instances = arr_rows.into_iter().map(arr_instance_view).collect();
    let tmpl = SearchDetailTemplate {
        id: search.id.to_string(),
        tmdb_id: search
            .tmdb_id
            .map_or_else(|| "-".to_string(), |v| v.to_string()),
        submitted_at: format_ts(search.submitted_at),
        decisions,
        arr_instances,
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

fn decision_view(
    d: crate::db::decisions::DecisionRow,
    profile_names: &std::collections::HashMap<Uuid, String>,
) -> DecisionView {
    decision_view_with_profile(d, profile_names, None)
}

/// Build a [`DecisionView`] honouring an operator-picked profile lock.
///
/// When `preferred_profile` is `None`, the displayed score is the
/// max-across-baseline-and-every-profile (sensible default for the
/// dashboard / releases / search detail without `?profile=`).
///
/// When `preferred_profile` is `Some(id)`, the score is *strictly* the
/// chosen profile's output — even if it scores lower than baseline.
/// That's the whole point of the profile lock: the operator wants to
/// see what THIS rule list produces, not the best-of-N.
fn decision_view_with_profile(
    d: crate::db::decisions::DecisionRow,
    profile_names: &std::collections::HashMap<Uuid, String>,
    preferred_profile: Option<Uuid>,
) -> DecisionView {
    let rule_chips: Vec<(String, String)> = d
        .matched_rules
        .iter()
        .map(|name| (name.clone(), classify_rule_chip(name).to_string()))
        .collect();
    let matched_rules = d.matched_rules.join(", ");
    let audio_chips = audio_chips_from_languages(&d.audio_languages, &d.subtitle_languages);
    let subtitle_chips = subtitle_chips_from_languages(&d.subtitle_languages);
    let provider_initial = first_alpha_initial(&d.provider_name);
    let age = humanize_age(d.decided_at);
    let baseline_score = d.score;
    let (display_score, winning_profile_id, profile_locked) = match preferred_profile {
        Some(pid) => {
            // Profile lock: read the exact persisted score for that
            // profile (falls back to baseline if the search ran before
            // the profile existed — search rows aren't retroactively
            // re-scored). Always set the winning_profile name so the
            // template surfaces which lens the operator's looking
            // through, even when the profile ties or loses to baseline.
            let pscore = d
                .profile_scores
                .get(&pid)
                .copied()
                .unwrap_or(baseline_score);
            (pscore, Some(pid), true)
        }
        None => d
            .profile_scores
            .iter()
            .max_by_key(|&(_, score)| *score)
            .filter(|&(_, score)| *score > baseline_score)
            .map_or((baseline_score, None, false), |(id, score)| {
                (*score, Some(*id), false)
            }),
    };
    let winning_profile = winning_profile_id.and_then(|id| profile_names.get(&id).cloned());
    DecisionView {
        id: d.id.to_string(),
        provider_name: d.provider_name,
        release_name: d.release_name,
        score: display_score,
        baseline_score,
        winning_profile,
        profile_locked,
        rejected: d.rejected,
        tags: d.tags.join(", "),
        matched_rules,
        rule_chips,
        audio_chips,
        subtitle_chips,
        resolution: d.resolution,
        kind: d.kind,
        seeders: d.seeders,
        size_human: humanize_bytes(d.size_bytes),
        provider_initial,
        age,
    }
}

/// Load every quality-profile name keyed by id. Used by handlers that
/// build a `DecisionView`: passing a pre-loaded map keeps decision_view
/// synchronous and avoids per-row DB queries.
async fn profile_name_map(
    pool: &crate::db::Pool,
) -> Result<std::collections::HashMap<Uuid, String>, AppError> {
    let rows = crate::db::quality_profiles::list_all(pool).await?;
    Ok(rows.into_iter().map(|p| (p.id, p.name)).collect())
}

/// First ASCII alphanumeric of `s`, uppercased. Falls back to `?` for
/// blank or punctuation-only names so the header chip always has a
/// visible mark. Non-ASCII letters get normalised to `?` rather than
/// risking a multi-codepoint badge that breaks the fixed-size circle.
fn first_alpha_initial(s: &str) -> String {
    for ch in s.chars() {
        if ch.is_ascii_alphanumeric() {
            return ch.to_ascii_uppercase().to_string();
        }
    }
    "?".to_string()
}

/// Format a decision timestamp as `"há N {unidade}"` in pt-BR — the
/// release card subtitle scans best when the operator can see at a
/// glance whether a row is hours or days old. Anything beyond a year
/// is rounded down to years; anything in the future (clock skew)
/// returns an empty string so the template can hide the line entirely.
fn humanize_age(decided_at: OffsetDateTime) -> String {
    let now = OffsetDateTime::now_utc();
    if decided_at > now {
        return String::new();
    }
    let elapsed = now - decided_at;
    let secs = elapsed.whole_seconds();
    if secs < 60 {
        return "agora".to_string();
    }
    let minutes = secs / 60;
    if minutes < 60 {
        return format!("há {minutes} min");
    }
    let hours = minutes / 60;
    if hours < 24 {
        return format!("há {hours} {}", if hours == 1 { "hora" } else { "horas" });
    }
    let days = hours / 24;
    if days < 30 {
        return format!("há {days} {}", if days == 1 { "dia" } else { "dias" });
    }
    let months = days / 30;
    if months < 12 {
        return format!("há {months} {}", if months == 1 { "mês" } else { "meses" });
    }
    let years = months / 12;
    format!("há {years} {}", if years == 1 { "ano" } else { "anos" })
}

/// Build explicit audio chips from the persisted enrichment snapshot.
///
/// Renders independent of the rule engine: even profiles with zero
/// Portuguese rules still surface a `PT-BR áudio` chip when the
/// release ships it. Ordering matches the audio track order in the
/// MediaInfo dump, with duplicates de-duplicated and an extra
/// `Dublado` / `Multi-áudio` / `JP áudio + leg PT` annotation appended
/// based on the combined audio + subtitle shape so anime, dubs, and
/// multi-language rips read at a glance.
fn audio_chips_from_languages(
    audio: &[brarr_core::Language],
    subtitle: &[brarr_core::Language],
) -> Vec<(String, String)> {
    use brarr_core::Language;
    let mut out = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for lang in audio {
        if !seen.insert(lang.clone()) {
            continue;
        }
        let chip = match lang {
            Language::PtBr => Some(("PT-BR áudio".to_string(), "pt".to_string())),
            Language::PtPt => Some(("PT-PT áudio".to_string(), "pt".to_string())),
            Language::Pt => Some(("PT áudio".to_string(), "pt".to_string())),
            Language::En => Some(("EN áudio".to_string(), "neutral".to_string())),
            Language::Jp => Some(("JP áudio".to_string(), "accent".to_string())),
            Language::Zh => Some(("ZH áudio".to_string(), "accent".to_string())),
            Language::Other(name) => Some((format!("{name} áudio"), "neutral".to_string())),
        };
        if let Some(c) = chip {
            out.push(c);
        }
    }
    // Composite annotations — appended last so explicit per-language
    // chips read first.
    let has_pt_audio = audio.iter().any(Language::is_portuguese);
    let has_pt_subtitle = subtitle.iter().any(Language::is_portuguese);
    let has_non_pt_audio = audio.iter().any(|l| {
        matches!(
            l,
            Language::En | Language::Jp | Language::Zh | Language::Other(_)
        )
    });
    if has_pt_audio && has_non_pt_audio {
        out.push(("Dublado".to_string(), "accent".to_string()));
    }
    // Anime case: non-PT audio (typically Japanese) + PT subtitle and no
    // PT audio at all → reads as legendado.
    if !has_pt_audio && has_non_pt_audio && has_pt_subtitle {
        out.push(("Legendado".to_string(), "accent".to_string()));
    }
    let unique_non_pt = {
        let mut s = std::collections::HashSet::new();
        for l in audio {
            if !l.is_portuguese() {
                s.insert(l.clone());
            }
        }
        s.len()
    };
    if unique_non_pt >= 2 {
        out.push(("Multi-áudio".to_string(), "warning".to_string()));
    }
    out
}

/// Build explicit subtitle chips. Same idea as
/// [`audio_chips_from_languages`] but a track without Portuguese audio
/// already carries the `Legendado` accent on the audio row, so subtitle
/// chips stay purely descriptive.
fn subtitle_chips_from_languages(subtitle: &[brarr_core::Language]) -> Vec<(String, String)> {
    use brarr_core::Language;
    let mut out = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for lang in subtitle {
        if !seen.insert(lang.clone()) {
            continue;
        }
        let chip = match lang {
            Language::PtBr => Some(("PT-BR legenda".to_string(), "pt".to_string())),
            Language::PtPt => Some(("PT-PT legenda".to_string(), "pt".to_string())),
            Language::Pt => Some(("PT legenda".to_string(), "pt".to_string())),
            Language::En => Some(("EN legenda".to_string(), "neutral".to_string())),
            Language::Jp => Some(("JP legenda".to_string(), "neutral".to_string())),
            Language::Zh => Some(("ZH legenda".to_string(), "neutral".to_string())),
            Language::Other(name) => Some((format!("{name} legenda"), "neutral".to_string())),
        };
        if let Some(c) = chip {
            out.push(c);
        }
    }
    out
}

/// Map a rule name to the chip colour kind the release card uses.
/// Heuristic — looks for substrings in the rule's display name. We
/// don't store per-rule metadata in the decision row (would balloon
/// the schema for a UI hint), so name-matching is the lightest
/// approach. Unknown rules fall through to `neutral`.
fn classify_rule_chip(name: &str) -> &'static str {
    let lower = name.to_lowercase();
    if lower.contains("pt")
        || lower.contains("portug")
        || lower.contains("legenda")
        || lower.contains("dublag")
    {
        "pt"
    } else if lower.contains("resol")
        || lower.contains("1080")
        || lower.contains("2160")
        || lower.contains("720")
        || lower.contains("hdr")
        || lower.contains("4k")
    {
        "accent"
    } else if lower.contains("seed") || lower.contains("idade") || lower.contains("age") {
        "warning"
    } else {
        "neutral"
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

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::{audio_chips_from_languages, subtitle_chips_from_languages};
    use brarr_core::Language;

    #[test]
    fn pt_br_audio_renders_as_green_pt_chip() {
        let chips = audio_chips_from_languages(&[Language::PtBr], &[]);
        assert_eq!(chips, vec![("PT-BR áudio".to_string(), "pt".to_string())]);
    }

    #[test]
    fn pt_br_audio_plus_english_audio_appends_dublado() {
        let chips = audio_chips_from_languages(&[Language::PtBr, Language::En], &[]);
        assert_eq!(
            chips,
            vec![
                ("PT-BR áudio".to_string(), "pt".to_string()),
                ("EN áudio".to_string(), "neutral".to_string()),
                ("Dublado".to_string(), "accent".to_string()),
            ]
        );
    }

    #[test]
    fn jp_audio_with_pt_subtitle_marks_legendado() {
        let chips = audio_chips_from_languages(
            &[Language::Other("Japanese".to_string())],
            &[Language::PtBr],
        );
        assert_eq!(
            chips,
            vec![
                ("Japanese áudio".to_string(), "neutral".to_string()),
                ("Legendado".to_string(), "accent".to_string()),
            ]
        );
    }

    #[test]
    fn three_distinct_non_pt_audios_flag_multi_audio() {
        let chips = audio_chips_from_languages(
            &[
                Language::En,
                Language::Other("Spanish".to_string()),
                Language::Other("French".to_string()),
            ],
            &[],
        );
        assert!(
            chips.iter().any(|c| c.0 == "Multi-áudio"),
            "expected Multi-áudio chip in {chips:?}"
        );
    }

    #[test]
    fn duplicate_audio_languages_deduped() {
        let chips = audio_chips_from_languages(&[Language::PtBr, Language::PtBr], &[]);
        assert_eq!(chips, vec![("PT-BR áudio".to_string(), "pt".to_string())]);
    }

    #[test]
    fn subtitle_chips_render_pt_explicitly() {
        let chips = subtitle_chips_from_languages(&[Language::PtBr, Language::En]);
        assert_eq!(
            chips,
            vec![
                ("PT-BR legenda".to_string(), "pt".to_string()),
                ("EN legenda".to_string(), "neutral".to_string()),
            ]
        );
    }

    #[test]
    fn empty_enrichment_produces_no_chips() {
        assert!(audio_chips_from_languages(&[], &[]).is_empty());
        assert!(subtitle_chips_from_languages(&[]).is_empty());
    }
}
