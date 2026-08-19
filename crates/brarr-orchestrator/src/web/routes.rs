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

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use askama::Template as _;
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
use crate::auth::{BypassConfig, TrustedPeers};
use crate::db::quality_profiles;
use crate::db::settings;
use crate::db::{
    arr_instances, decisions, download_clients, grabs, item_ids, library, media_server_mappings,
    media_servers, path_mappings, providers, push_history, root_folders, searches,
};
use crate::metadata::art;
use crate::metadata::registry::Registry;
use crate::scan::ScanProgress;
#[allow(
    unused_imports,
    reason = "re-exported for downstream tests that still call it"
)]
use crate::search::run_tmdb_search;
use crate::structure;
use crate::web::render::html;
use crate::web::templates::{
    AddOptionFolder, AddOptionProfile, ArrImportBodyPartial, ArrImportReportPartial,
    ArrImportRootView, ArrImportTemplate, ArrImportTitleView, ArrInstanceView,
    ArrInstancesListPartial, ArrInstancesTemplate, ArrRootOption, DashboardTemplate, DecisionView,
    DownloadClientView, DownloadClientsListPartial, DownloadClientsTemplate,
    EditArrInstanceModalPartial, EditDownloadClientModalPartial, EditMediaServerModalPartial,
    EditProviderModalPartial, EndpointHealthView, EndpointRequestView, EpisodeView, ErrorTemplate,
    GrabView, HealthTemplate, ImportDirEntry, ImportIgnoredView, ImportModalPartial,
    ImportOutcomeView, ImportPickEpisodePartial, ImportPickTitlePartial, ImportReportPartial,
    ImportRowPartial, ImportRowView, InteractiveReleaseView, InteractiveResultsPartial,
    LibraryAddOptionsModalPartial, LibraryAddTemplate, LibraryDetailTemplate, LibraryDetailView,
    LibraryGrabsModalPartial, LibraryItemView, LibrarySeasonPartial, LibraryTemplate,
    LoginTemplate, MediaServerMappingView, MediaServerMappingsPartial, MediaServerOption,
    MediaServerView, MediaServersListPartial, MediaServersTemplate, NewProfileModalPartial,
    NewSearchModalPartial, PathMappingClientOption, PathMappingView, PathMappingsPartial,
    PickEpisodeView, PickTitleView, ProfileEditorTemplate, ProfileView, ProfilesTemplate,
    ProviderHealthView, ProviderView, ProvidersListPartial, ProvidersTemplate, PushGroupView,
    PushHistoryView, PushesFilterView, PushesTemplate, RecentSearchView, ReleasesTemplate,
    RootFolderView, RootFoldersListPartial, SearchDetailTemplate, SearchesFilterView,
    SearchesIndexTemplate, SeasonMarkView, SeasonView, SettingsFlash, SettingsTemplate,
    SettingsValues, StuckImportView, TmdbHitView, WebhookEventView, WebhooksTemplate,
};
use crate::{AppError, AppState};
use brarr_core::{Block, MediaType, MetadataSource, Ordering, OrderingFamily, TmdbId};

/// Build the Axum router with `state` as shared state.
pub fn router(state: AppState, static_dir: &std::path::Path) -> Router {
    let auth_layer = middleware::from_fn_with_state(state.clone(), auth_middleware);

    // Routes that require auth — wrapped by the middleware below.
    let protected = Router::new()
        .route("/", get(dashboard))
        .route("/providers", get(providers_index).post(providers_create))
        .route(
            "/providers/{id}",
            delete(providers_delete).put(providers_update),
        )
        .route("/providers/{id}/edit", get(providers_edit))
        .route("/providers/{id}/test", post(providers_test))
        .route("/providers/{id}/probe", get(providers_probe))
        .route("/providers/{id}/toggle", post(providers_toggle))
        .merge(arr_routes())
        .merge(download_client_routes())
        .merge(media_server_routes())
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
        .route("/searches", get(searches_index).post(searches_create))
        .route("/searches/new", get(new_search_modal))
        .route("/searches/{id}", get(search_detail))
        .route("/library", get(library_index))
        .route("/library/items", get(library_items))
        .route("/library/bulk", post(library_bulk))
        .route("/library/add", get(library_add).post(library_add_submit))
        .route("/library/add/options", get(library_add_options))
        .route("/library/verify", post(library_verify))
        .merge(import_routes())
        .route("/library/{id}/monitor", post(library_toggle_monitor))
        .route("/library/{id}/profile", post(library_set_profile))
        .route("/library/{id}/placement", get(library_placement))
        .route("/library/{id}/sources", get(library_sources))
        .route("/library/{id}/structure", post(library_structure))
        .route("/library/{id}/descriptive", post(library_descriptive))
        .route("/pause-banner", get(pause_banner))
        .route("/library/{id}/refresh", post(library_refresh))
        .route("/library/{id}/scan", post(library_scan_now))
        .route("/library/{id}/scan/status", get(library_scan_status))
        .route("/library/{id}/scan/target", post(library_scan_target))
        .route("/library/{id}/interactive", get(library_interactive))
        .route("/library/{id}/grab/{decision_id}", post(library_grab))
        .route("/library/{id}/grabs", get(library_grabs))
        .route("/library/{id}/season/{season_id}", get(library_season))
        .route(
            "/library/{id}/season/{season_id}/monitor",
            post(library_season_monitor),
        )
        .route(
            "/library/{id}/episode/{episode_id}/monitor",
            post(library_episode_monitor),
        )
        .route("/library/{id}", get(library_detail).delete(library_delete))
        .route("/queue", get(queue_index))
        .route("/queue/live", get(queue_live))
        .route("/webhooks", get(webhooks_index))
        .route("/health", get(health_index))
        .route("/settings", get(settings_index))
        .route("/settings/general", post(settings_general))
        .route("/settings/token", post(settings_token))
        .route(
            "/settings/maintenance/prune",
            post(settings_maintenance_prune),
        )
        .route(
            "/settings/maintenance/vacuum",
            post(settings_maintenance_vacuum),
        )
        .route("/logout", post(logout))
        .layer(auth_layer);

    // Torznab/Newznab endpoint for Sonarr/Radarr — same shared state,
    // but its own auth middleware (apikey query / bearer header instead
    // of the UI session cookie).
    let torznab = crate::web::torznab::router(state.clone());

    // Inbound Radarr/Sonarr Connect webhooks. Same machine-facing
    // auth model as Torznab (apikey query / bearer header / trusted
    // peer bypass).
    let webhooks = crate::web::webhooks::router(state.clone());

    // Open routes — login form, health, static files.
    Router::new()
        .merge(protected)
        .merge(torznab)
        .merge(webhooks)
        .route("/login", get(login_get).post(login_post))
        .route("/healthz", get(healthz))
        .nest_service("/static", static_files(static_dir))
        // Branded 404. Without this axum returns a bare `Nothing
        // matched` text body; the fallback lets us reuse the same
        // template that powers other error surfaces.
        .fallback(not_found)
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}

/// Download-client admin routes, merged into the protected router (and
/// so covered by the same auth layer). Split out to keep [`router`]
/// itself readable as one screen.
/// The import-from-disk surface, kept together so `router` stays under
/// the line limit and so this block reads as one flow.
///
/// All five sit under `/library/import`, which is a *literal* sibling of
/// `/library/{id}` — and that catch-all is registered last precisely so
/// literals win.
fn import_routes() -> Router<AppState> {
    Router::new()
        .route(
            "/library/import",
            get(library_import).post(library_import_submit),
        )
        .route("/library/import/unignore", post(library_import_unignore))
        .route("/library/import/pick-title", get(library_import_pick_title))
        .route(
            "/library/import/pick-episode",
            get(library_import_pick_episode),
        )
        .route("/library/import/row", get(library_import_row))
        .route("/library/import/bulk", post(library_import_bulk))
        .route("/library/adoption/{grab_id}", delete(library_adopt_undo))
}

/// Everything hanging off a configured Sonarr/Radarr.
///
/// Two eras in one group: the push/poll surface, deprecated since brarr
/// took over the download side, and the import — which is why the *arr
/// are still configured at all.
fn arr_routes() -> Router<AppState> {
    Router::new()
        .route(
            "/arr-instances",
            get(arr_instances_index).post(arr_instances_create),
        )
        .route(
            "/arr-instances/{id}",
            delete(arr_instances_delete).put(arr_instances_update),
        )
        .route("/arr-instances/{id}/edit", get(arr_instances_edit))
        .route("/arr-instances/{id}/test", post(arr_instances_test))
        .route("/arr-instances/{id}/poll-now", post(arr_instances_poll_now))
        .route("/arr-instances/{id}/toggle", post(arr_instances_toggle))
        .route(
            "/arr-instances/{id}/webhook-driven",
            post(arr_instances_webhook_driven_toggle),
        )
        .route(
            "/arr-instances/{id}/threshold",
            post(arr_instances_update_threshold),
        )
        .route(
            "/arr-instances/{id}/sync-source",
            post(arr_instances_sync_source_toggle),
        )
        .route("/arr-instances/{id}/import", get(arr_import_index))
        .route(
            "/arr-instances/{id}/import/mappings",
            post(arr_import_add_mapping),
        )
        .route("/arr-instances/{id}/import/run", post(arr_import_run))
        .route("/arr-root-mappings/{id}", delete(arr_root_mapping_delete))
}

fn download_client_routes() -> Router<AppState> {
    Router::new()
        .route(
            "/download-clients",
            get(download_clients_index).post(download_clients_create),
        )
        .route(
            "/download-clients/{id}",
            delete(download_clients_delete).put(download_clients_update),
        )
        .route("/download-clients/{id}/edit", get(download_clients_edit))
        .route("/download-clients/{id}/test", post(download_clients_test))
        .route(
            "/download-clients/{id}/toggle",
            post(download_clients_toggle),
        )
        .route("/root-folders", post(root_folders_create))
        .route("/root-folders/{id}", delete(root_folders_delete))
        .route("/path-mappings", post(path_mappings_create))
        .route("/path-mappings/{id}", delete(path_mappings_delete))
        .route("/grabs/{id}/requeue-import", post(grab_requeue_import))
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
    // An HTMX request must not be redirected. `fetch` follows a 303
    // transparently, so the whole login page would be swapped into
    // whatever fragment target issued the request — a dropdown, a table
    // row, a 24px badge. `HX-Redirect` on an empty 401 tells htmx to
    // navigate the window instead, which is what the operator wants to
    // happen when the session died.
    //
    // This is a prerequisite for polling, not a nicety: today the misfire
    // needs a click, and a `hx-trigger="every Ns"` makes it fire
    // unattended. The session cookie carries no `Max-Age` (it dies with
    // the browser) and the deploy recipe regenerates `BRARR_AUTH_TOKEN`
    // per run, so an expired session is the ordinary case.
    if req.headers().contains_key("hx-request") {
        let mut resp = StatusCode::UNAUTHORIZED.into_response();
        resp.headers_mut()
            .insert("HX-Redirect", HeaderValue::from_static("/login"));
        return Err(resp);
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
        .map(search_row_view)
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

async fn arr_instances_index(
    State(state): State<AppState>,
    headers: axum::http::HeaderMap,
) -> Result<Response, AppError> {
    let rows = arr_instances::list_all(state.pool()).await?;
    let profile_rows = quality_profiles::list_all(state.pool()).await?;
    let profile_by_id: std::collections::HashMap<
        Uuid,
        &crate::db::quality_profiles::QualityProfileRow,
    > = profile_rows.iter().map(|p| (p.id, p)).collect();
    let mut instances: Vec<_> = rows
        .iter()
        .map(|r| arr_instance_view_with_profile(r, &profile_by_id))
        .collect();
    fill_webhook_urls(&state, Some(&headers), &mut instances);
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
    let mut instances: Vec<_> = rows.into_iter().map(arr_instance_view).collect();
    fill_webhook_urls(&state, None, &mut instances);
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
    // Re-render the whole list (not just the row) so the per-instance
    // webhook detail sub-row can't be left orphaned.
    render_arr_instances_partial(&state).await
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
    let mut instances: Vec<_> = rows.into_iter().map(arr_instance_view).collect();
    fill_webhook_urls(&state, None, &mut instances);
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
        r#"<span id="arr-ping-{aid}" class="badge bg-info-soft text-info-soft-fg" title="searched {searched} of {considered} monitored movies; pushed {pushed}; {errors} search errors">{pushed} push / {searched} buscas</span>"#,
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
    render_status_badge(&format!("arr-ping-{arr_id}"), b)
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
    let base_url = crate::push::derive_request_base(&state, &headers);
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
    // Design tokens, not a Tailwind palette scale — see
    // [`render_status_badge`] for what the old `bg-emerald-100` spelling
    // cost.
    let (bg, fg, label) = match row.status {
        crate::db::push_history::PushStatus::Ok => {
            ("bg-success-soft", "text-success-soft-fg", "ok")
        }
        crate::db::push_history::PushStatus::HttpError => {
            ("bg-danger-soft", "text-danger-soft-fg", "http")
        }
        crate::db::push_history::PushStatus::TransportError => {
            ("bg-warning-soft", "text-warning-soft-fg", "net")
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

#[derive(Debug, Default, Deserialize)]
struct PushesIndexQuery {
    #[serde(default)]
    arr_instance_id: Option<String>,
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    from: Option<String>,
    #[serde(default)]
    to: Option<String>,
    #[serde(default)]
    release_query: Option<String>,
}

async fn pushes_index(
    State(state): State<AppState>,
    axum::extract::Query(q): axum::extract::Query<PushesIndexQuery>,
) -> Result<Response, AppError> {
    let pool = state.pool();

    let arr_id = nonblank(q.arr_instance_id.as_deref()).and_then(|s| Uuid::parse_str(s).ok());
    let status_choice = nonblank(q.status.as_deref()).unwrap_or("any");
    let status = match status_choice {
        "ok" => Some(crate::db::push_history::PushStatus::Ok),
        "http_error" => Some(crate::db::push_history::PushStatus::HttpError),
        "transport_error" => Some(crate::db::push_history::PushStatus::TransportError),
        _ => None,
    };
    let from_unix = nonblank(q.from.as_deref()).and_then(parse_iso_date_start);
    let to_unix = nonblank(q.to.as_deref()).and_then(parse_iso_date_end);
    let release_query = nonblank(q.release_query.as_deref()).map(str::to_string);

    let params = crate::db::push_history::FilterParams {
        arr_instance_id: arr_id,
        status,
        from_unix,
        to_unix,
        release_query: release_query.clone(),
        limit: 500,
        offset: 0,
    };
    let total_count = crate::db::push_history::count_filtered(pool, &params).await?;
    let rows = crate::db::push_history::filter(pool, params).await?;
    let groups = group_pushes(rows);

    // Populate the arr-instance dropdown — needs both enabled +
    // disabled rows so a filter set against a soft-disabled arr
    // doesn't disappear silently.
    let arr_rows = arr_instances::list_all(pool).await?;
    let arr_options = arr_rows
        .into_iter()
        .map(|a| (a.id.to_string(), a.name))
        .collect();

    let filters = PushesFilterView {
        arr_instance_id: q.arr_instance_id.clone().unwrap_or_default(),
        status: status_choice.to_string(),
        from_date: q.from.clone().unwrap_or_default(),
        to_date: q.to.clone().unwrap_or_default(),
        release_query: q.release_query.clone().unwrap_or_default(),
    };

    html(&PushesTemplate {
        groups,
        filters,
        arr_options,
        total_count,
    })
}

/// `GET /webhooks` — audit log of inbound Sonarr/Radarr Connect events.
/// Wires the `webhook_events::recent` query (previously orphaned) to a
/// page so the operator can see what *arr fired and which search it
/// triggered.
async fn webhooks_index(State(state): State<AppState>) -> Result<Response, AppError> {
    let pool = state.pool();
    let events = crate::db::webhook_events::recent(pool, 200).await?;
    let name_by_id: std::collections::HashMap<Uuid, String> = arr_instances::list_all(pool)
        .await?
        .into_iter()
        .map(|a| (a.id, a.name))
        .collect();
    let views = events
        .into_iter()
        .map(|e| WebhookEventView {
            received_at: format_ts(e.received_at),
            arr_instance_name: name_by_id
                .get(&e.arr_instance_id)
                .cloned()
                .unwrap_or_else(|| "(removida)".to_string()),
            kind: e.kind.label().to_string(),
            event_type: e.event_type,
            triggered_search_id: e.triggered_search_id.map(|u| u.to_string()),
            payload_preview: truncate_payload(&e.payload_json),
        })
        .collect();
    html(&WebhooksTemplate { events: views })
}

// ---- library ---------------------------------------------------------

/// Query string of `GET /library`.
#[derive(Debug, Deserialize)]
struct LibraryQuery {
    /// `grid` (default) or `list`.
    #[serde(default)]
    view: Option<String>,
    /// `movie`, `tv`, `unmonitored`, `missing`, `complete`, or absent
    /// for everything.
    #[serde(default)]
    filter: Option<String>,
    /// Free-text title search. Matched against the title *and* the
    /// original title, forgiving accents, punctuation, word order and a
    /// bounded number of typos — see [`crate::fuzzy`].
    #[serde(default)]
    q: Option<String>,
    /// `title_asc`, `title_desc`, `added_asc`, `added_desc`, or absent
    /// for the default — match order while searching, newest added
    /// otherwise.
    #[serde(default)]
    sort: Option<String>,
}

/// Query string of `GET /library/add`.
#[derive(Debug, Deserialize)]
struct LibraryAddQuery {
    /// Free-text search term.
    #[serde(default)]
    q: Option<String>,
    /// `all` (default), `movie` or `tv`.
    #[serde(default)]
    kind: Option<String>,
}

/// Form body of `POST /library/add`.
#[derive(Debug, Deserialize)]
struct LibraryAddForm {
    /// TMDB id of the chosen hit.
    tmdb_id: i64,
    /// `movie` or `tv`.
    media_type: String,
    /// Chosen root folder. Empty means "leave whatever is there".
    #[serde(default)]
    root_folder: Option<String>,
    /// Chosen quality profile. Empty means "no profile", which is a
    /// real choice, not an absent one — see the handler.
    #[serde(default)]
    profile_id: Option<String>,
    /// [`crate::db::library::MonitorScope`] label.
    #[serde(default)]
    monitor_scope: Option<String>,
    /// Movies have a checkbox instead of a scope select. An unchecked
    /// checkbox posts nothing at all, which is why this is an `Option`
    /// and not a `bool`.
    #[serde(default)]
    monitored_movie: Option<String>,
    /// Whether to sweep for this title immediately.
    #[serde(default)]
    search_now: Option<String>,
}

/// Query string of `GET /library/add/options`.
#[derive(Debug, Deserialize)]
struct LibraryAddOptionsQuery {
    tmdb_id: i64,
    media_type: String,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    year: Option<i32>,
}

/// Render a stored date as `dd/mm/aaaa`.
fn short_date(ts: OffsetDateTime) -> String {
    format!("{:02}/{:02}/{}", ts.day(), u8::from(ts.month()), ts.year())
}

/// `GET /library` — the catalogue.
async fn library_index(
    State(state): State<AppState>,
    axum::extract::Query(q): axum::extract::Query<LibraryQuery>,
) -> Result<Response, AppError> {
    library_page(state, q, false, String::new()).await
}

/// `GET /library/items` — just the results, for the live search and the
/// filter chips.
///
/// Same code path as the page, so a filtered fragment and a reloaded
/// page can never disagree about what matches.
async fn library_items(
    State(state): State<AppState>,
    axum::extract::Query(q): axum::extract::Query<LibraryQuery>,
) -> Result<Response, AppError> {
    library_page(state, q, true, String::new()).await
}

#[allow(
    clippy::too_many_lines,
    reason = "one linear pipeline — read the query, take four bulk reads, \
              fold each row through coverage, order, render. Cutting it up \
              means helpers taking eight parameters each, which hides the \
              sequence instead of clarifying it; the pieces that *are* \
              self-contained (tree_summary, passes_filter, rank_items) are \
              already out."
)]
async fn library_page(
    state: AppState,
    q: LibraryQuery,
    fragment: bool,
    notice: String,
) -> Result<Response, AppError> {
    let view = match q.view.as_deref() {
        Some("list") => "list".to_owned(),
        _ => "grid".to_owned(),
    };
    let filter = q.filter.unwrap_or_default();
    let query = q.q.unwrap_or_default().trim().to_owned();
    let sort = normalise_sort(q.sort.as_deref());

    let counts = library::counts(state.pool()).await?;
    let all = library::list(state.pool()).await?;

    // One query for every profile name, not one per title. This was a
    // `get_by_id` inside the loop — 360 round trips to render a column
    // — which the de-N+1 pass missed because it was counting the
    // *season* queries.
    let profile_names = profile_name_map(state.pool()).await?;
    // One query for every id on the page, for the reason the four below
    // exist: this screen renders 360 rows and has been N+1 twice.
    let ids_by_item = item_ids::for_all(state.pool()).await?;

    // Four queries for the whole page, whatever its size. This used to
    // be two per series — `seasons` and `episodes` — which is 720 round
    // trips once the *arr migration lands 360 titles, to render a
    // summary line.
    let now = OffsetDateTime::now_utc();
    let monitored_episodes = library::monitored_episodes(state.pool()).await?;
    let coverage = grabs::live_coverage(state.pool()).await?;
    let trees = library::tree_counts(state.pool()).await?;
    let series_progress = crate::coverage::summarise(&monitored_episodes, &coverage, now);

    let mut items: Vec<Sortable> = Vec::with_capacity(all.len());
    for item in all {
        // The search runs before the coverage work, so a query narrows
        // what has to be summarised rather than summarising everything
        // and throwing most of it away.
        let rank = if query.is_empty() {
            0
        } else {
            let original = item.original_title.clone().unwrap_or_default();
            match crate::fuzzy::score(&query, &[&item.title, &original]) {
                Some(r) => r,
                None => continue,
            }
        };

        // Only series have a tree. Season 0 is TMDB's specials bucket,
        // and it is not small: The Boys carries 76 of them against 40
        // real episodes. Both halves of the summary exclude it or the
        // line reads "5 temporadas · 116 episódios", which is nonsense.
        let is_series = item.media_type == library::MediaType::Tv;
        let tree_summary = tree_summary(trees.get(&item.id).copied(), is_series);

        let progress = crate::coverage::progress_of(&item, &series_progress, &coverage, now);
        let status = crate::coverage::ItemStatus::of(item.monitored, progress);
        let (missing, upcoming) = status.callout(progress);

        if !passes_filter(&filter, is_series, item.monitored, missing, status) {
            continue;
        }

        let profile = item
            .profile_id
            .and_then(|pid| profile_names.get(&pid).cloned())
            .unwrap_or_else(|| "—".to_owned());

        items.push(Sortable {
            rank,
            added_at: item.added_at,
            view: LibraryItemView {
                id: item.id.to_string(),
                title: item.title,
                year: item.year.map_or_else(|| "—".to_owned(), |y| y.to_string()),
                kind_label: if is_series { "Série" } else { "Filme" }.to_owned(),
                is_series,
                poster_url: art::url(
                    item.poster_source.unwrap_or(MetadataSource::Tmdb),
                    item.poster_path.as_deref(),
                    art::ImageSize::Index,
                ),
                monitored: item.monitored,
                profile,
                ids: ids_by_item
                    .get(&item.id)
                    .map(|stored| {
                        stored
                            .iter()
                            .map(|s| crate::web::templates::IdChipView::of(&s.id))
                            .collect()
                    })
                    .unwrap_or_default(),
                tree_summary,
                added_at: short_date(item.added_at),
                tone: status.tone().to_owned(),
                status_label: status.label().to_owned(),
                monitored_count: progress.total,
                have: progress.have,
                missing,
                upcoming,
                percent: progress.percent(),
            },
        });
    }

    let items = rank_items(items, &query, &sort);

    let tmdb_ready = crate::tmdb_sync::load_config(state.pool())
        .await?
        .is_configured();

    let matched = items.len();
    let profiles = profile_options(state.pool()).await?;
    let root_folders = bulk_root_options(&state).await?;

    if fragment {
        return html(&crate::web::templates::LibraryItemsPartial {
            items,
            matched,
            view,
            filter,
            query,
            sort,
            profiles,
            root_folders,
            notice,
            tmdb_ready,
        });
    }
    html(&LibraryTemplate {
        movies: counts.movies,
        series: counts.series,
        unmonitored: counts.unmonitored,
        tmdb_ready,
        items,
        matched,
        view,
        filter,
        query,
        sort,
        profiles,
        root_folders,
        notice,
    })
}

/// `POST /library/bulk` — one action against every checked title.
///
/// Body is `Vec<(String, String)>` rather than a struct, the same shape
/// the import screen uses: `sel` repeats once per checked row, and
/// `serde_urlencoded` — which is what axum's `Form` runs on — collapses
/// repeated keys instead of collecting them. A `Vec<String>` field
/// silently loses every value but one.
///
/// Answers the re-rendered list rather than a redirect, so the operator
/// sees the new state without losing the filter or the search they were
/// in. An unknown id is skipped rather than failing the batch: the page
/// may be minutes old, and a title deleted in another tab must not abort
/// the other thirty-nine updates.
async fn library_bulk(
    State(state): State<AppState>,
    Form(fields): Form<Vec<(String, String)>>,
) -> Result<Response, AppError> {
    let value = |key: &str| {
        fields
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.trim().to_owned())
    };
    let ids: Vec<Uuid> = fields
        .iter()
        .filter(|(k, _)| k == "sel")
        .filter_map(|(_, v)| Uuid::parse_str(v).ok())
        .collect();
    let action = value("action").unwrap_or_default();
    let mut notice = String::new();

    if !ids.is_empty() {
        match action.as_str() {
            "monitor" => {
                library::set_monitored_cascading(state.pool(), &ids, true).await?;
            }
            "unmonitor" => {
                library::set_monitored_cascading(state.pool(), &ids, false).await?;
            }
            "profile" => {
                let raw = value("profile_id").unwrap_or_default();
                let profile =
                    if raw.is_empty() {
                        None
                    } else {
                        Some(Uuid::parse_str(&raw).map_err(|e| {
                            AppError::InvalidInput(format!("invalid profile id: {e}"))
                        })?)
                    };
                library::set_profile_many(state.pool(), &ids, profile).await?;
            }
            "root" => {
                let raw = value("root_folder").unwrap_or_default();
                if raw.is_empty() {
                    library::set_root_folder_many(state.pool(), &ids, None).await?;
                } else {
                    // A root that serves only movies must not be written
                    // onto a series: the importer would place the files
                    // under it and the operator would find a season tree
                    // inside /midias/Filmes. A mixed selection is the
                    // normal case here, so the incompatible half is
                    // skipped and *reported* rather than dropped.
                    let (ok, skipped) = split_by_root_compatibility(&state, &ids, &raw).await?;
                    library::set_root_folder_many(state.pool(), &ok, Some(raw.as_str())).await?;
                    if skipped > 0 {
                        notice = format!(
                            "Pasta aplicada a {} título(s). {} ficaram de fora: essa raiz não serve o tipo deles.",
                            ok.len(),
                            skipped
                        );
                    }
                }
            }
            other => {
                return Err(AppError::InvalidInput(format!(
                    "ação em lote desconhecida: {other}"
                )));
            }
        }
    }

    library_page(
        state,
        LibraryQuery {
            view: value("view"),
            filter: value("filter"),
            q: value("q"),
            sort: value("sort"),
        },
        true,
        notice,
    )
    .await
}

/// The `/static` tree, with revalidation forced.
///
/// `no-cache` means "revalidate", not "do not store": the browser keeps
/// the file and asks with `If-Modified-Since`, so the ordinary answer is
/// a 304 with no body.
///
/// It is here because `ServeDir` sends only `Last-Modified`, and a
/// response carrying no explicit directive is **heuristically**
/// cacheable — the browser invents an expiry from the file's age. A
/// hand-authored stylesheet that changes every release is the worst
/// possible thing to guess about, and the failure is silent in the ugly
/// direction: the operator gets fresh markup rendered against days-old
/// CSS, so every new class is simply inert. That is what "the filters
/// lost their padding" was.
///
/// Wrapped in its own `Router` rather than layered onto the outer one,
/// so the header lands on the static tree and nowhere near the Torznab
/// feeds. Belt to the `?v=` braces on the `<link>` tags, which give a
/// changed URL per release.
fn static_files(dir: &std::path::Path) -> Router {
    Router::new().fallback_service(ServeDir::new(dir)).layer(
        tower_http::set_header::SetResponseHeaderLayer::overriding(
            header::CACHE_CONTROL,
            HeaderValue::from_static("no-cache"),
        ),
    )
}

/// "5 temporadas · 40 episódios · 76 especiais", or empty for a movie.
///
/// Season 0 is counted separately rather than folded in: The Boys
/// carries 76 specials against 40 real episodes, and adding them would
/// render "5 temporadas · 116 episódios", which is nonsense.
fn tree_summary(tree: Option<library::TreeCounts>, is_series: bool) -> String {
    if !is_series {
        return String::new();
    }
    let tree = tree.unwrap_or_default();
    if tree.specials > 0 {
        format!(
            "{} temporadas · {} episódios · {} especiais",
            tree.seasons, tree.episodes, tree.specials
        )
    } else {
        format!("{} temporadas · {} episódios", tree.seasons, tree.episodes)
    }
}

/// Whether one title survives the active filter chip.
///
/// The status chips are answered here rather than against a column,
/// because "faltando" and "completa" are things `crate::coverage`
/// computes. `missing` is the **callout** count, so a paused title with
/// the same gaps does not appear under "faltando": brarr is not going
/// to chase it, and listing it would be a call to an action that does
/// not exist.
fn passes_filter(
    filter: &str,
    is_series: bool,
    monitored: bool,
    missing: usize,
    status: crate::coverage::ItemStatus,
) -> bool {
    match filter {
        "movie" => !is_series,
        "tv" => is_series,
        "unmonitored" => !monitored,
        "missing" => missing > 0,
        "complete" => status == crate::coverage::ItemStatus::Complete,
        _ => true,
    }
}

/// `(id, name)` for every quality profile, for the bulk picker.
async fn profile_options(pool: &crate::db::Pool) -> Result<Vec<(String, String)>, AppError> {
    Ok(quality_profiles::list_all(pool)
        .await?
        .into_iter()
        .map(|p| (p.id.to_string(), p.name))
        .collect())
}

/// `GET /library/add` — TMDB search.
async fn library_add(
    State(state): State<AppState>,
    axum::extract::Query(q): axum::extract::Query<LibraryAddQuery>,
) -> Result<Response, AppError> {
    let query = q.q.unwrap_or_default().trim().to_owned();
    let kind = match q.kind.as_deref() {
        Some("movie") => "movie".to_owned(),
        Some("tv") => "tv".to_owned(),
        _ => "all".to_owned(),
    };

    let cfg = crate::tmdb_sync::load_config(state.pool()).await?;
    if !cfg.is_configured() {
        return html(&LibraryAddTemplate {
            query,
            kind,
            results: Vec::new(),
            tmdb_ready: false,
            error: None,
            searched: false,
        });
    }
    if query.is_empty() {
        return html(&LibraryAddTemplate {
            query,
            kind,
            results: Vec::new(),
            tmdb_ready: true,
            error: None,
            searched: false,
        });
    }

    // Which TMDB ids are already catalogued, so the list can show a pill
    // instead of an add button. Asked on the identity set: a title the
    // catalogue holds only under another source is still in the library,
    // and a comparison against one column would offer to add it twice.
    let catalogued = catalogued_tmdb_ids(state.pool()).await?;

    let tmdb = crate::tmdb_sync::client(state.pool()).await?;
    let mut results = Vec::new();
    let mut error = None;

    if kind != "tv" {
        match tmdb.search_movies(&query, None).await {
            Ok(hits) => {
                for h in hits {
                    let year = h.year().map_or_else(|| "—".to_owned(), |y| y.to_string());
                    let in_library = catalogued.contains(&(library::MediaType::Movie, h.tmdb_id));
                    results.push(TmdbHitView {
                        tmdb_id: h.tmdb_id,
                        media_type: "movie".to_owned(),
                        kind_label: "Filme".to_owned(),
                        is_series: false,
                        title: h.title,
                        year,
                        overview: h.overview,
                        poster_url: art::url(
                            MetadataSource::Tmdb,
                            h.poster_path.as_deref(),
                            art::ImageSize::Index,
                        ),
                        in_library,
                    });
                }
            }
            Err(e) => error = Some(format!("Busca de filmes falhou: {e}")),
        }
    }
    if kind != "movie" {
        match tmdb.search_tv(&query, None).await {
            Ok(hits) => {
                for h in hits {
                    let year = h.year().map_or_else(|| "—".to_owned(), |y| y.to_string());
                    let in_library = catalogued.contains(&(library::MediaType::Tv, h.tmdb_id));
                    results.push(TmdbHitView {
                        tmdb_id: h.tmdb_id,
                        media_type: "tv".to_owned(),
                        kind_label: "Série".to_owned(),
                        is_series: true,
                        title: h.name,
                        year,
                        overview: h.overview,
                        poster_url: art::url(
                            MetadataSource::Tmdb,
                            h.poster_path.as_deref(),
                            art::ImageSize::Index,
                        ),
                        in_library,
                    });
                }
            }
            Err(e) => error = Some(format!("Busca de séries falhou: {e}")),
        }
    }

    html(&LibraryAddTemplate {
        query,
        kind,
        results,
        tmdb_ready: true,
        error,
        searched: true,
    })
}

/// Every TMDB id the catalogue holds, paired with both media kinds.
///
/// One query rather than `library::list` plus a scan over three columns:
/// the screen only needs to know whether a hit is already held, and the
/// answer lives in `library_item_ids`. Both kinds are inserted because a
/// film and a series may legitimately share a TMDB id, which is what the
/// old composite unique index guarded — the caller asks with the kind it
/// is rendering.
async fn catalogued_tmdb_ids(
    pool: &crate::db::Pool,
) -> Result<std::collections::HashSet<(library::MediaType, i64)>, AppError> {
    let mut out = std::collections::HashSet::new();
    for ids in item_ids::for_all(pool).await?.values() {
        for stored in ids {
            if stored.id.source() != brarr_core::MetadataSource::Tmdb {
                continue;
            }
            if let Ok(value) = stored.id.value().parse::<i64>() {
                out.insert((library::MediaType::Movie, value));
                out.insert((library::MediaType::Tv, value));
            }
        }
    }
    Ok(out)
}

/// `GET /library/add/options` — the dialog that lets the operator choose
/// destination, profile and monitoring *before* committing.
async fn library_add_options(
    State(state): State<AppState>,
    axum::extract::Query(q): axum::extract::Query<LibraryAddOptionsQuery>,
) -> Result<Response, AppError> {
    let media_type = crate::db::library::media_type_from_label(&q.media_type)?;
    let existing =
        match brarr_core::ExternalId::new(brarr_core::MetadataSource::Tmdb, &q.tmdb_id.to_string())
        {
            Ok(id) => library::get_by_external(state.pool(), media_type, &id)
                .await
                .ok(),
            Err(_) => None,
        };
    html(
        &add_options_modal(
            &state,
            &AddOptionsContext {
                tmdb_id: q.tmdb_id,
                media_type,
                title: q.title.unwrap_or_else(|| "Sem título".to_owned()),
                year: q.year,
                existing: existing.as_ref(),
                error: None,
            },
        )
        .await?,
    )
}

/// Everything the dialog needs that is not derived from the database.
struct AddOptionsContext<'a> {
    tmdb_id: i64,
    media_type: crate::db::library::MediaType,
    title: String,
    year: Option<i32>,
    existing: Option<&'a library::LibraryItem>,
    error: Option<String>,
}

async fn add_options_modal(
    state: &AppState,
    ctx: &AddOptionsContext<'_>,
) -> Result<LibraryAddOptionsModalPartial, AppError> {
    use crate::db::library::MediaType;

    let chosen_folder = ctx.existing.and_then(|i| i.root_folder.clone());
    let root_folders: Vec<AddOptionFolder> = root_folders::list_all(state.pool())
        .await?
        .into_iter()
        .filter(|f| f.media_type.is_none() || f.media_type == Some(ctx.media_type))
        .map(|f| {
            let path = f.path.to_string_lossy().into_owned();
            AddOptionFolder {
                selected: chosen_folder.as_deref() == Some(path.as_str()),
                label: match f.media_type {
                    Some(MediaType::Movie) => "filmes".to_owned(),
                    Some(MediaType::Tv) => "séries".to_owned(),
                    None => String::new(),
                },
                path,
            }
        })
        .collect();

    let chosen_profile = ctx.existing.and_then(|i| i.profile_id);
    let profiles: Vec<AddOptionProfile> = quality_profiles::list_all(state.pool())
        .await?
        .into_iter()
        .map(|p| AddOptionProfile {
            selected: chosen_profile == Some(p.id),
            id: p.id.to_string(),
            name: p.name,
            threshold: p.push_threshold,
        })
        .collect();

    Ok(LibraryAddOptionsModalPartial {
        tmdb_id: ctx.tmdb_id,
        title: ctx.title.clone(),
        year: ctx.year,
        is_series: ctx.media_type == MediaType::Tv,
        already_in_library: ctx.existing.is_some(),
        no_profile_selected: chosen_profile.is_none(),
        default_threshold: crate::scan::DEFAULT_PUSH_THRESHOLD,
        scope: ctx
            .existing
            .map_or(crate::db::library::MonitorScope::All, |i| i.monitor_scope)
            .label()
            .to_owned(),
        root_folders,
        profiles,
        error: ctx.error.clone(),
    })
}

/// `POST /library/add` — pull the full record from TMDB and catalogue it
/// with the operator's choices.
async fn library_add_submit(
    State(state): State<AppState>,
    Form(form): Form<LibraryAddForm>,
) -> Result<Response, AppError> {
    let media_type = crate::db::library::media_type_from_label(&form.media_type)?;

    // A path arriving from the page is not trusted just because the page
    // offered it — same rule the detail screen's picker follows.
    let root_folder = match form.root_folder.as_deref().map(str::trim) {
        None | Some("") => None,
        Some(path) => Some(validated_root_folder(&state, path).await?),
    };
    let profile_id = match form.profile_id.as_deref().map(str::trim) {
        None | Some("") => None,
        Some(raw) => Some(
            Uuid::parse_str(raw)
                .map_err(|e| AppError::InvalidInput(format!("perfil inválido: {e}")))?,
        ),
    };

    // A movie has a checkbox, a series has a select. Both land here as
    // one scope.
    let scope = if media_type == crate::db::library::MediaType::Movie {
        Some(if form.monitored_movie.is_some() {
            crate::db::library::MonitorScope::All
        } else {
            crate::db::library::MonitorScope::Nothing
        })
    } else {
        match form.monitor_scope.as_deref().map(str::trim) {
            None | Some("") => None,
            Some(raw) => Some(crate::db::library::MonitorScope::from_label(raw)?),
        }
    };

    let tmdb = crate::tmdb_sync::client(state.pool()).await?;
    let registry = Registry::build(state.pool()).await?;
    let item = crate::tmdb_sync::add_with_options(
        state.pool(),
        &tmdb,
        &registry,
        media_type,
        form.tmdb_id,
        &crate::tmdb_sync::AddOptions {
            profile_id,
            root_folder,
            monitor_scope: scope,
        },
    )
    .await?;

    if form.search_now.is_some() && item.monitored {
        // Same shape as the "buscar agora" button: spawn it and answer,
        // rather than holding the request open while a series with forty
        // episodes is swept.
        let state = state.clone();
        let target = item.clone();
        tokio::spawn(async move { crate::scan::run_once_for_item(&state, &target).await });
    }

    // HX-Refresh rather than a redirect: the POST comes from a dialog
    // inside #modal-target, so a 3xx would swap the whole /library page
    // into the modal slot.
    Ok(hx_refresh())
}

/// `POST /library/{id}/monitor` — flip the monitoring flag.
async fn library_toggle_monitor(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Response, AppError> {
    let uuid = Uuid::parse_str(&id)
        .map_err(|e| AppError::InvalidInput(format!("invalid library id: {e}")))?;
    let item = library::get_by_id(state.pool(), uuid).await?;
    // Cascading, like the bulk action: the item flag alone leaves the
    // sweep with no targets, so the bookmark would go green and nothing
    // would be chased.
    library::set_monitored_cascading(state.pool(), &[uuid], !item.monitored).await?;
    Ok(hx_refresh())
}

/// `DELETE /library/{id}` — drop the item; seasons, episodes and grabs
/// cascade.
async fn library_delete(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Response, AppError> {
    let uuid = Uuid::parse_str(&id)
        .map_err(|e| AppError::InvalidInput(format!("invalid library id: {e}")))?;
    library::delete(state.pool(), uuid).await?;
    Ok(hx_refresh())
}

/// Form body of `POST /library/{id}/profile`.
#[derive(Debug, Deserialize)]
struct LibraryProfileForm {
    /// Stringified profile UUID, or empty to detach.
    #[serde(default)]
    profile_id: String,
    /// Registered root-folder path this item is pinned to. Empty falls
    /// back to the per-media-type rule; absent (an older page) leaves
    /// whatever the item already had.
    ///
    /// This is what makes more than one root folder per media type
    /// usable: the rule can only pick one `tv` folder, so a library
    /// split into "Séries" and "Animes" needs the per-item choice.
    #[serde(default)]
    root_folder: Option<String>,
}

/// A production status in the operator's language.
///
/// The hero used to render TMDB's own English — `Returning Series`,
/// `Ended` — because the column held that provider's words verbatim.
/// With brarr's own vocabulary behind it the screen can say what it
/// means, which is the rule this repository applies everywhere else:
/// English identifiers, Portuguese for what a person reads.
const fn status_label(status: library::ProductionStatus) -> &'static str {
    match status {
        library::ProductionStatus::Returning => "em exibição",
        library::ProductionStatus::Ended => "encerrada",
        library::ProductionStatus::Cancelled => "cancelada",
        library::ProductionStatus::InProduction => "em produção",
        library::ProductionStatus::Released => "lançado",
        library::ProductionStatus::Announced => "anunciado",
    }
}

/// `GET /library/{id}` — one catalogue entry in full.
async fn library_detail(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Response, AppError> {
    let uuid = Uuid::parse_str(&id)
        .map_err(|e| AppError::InvalidInput(format!("invalid library id: {e}")))?;
    let item = library::get_by_id(state.pool(), uuid).await?;
    let is_series = item.media_type == library::MediaType::Tv;

    // Coverage for this one item — the same rule the index applies over
    // the whole library, narrowed to one row.
    let coverage = grabs::live_coverage_for_item(state.pool(), uuid).await?;
    let now = OffsetDateTime::now_utc();
    let episodes = if is_series {
        library::episodes(state.pool(), uuid).await?
    } else {
        Vec::new()
    };
    // Specials included: the count follows monitoring and nothing else.
    let monitored = monitored_rows(uuid, &episodes);
    let series_progress = crate::coverage::summarise(&monitored, &coverage, now);
    let progress = crate::coverage::progress_of(&item, &series_progress, &coverage, now);
    let status = crate::coverage::ItemStatus::of(item.monitored, progress);

    // Whatever the catalogue knows this title as. One query, because the
    // hero renders one item — the index reads them in bulk.
    let stored_ids = crate::db::item_ids::for_item(state.pool(), uuid).await?;

    // Seasons carry only their header here; episodes load on expand.
    let seasons = if is_series {
        library::seasons(state.pool(), uuid)
            .await?
            .iter()
            .map(|s| season_view(s, &monitored, &coverage, now))
            .collect()
    } else {
        Vec::new()
    };

    let grab_count = grabs::for_item(state.pool(), uuid).await?.len();

    let profiles = quality_profiles::list_all(state.pool())
        .await?
        .into_iter()
        .map(|p| (p.id.to_string(), p.name))
        .collect();

    let root_folder_options = root_folder_options(&state, item.media_type).await?;
    let current_root = item.root_folder.clone().unwrap_or_default();

    // Only meaningful for movies, and only while the date is ahead of us.
    let in_theatrical_window = item.digital_release_at.is_some_and(|d| d > now);

    let original_title = item
        .original_title
        .clone()
        .filter(|o| o.trim() != item.title.trim());

    html(&LibraryDetailTemplate {
        item: LibraryDetailView {
            id: item.id.to_string(),
            title: item.title,
            original_title,
            year: item.year.map_or_else(|| "—".to_owned(), |y| y.to_string()),
            kind_label: if is_series { "Série" } else { "Filme" }.to_owned(),
            is_series,
            poster_url: art::url(
                item.poster_source.unwrap_or(MetadataSource::Tmdb),
                item.poster_path.as_deref(),
                art::ImageSize::Hero,
            ),
            overview: item.overview,
            ids: stored_ids
                .iter()
                .map(|stored| crate::web::templates::IdChipView::of(&stored.id))
                .collect(),
            monitored: item.monitored,
            profile_id: item.profile_id.map(|p| p.to_string()).unwrap_or_default(),
            status: item.status.map(|s| status_label(s).to_owned()),
            runtime: item
                .runtime_minutes
                .map_or_else(String::new, |r| format!("{r} min")),
            next_air_date: item.next_air_date.map_or_else(String::new, short_date),
            digital_release: item.digital_release_at.map_or_else(String::new, short_date),
            physical_release: item
                .physical_release_at
                .map_or_else(String::new, short_date),
            in_theatrical_window,
        },
        status: crate::web::templates::ItemStatusView {
            tone: status.tone().to_owned(),
            status_label: status.label().to_owned(),
            monitored_count: progress.total,
            have: progress.have,
            missing: status.callout(progress).0,
            percent: progress.percent(),
        },
        seasons,
        grab_count,
        profiles,
        root_folders: root_folder_options,
        root_folder: current_root,
    })
}

/// The item's status line, read fresh.
///
/// Both toggles call this: monitoring a season or a single episode moves
/// the denominator, so the hero is wrong the moment either runs. Three
/// small queries on a click is the cheap half of the trade — the other
/// option was `HX-Refresh`, which closes the accordion.
async fn item_status_view(
    state: &AppState,
    item_id: Uuid,
) -> Result<crate::web::templates::ItemStatusView, AppError> {
    let item = library::get_by_id(state.pool(), item_id).await?;
    let episodes = library::episodes(state.pool(), item_id).await?;
    let coverage = grabs::live_coverage_for_item(state.pool(), item_id).await?;
    let now = OffsetDateTime::now_utc();
    let monitored = monitored_rows(item_id, &episodes);
    let series = crate::coverage::summarise(&monitored, &coverage, now);
    let progress = crate::coverage::progress_of(&item, &series, &coverage, now);
    let status = crate::coverage::ItemStatus::of(item.monitored, progress);
    Ok(crate::web::templates::ItemStatusView {
        tone: status.tone().to_owned(),
        status_label: status.label().to_owned(),
        monitored_count: progress.total,
        have: progress.have,
        missing: status.callout(progress).0,
        percent: progress.percent(),
    })
}

/// The monitored episodes of one item, as the coverage rule reads them.
///
/// **No season filter.** The count follows monitoring, so a monitored
/// special counts and an unmonitored one does not — which is what makes
/// the operator's season toggle mean something. See [`crate::coverage`].
fn monitored_rows(item_id: Uuid, episodes: &[library::Episode]) -> Vec<library::MonitoredEpisode> {
    episodes
        .iter()
        .filter(|e| e.monitored)
        .map(|e| library::MonitoredEpisode {
            item_id,
            id: e.id,
            season_number: e.season_number,
            air_date: e.air_date,
        })
        .collect()
}

/// One season header, with its own progress.
///
/// Per-season progress runs the **same** summariser over just this
/// season's monitored episodes, so a season chip can never disagree with
/// the item chip above it — a second hand-rolled count is how they would.
fn season_view(
    season: &library::Season,
    monitored: &[library::MonitoredEpisode],
    coverage: &[grabs::Coverage],
    now: OffsetDateTime,
) -> SeasonView {
    let mine: Vec<library::MonitoredEpisode> = monitored
        .iter()
        .filter(|e| e.season_number == season.season_number)
        .copied()
        .collect();
    // Every row carries the same `item_id`, so the map has one entry.
    let progress = crate::coverage::summarise(&mine, coverage, now)
        .into_values()
        .next()
        .unwrap_or_default();
    let status = crate::coverage::ItemStatus::of(season.monitored, progress);
    SeasonView {
        id: season.id.to_string(),
        number: season.season_number,
        label: if season.season_number == 0 {
            "Especiais".to_owned()
        } else {
            format!("Temporada {}", season.season_number)
        },
        episode_count: season.episode_count,
        monitored: season.monitored,
        tone: status.tone().to_owned(),
        status_label: status.label().to_owned(),
        monitored_count: progress.total,
        have: progress.have,
        percent: progress.percent(),
    }
}

/// Root folders an item of `media_type` could be pinned to, as
/// `(path, label)`.
///
/// Only the ones that serve that kind: pinning a series to a
/// movies-only folder is not a choice worth offering. This picker is
/// what makes a *second* folder of the same kind reachable at all — the
/// per-media-type rule can only ever pick one, so a library split into
/// "Séries" and "Animes" has no other way to say which is which.
async fn root_folder_options(
    state: &AppState,
    media_type: crate::db::library::MediaType,
) -> Result<Vec<(String, String)>, AppError> {
    Ok(all_root_folder_options(state)
        .await?
        .into_iter()
        .filter(|(_, _, serves)| serves.is_none() || *serves == Some(media_type))
        .map(|(path, label, _)| (path, label))
        .collect())
}

/// Accept only an ordering the screen offers; anything else is the
/// default.
///
/// Falling back rather than erroring because the value arrives in a URL
/// the operator can edit and share. A typo there should reorder nothing,
/// not answer 400.
fn normalise_sort(raw: Option<&str>) -> String {
    match raw {
        Some(s @ ("title_asc" | "title_desc" | "added_asc" | "added_desc")) => s.to_owned(),
        _ => String::new(),
    }
}

/// One catalogue row plus the two keys it can be ordered by.
///
/// `added_at` travels as the timestamp rather than the `dd/mm/aaaa` the
/// view carries — sorting the formatted string would order by day of the
/// month.
struct Sortable {
    /// Search match rank; `0` when there is no query.
    rank: u32,
    /// When the operator added it.
    added_at: OffsetDateTime,
    /// The row itself.
    view: LibraryItemView,
}

/// Put the rows in order and drop the keys.
///
/// An explicit choice always wins. With none, a search orders by match
/// and an unfiltered catalogue keeps the SQL order — newest added first,
/// which is what the operator expects to open the screen to.
///
/// Titles sort on [`crate::fuzzy::normalise`], not on the raw string:
/// byte order puts every uppercase title before every lowercase one and
/// files "Ávatar" after "Zulu", which is not what anyone means by
/// alphabetical in Portuguese.
fn rank_items(mut items: Vec<Sortable>, query: &str, sort: &str) -> Vec<LibraryItemView> {
    match sort {
        "title_asc" => items.sort_by_key(|i| crate::fuzzy::normalise(&i.view.title)),
        "title_desc" => {
            items.sort_by_key(|i| crate::fuzzy::normalise(&i.view.title));
            items.reverse();
        }
        "added_asc" => items.sort_by_key(|i| i.added_at),
        "added_desc" => {
            items.sort_by_key(|i| i.added_at);
            items.reverse();
        }
        _ if !query.is_empty() => items.sort_by(|a, b| {
            b.rank.cmp(&a.rank).then_with(|| {
                crate::fuzzy::normalise(&a.view.title).cmp(&crate::fuzzy::normalise(&b.view.title))
            })
        }),
        _ => {}
    }
    items.into_iter().map(|i| i.view).collect()
}

/// Root folders for the bulk picker: **every** one, whatever it serves.
///
/// Not [`root_folder_options`], which filters by a single media type. A
/// bulk selection can hold movies and series at once, so filtering here
/// is a guess — and hard-coding it to `Tv` is what made the movie root
/// unreachable from the library screen entirely.
async fn bulk_root_options(state: &AppState) -> Result<Vec<(String, String)>, AppError> {
    Ok(all_root_folder_options(state)
        .await?
        .into_iter()
        .map(|(path, label, _)| (path, label))
        .collect())
}

/// Split `ids` into the ones `root` can serve and a count of the rest.
///
/// A root with no media type serves either, so nothing is ever refused
/// against it. One query for the whole catalogue rather than one per id
/// — the selection can be the entire library.
async fn split_by_root_compatibility(
    state: &AppState,
    ids: &[Uuid],
    root: &str,
) -> Result<(Vec<Uuid>, usize), AppError> {
    let serves = root_folders::list_all(state.pool())
        .await?
        .into_iter()
        .find(|f| f.path.to_string_lossy() == root)
        .and_then(|f| f.media_type);
    let Some(serves) = serves else {
        return Ok((ids.to_vec(), 0));
    };

    let kinds: std::collections::HashMap<Uuid, library::MediaType> = library::list(state.pool())
        .await?
        .into_iter()
        .map(|i| (i.id, i.media_type))
        .collect();
    let ok: Vec<Uuid> = ids
        .iter()
        .copied()
        // An id the catalogue no longer has is left to the setter, which
        // already treats a missing row as a no-op.
        .filter(|id| kinds.get(id).is_none_or(|k| *k == serves))
        .collect();
    let skipped = ids.len() - ok.len();
    Ok((ok, skipped))
}

/// Every root folder, whatever it serves.
///
/// The bulk picker uses this rather than [`root_folder_options`]: a
/// selection can hold movies and series at once, so filtering it by one
/// media type is a guess. It was hard-coded to `Tv`, which simply hid
/// the movie root from the list.
///
/// Safe to offer unfiltered because the labels already say what each one
/// serves, and [`library_bulk`] refuses to apply a root to a title it
/// does not serve.
async fn all_root_folder_options(
    state: &AppState,
) -> Result<Vec<(String, String, Option<crate::db::library::MediaType>)>, AppError> {
    use crate::db::library::MediaType;
    Ok(root_folders::list_all(state.pool())
        .await?
        .into_iter()
        .map(|f| {
            let path = f.path.to_string_lossy().into_owned();
            let label = match f.media_type {
                Some(MediaType::Movie) => format!("{path} (filmes)"),
                Some(MediaType::Tv) => format!("{path} (séries)"),
                None => format!("{path} (qualquer)"),
            };
            (path, label, f.media_type)
        })
        .collect())
}

/// Accept a root-folder path only if it is one brarr actually knows.
///
/// A path arriving from a page is not trusted just because the page
/// offered it: the form is the operator's, but the request is anybody's,
/// and this value ends up as a directory brarr writes files into.
async fn validated_root_folder(state: &AppState, path: &str) -> Result<String, AppError> {
    let known = root_folders::list_all(state.pool())
        .await?
        .into_iter()
        .any(|f| f.path == std::path::Path::new(path));
    if !known {
        return Err(AppError::InvalidInput(format!(
            "{path} não é uma pasta raiz cadastrada"
        )));
    }
    Ok(path.to_owned())
}

/// Map episode rows into the shared partial. Used both by the season
/// expand and by the single-row swap after an episode toggle.
///
/// `grabs` is the item's whole history, failed and vanished rows
/// included — [`crate::coverage::episode_mark`] needs them to tell
/// "never had it" from "had it and the file is gone", which are
/// different problems with different fixes.
/// `numbering` is the translation in force, canonical → what releases
/// call it. **The row leads with the coordinate brarr actually searches
/// for**, because that is the number the operator has to reconcile
/// against a release name, a file on disk and everything the scene
/// publishes. The catalogue's own number stays visible beside it —
/// dropping it would make the row impossible to line up against the
/// season it lives in, which is still TMDB's.
fn episode_views(
    episodes: &[library::Episode],
    grabs: &[grabs::Grab],
    progress: &crate::ttl_cache::TtlCache<Uuid, u8>,
) -> Vec<EpisodeView> {
    let now = OffsetDateTime::now_utc();
    let read_at = Instant::now();
    episodes
        .iter()
        .map(|e| {
            let mark = crate::coverage::episode_mark(e.id, e.season_number, e.air_date, grabs, now);
            // Cache read, never a client call: `queue::snapshot` is one
            // HTTP request per in-flight grab, and paying that per season
            // expand is how a screen becomes slower the more it has to
            // report.
            let percent = mark.grab_id.and_then(|id| progress.get(&id, read_at));
            EpisodeView {
                percent,
                id: e.id.to_string(),
                code: format!("S{:02}E{:02}", e.season_number, e.episode_number),
                season_number: e.season_number,
                episode_number: e.episode_number,
                title: e.title.clone().unwrap_or_else(|| "—".to_owned()),
                air_date: e.air_date.map_or_else(String::new, short_date),
                monitored: e.monitored,
                state_tone: mark.state.tone().to_owned(),
                state_label: mark.state.label().to_owned(),
                file_name: base_name(&mark.detail),
                detail: mark.detail,
            }
        })
        .collect()
}

/// Last path component, for the inline hint next to a row. The full path
/// stays in the tooltip — it is the half that answers "which mount?".
fn base_name(path: &str) -> String {
    if path.is_empty() {
        return String::new();
    }
    path.rsplit(['/', '\\']).next().unwrap_or(path).to_owned()
}

/// `GET /library/{id}/season/{season_id}` — episode rows, fetched when
/// the operator actually opens the season.
async fn library_season(
    State(state): State<AppState>,
    Path((id, season_id)): Path<(String, String)>,
) -> Result<Response, AppError> {
    let item_uuid = Uuid::parse_str(&id)
        .map_err(|e| AppError::InvalidInput(format!("invalid library id: {e}")))?;
    let season_uuid = Uuid::parse_str(&season_id)
        .map_err(|e| AppError::InvalidInput(format!("invalid season id: {e}")))?;

    let seasons = library::seasons(state.pool(), item_uuid).await?;
    let season = seasons
        .iter()
        .find(|s| s.id == season_uuid)
        .ok_or_else(|| AppError::NotFound(format!("library_season {season_id}")))?;

    let all = library::episodes(state.pool(), item_uuid).await?;
    let episodes: Vec<library::Episode> = all
        .into_iter()
        .filter(|e| e.season_number == season.season_number)
        .collect();
    let history = grabs::for_item(state.pool(), item_uuid).await?;

    // Refresh the cache from the client before rendering, but only for
    // this item and only when a row is actually going to show a number.
    // `/queue` probes live on every render, so without this the same
    // download read two different percentages and the detail screen's
    // was always the older one — up to a full `SYNC_INTERVAL` behind.
    //
    // Best-effort: a client that will not answer must not take the
    // season tree down with it. The row falls back to the cached value,
    // or to no number at all.
    if history.iter().any(|g| {
        matches!(
            g.status,
            grabs::GrabStatus::Sent | grabs::GrabStatus::Downloading
        )
    }) && let Err(e) = crate::queue::refresh_progress_for_item(&state, item_uuid).await
    {
        tracing::debug!(
            target: "brarr_orchestrator::web",
            item = %item_uuid,
            error = %e,
            "could not refresh download progress; rendering from the cache"
        );
    }

    let views = episode_views(&episodes, &history, state.progress());
    // Poll only while something is moving, and let the response decide —
    // same server-driven cadence as `/queue`. A season with nothing in
    // flight renders a plain wrapper and never asks again.
    let poll_secs = views
        .iter()
        .any(|e| e.state_tone == "busy")
        .then(|| crate::queue::LIVE_POLL_ACTIVE.as_secs());

    html(&LibrarySeasonPartial {
        item_id: id,
        season_id: Some(season_id),
        poll_secs,
        episodes: views,
        oob: None,
        // A plain expand changes nothing, so the hero stays as it is.
        item_status: None,
    })
}

/// `POST /library/{id}/season/{season_id}/monitor` — flip a season,
/// cascading to its episodes.
async fn library_season_monitor(
    State(state): State<AppState>,
    Path((id, season_id)): Path<(String, String)>,
) -> Result<Response, AppError> {
    let item_uuid = Uuid::parse_str(&id)
        .map_err(|e| AppError::InvalidInput(format!("invalid library id: {e}")))?;
    let season_uuid = Uuid::parse_str(&season_id)
        .map_err(|e| AppError::InvalidInput(format!("invalid season id: {e}")))?;
    let current = library::seasons(state.pool(), item_uuid)
        .await?
        .into_iter()
        .find(|s| s.id == season_uuid)
        .ok_or_else(|| AppError::NotFound(format!("library_season {season_id}")))?;
    let now_monitored = !current.monitored;
    library::set_season_monitored(state.pool(), season_uuid, now_monitored).await?;

    // The cascade rewrote every episode of the season, so answering with
    // the season's rows is what makes it believable: the operator sees
    // every bookmark move rather than being told it happened. The
    // season's own bookmark lives in the `<summary>`, outside this swap
    // target, so it rides along out-of-band.
    //
    // This used to be an `HX-Refresh`, which was correct and closed the
    // accordion the operator had just opened.
    let episodes: Vec<library::Episode> = library::episodes(state.pool(), item_uuid)
        .await?
        .into_iter()
        .filter(|e| e.season_number == current.season_number)
        .collect();
    let history = grabs::for_item(state.pool(), item_uuid).await?;

    // Re-read the season the same way the page built it, so the chip
    // that rides out-of-band agrees with the rows below it.
    let coverage = grabs::live_coverage_for_item(state.pool(), item_uuid).await?;
    let now = OffsetDateTime::now_utc();
    let monitored = monitored_rows(item_uuid, &episodes);
    let fresh = library::Season {
        monitored: now_monitored,
        ..current
    };
    let view = season_view(&fresh, &monitored, &coverage, now);

    html(&LibrarySeasonPartial {
        item_id: id,
        // A toggle re-renders the whole season body, so it carries the
        // wrapper — and the poll with it, if something is in flight.
        season_id: Some(season_id.clone()),
        poll_secs: None,
        episodes: episode_views(&episodes, &history, state.progress()),
        oob: Some(SeasonMarkView {
            season_id,
            monitored: now_monitored,
            tone: view.tone,
            status_label: view.status_label,
            monitored_count: view.monitored_count,
            have: view.have,
            percent: view.percent,
        }),
        item_status: Some(item_status_view(&state, item_uuid).await?),
    })
}

/// `POST /library/{id}/episode/{episode_id}/monitor` — flip one
/// episode and swap its row back.
async fn library_episode_monitor(
    State(state): State<AppState>,
    Path((id, episode_id)): Path<(String, String)>,
) -> Result<Response, AppError> {
    let item_uuid = Uuid::parse_str(&id)
        .map_err(|e| AppError::InvalidInput(format!("invalid library id: {e}")))?;
    let episode_uuid = Uuid::parse_str(&episode_id)
        .map_err(|e| AppError::InvalidInput(format!("invalid episode id: {e}")))?;

    let episodes = library::episodes(state.pool(), item_uuid).await?;
    let current = episodes
        .iter()
        .find(|e| e.id == episode_uuid)
        .ok_or_else(|| AppError::NotFound(format!("library_episode {episode_id}")))?;
    library::set_episode_monitored(state.pool(), episode_uuid, !current.monitored).await?;

    // Re-read so the swapped row reflects what was actually persisted.
    let updated = library::episodes(state.pool(), item_uuid)
        .await?
        .into_iter()
        .find(|e| e.id == episode_uuid)
        .ok_or_else(|| AppError::NotFound(format!("library_episode {episode_id}")))?;
    let history = grabs::for_item(state.pool(), item_uuid).await?;
    html(&LibrarySeasonPartial {
        item_id: id,
        // One row, swapped straight into `#ep-{id}`: no wrapper.
        season_id: None,
        poll_secs: None,
        episodes: episode_views(&[updated], &history, state.progress()),
        oob: None,
        // One episode moves the denominator too.
        item_status: Some(item_status_view(&state, item_uuid).await?),
    })
}

/// `GET /library/{id}/grabs` — the acquisition history, in a dialog.
async fn library_grabs(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Response, AppError> {
    let uuid = Uuid::parse_str(&id)
        .map_err(|e| AppError::InvalidInput(format!("invalid library id: {e}")))?;
    let item = library::get_by_id(state.pool(), uuid).await?;
    html(&LibraryGrabsModalPartial {
        title: item.title,
        grabs: grab_views(grabs::for_item(state.pool(), uuid).await?),
    })
}

/// Map acquisition rows for the history dialog.
fn grab_views(rows: Vec<grabs::Grab>) -> Vec<GrabView> {
    rows.into_iter()
        .map(|g| GrabView {
            id: g.id.to_string(),
            is_local: g.protocol == grabs::Protocol::Local,
            in_place: grabs::is_in_place(&g),
            release_name: g.release_name,
            provider_name: g.provider_name,
            protocol: g.protocol.label().to_owned(),
            status: g.status.label().to_owned(),
            tone: match g.status {
                grabs::GrabStatus::Imported | grabs::GrabStatus::Completed => "ok".to_owned(),
                grabs::GrabStatus::Failed | grabs::GrabStatus::Rejected => "err".to_owned(),
                grabs::GrabStatus::Reserved => "warn".to_owned(),
                _ => "neutral".to_owned(),
            },
            grabbed_at: short_date(g.grabbed_at),
            // A file that vanished after the import outranks the status
            // as the thing the operator needs to see on this row.
            error: match (&g.error, g.file_missing_at.is_some()) {
                (_, true) => Some(format!(
                    "arquivo não está mais em {}",
                    g.imported_path.as_deref().unwrap_or("disco")
                )),
                (other, false) => other.clone(),
            },
            file_missing: g.file_missing_at.is_some(),
        })
        .collect()
}

/// `POST /library/{id}/profile` — attach or detach a quality profile.
async fn library_set_profile(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Form(form): Form<LibraryProfileForm>,
) -> Result<Response, AppError> {
    let uuid = Uuid::parse_str(&id)
        .map_err(|e| AppError::InvalidInput(format!("invalid library id: {e}")))?;
    let item = library::get_by_id(state.pool(), uuid).await?;
    let profile = if form.profile_id.trim().is_empty() {
        None
    } else {
        Some(
            Uuid::parse_str(form.profile_id.trim())
                .map_err(|e| AppError::InvalidInput(format!("invalid profile id: {e}")))?,
        )
    };
    // A blank selection means "use the rule for this media type"; the
    // form only offers registered folders, and the importer refuses
    // anything that is not one of them anyway.
    let root_folder = match form.root_folder.as_deref().map(str::trim) {
        None | Some("") => None,
        Some(path) => Some(validated_root_folder(&state, path).await?),
    };
    // The item keeps whatever it had if the form did not carry the field
    // at all — an older cached page must not silently clear a placement.
    let root_folder = root_folder.or_else(|| {
        if form.root_folder.is_none() {
            item.root_folder.clone()
        } else {
            None
        }
    });
    library::set_placement(state.pool(), uuid, profile, root_folder.as_deref()).await?;
    Ok(Redirect::to(&format!("/library/{id}")).into_response())
}

/// Axis of a manual search: which season and episode, if any.
#[derive(Debug, Default, Deserialize)]
struct InteractiveQuery {
    #[serde(default)]
    season: Option<String>,
    #[serde(default)]
    episode: Option<String>,
}

impl InteractiveQuery {
    fn parsed(&self) -> (Option<u16>, Option<u16>) {
        let read = |v: &Option<String>| {
            v.as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .and_then(|s| s.parse::<u16>().ok())
        };
        (read(&self.season), read(&self.episode))
    }
}

/// `GET /library/{id}/interactive` — run a search the operator drives.
///
/// The automatic sweep picks the best release above the item's
/// threshold; this shows everything it found and lets the operator
/// choose — a specific provider, a season pack, a particular encode.
/// Nothing is graded away: the score column says which side of the
/// threshold a release fell on, and that is all it does.
async fn library_interactive(
    State(state): State<AppState>,
    Path(id): Path<String>,
    axum::extract::Query(q): axum::extract::Query<InteractiveQuery>,
) -> Result<Response, AppError> {
    let uuid = Uuid::parse_str(&id)
        .map_err(|e| AppError::InvalidInput(format!("invalid library id: {e}")))?;
    let item = library::get_by_id(state.pool(), uuid).await?;
    let (season, episode) = q.parsed();

    // The picker offers the catalogue's numbers and asks the indexer for
    // exactly those, which is only correct because the catalogue is now
    // numbered the way releases are. This screen used to be the one that
    // did *not* translate while the sweep did, so the magnifier asked for
    // `S01E47` while every release for it was named `S02E23`.
    let stored_ids = item_ids::for_item(state.pool(), uuid).await?;
    let keys = interactive_keys(&item, &stored_ids, season, episode);
    if !keys.has_any() {
        return html(&InteractiveResultsPartial {
            item_id: id,
            axis: String::new(),
            season: String::new(),
            episode: String::new(),
            results: Vec::new(),
            message: "este título não tem id externo utilizável para busca".to_string(),
        });
    }

    // Unfiltered on purpose: the automatic path drops releases every
    // profile rejects, and this screen exists precisely to show those.
    let outcome = crate::search::run_search_unfiltered(&state, keys).await?;
    let profile = match item.profile_id {
        Some(pid) => quality_profiles::get_by_id(state.pool(), pid).await.ok(),
        None => None,
    };
    let threshold = profile
        .as_ref()
        .map_or(crate::scan::DEFAULT_PUSH_THRESHOLD, |p| p.push_threshold);

    let mut results: Vec<InteractiveReleaseView> = outcome
        .decisions
        .iter()
        .map(|d| {
            let score = profile.as_ref().map_or(d.score, |p| {
                quality_profiles::effective_score(&d.profile_scores, d.score, p.id)
            });
            InteractiveReleaseView {
                id: d.id.to_string(),
                release_name: d.release_name.clone(),
                provider_name: d.provider_name.clone(),
                protocol: if d
                    .provider_kind
                    .as_deref()
                    .is_some_and(|k| k.eq_ignore_ascii_case("newznab"))
                {
                    "usenet".to_string()
                } else {
                    "torrent".to_string()
                },
                size: humanize_bytes(d.size_bytes),
                seeders: if d.seeders == 0 {
                    "—".to_string()
                } else {
                    d.seeders.to_string()
                },
                score,
                passes: score >= threshold,
                rejected: d.rejected,
                languages: audio_chips_from_languages(&d.audio_languages, &d.subtitle_languages),
                grabbable: d.download_url.is_some(),
            }
        })
        .collect();
    // Best first — the operator scans down from the top.
    results.sort_by_key(|r| std::cmp::Reverse(r.score));

    let axis = match (season, episode) {
        (Some(s), Some(e)) => format!("S{s:02}E{e:02}"),
        (Some(s), None) => format!("temporada {s} (pack)"),
        _ => String::new(),
    };
    let message = if results.is_empty() {
        "nenhuma release encontrada nos providers configurados".to_string()
    } else {
        String::new()
    };
    html(&InteractiveResultsPartial {
        item_id: id,
        axis,
        season: season.map(|s| s.to_string()).unwrap_or_default(),
        episode: episode.map(|e| e.to_string()).unwrap_or_default(),
        results,
        message,
    })
}

/// Search axis for a manual search. A series with a season asks the TVDB
/// axis (the only one that understands seasons); everything else asks
/// the movie axes the item carries.
fn interactive_keys(
    item: &crate::db::library::LibraryItem,
    ids: &[item_ids::StoredId],
    season: Option<u16>,
    episode: Option<u16>,
) -> crate::search::SearchKeys {
    let (axis, _) = crate::metadata::axis::resolve(ids, item.media_type);
    if let (Some(tvdb), Some(season)) = (axis.tvdb, season) {
        return crate::search::SearchKeys::from_tvdb(tvdb, Some(season), episode);
    }
    crate::search::SearchKeys {
        tmdb: axis.tmdb,
        imdb: axis.imdb,
        tvdb: axis.tvdb,
        ..crate::search::SearchKeys::default()
    }
}

/// `POST /library/{id}/grab/{decision_id}` — take this exact release.
///
/// The barrier still applies: the operator picking a release does not
/// make it safe to grab the same one twice. What is bypassed is the
/// *threshold*, because the whole point of the screen is choosing
/// something the automatic rules would not have.
async fn library_grab(
    State(state): State<AppState>,
    Path((id, decision_id)): Path<(String, String)>,
    axum::extract::Query(q): axum::extract::Query<InteractiveQuery>,
) -> Result<Response, AppError> {
    let item_uuid = Uuid::parse_str(&id)
        .map_err(|e| AppError::InvalidInput(format!("invalid library id: {e}")))?;
    let decision_uuid = Uuid::parse_str(&decision_id)
        .map_err(|e| AppError::InvalidInput(format!("invalid decision id: {e}")))?;
    let decision = decisions::get_by_id(state.pool(), decision_uuid).await?;
    let (season, episode_number) = q.parsed();

    // An episode number resolves to a row; a season with no episode is a
    // pack, which records the season and names no episode.
    let episode_id = match (season, episode_number) {
        (Some(s), Some(e)) => library::episodes(state.pool(), item_uuid)
            .await?
            .into_iter()
            .find(|row| row.season_number == i32::from(s) && row.episode_number == i32::from(e))
            .map(|row| row.id),
        _ => None,
    };
    let Some(provider_id) = decision.provider_id else {
        return Ok(html_string(render_status_badge(
            &format!("ir-result-{decision_id}"),
            &PingBadge {
                ok: false,
                label: "provider removido".to_string(),
                detail: "a release veio de um provider que não existe mais".to_string(),
            },
        )));
    };

    let new = grabs::NewGrab {
        item_id: item_uuid,
        episode_id,
        season_number: season.map(i32::from),
        decision_id: Some(decision.id),
        provider_id,
        provider_name: &decision.provider_name,
        release_id_remote: &decision.stable_release_key(),
        release_name: &decision.release_name,
        download_url: decision.download_url.as_deref(),
        protocol: if decision
            .provider_kind
            .as_deref()
            .is_some_and(|k| k.eq_ignore_ascii_case("newznab"))
        {
            grabs::Protocol::Usenet
        } else {
            grabs::Protocol::Torrent
        },
    };

    let badge = match grabs::reserve(state.pool(), &new).await? {
        None => PingBadge {
            ok: false,
            label: "já pego".to_string(),
            detail: "esta release já foi reservada ou tentada para este alvo".to_string(),
        },
        Some(grab) => match crate::deliver::deliver(&state, &grab).await? {
            crate::deliver::DeliveryOutcome::Sent { client_name, .. } => PingBadge {
                ok: true,
                label: "enviado".to_string(),
                detail: format!("entregue a {client_name}"),
            },
            other => PingBadge {
                ok: false,
                label: "falhou".to_string(),
                detail: other.reason().to_string(),
            },
        },
    };
    Ok(html_string(render_status_badge(
        &format!("ir-result-{decision_id}"),
        &badge,
    )))
}

/// `POST /library/{id}/refresh` — re-pull metadata.
///
/// TMDB for the descriptive half, and whoever owns the shape for the
/// tree. Those became two questions the day a title could be moved.
async fn library_refresh(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Response, AppError> {
    let uuid = Uuid::parse_str(&id)
        .map_err(|e| AppError::InvalidInput(format!("invalid library id: {e}")))?;
    let registry = Registry::build(state.pool()).await?;
    crate::tmdb_sync::refresh(state.pool(), &registry, uuid).await?;
    Ok(Redirect::to(&format!("/library/{id}")).into_response())
}

/// How long the "buscar agora" request waits for its own sweep before
/// answering "still running".
///
/// The sweep is spawned rather than awaited inline, and the wait is on
/// its `JoinHandle`, so a timeout here reports without cancelling
/// anything. A movie is one search and lands well inside the window; a
/// series is one search *per aired episode* — The Boys is 40 — and
/// holding an HTTP request open for that long would just hit the
/// browser's own timeout with nothing to show.
const MANUAL_SCAN_WAIT: Duration = Duration::from_secs(10);

/// `POST /library/{id}/scan` — "buscar agora" for one item.
///
/// Runs the same sweep the scheduler runs, minus the per-cycle cap: the
/// operator asked for this title specifically. Answers a small badge
/// with the counts so the detail page can report without a reload;
/// clicking it again after a grab lands is a no-op, because the grab now
/// covers the item.
async fn library_scan_now(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Response, AppError> {
    let uuid = Uuid::parse_str(&id)
        .map_err(|e| AppError::InvalidInput(format!("invalid library id: {e}")))?;
    let item = library::get_by_id(state.pool(), uuid).await?;

    // Claim the mailbox before spawning, so a status poll that arrives
    // between the spawn and the first await still sees "running" rather
    // than "nothing here".
    state
        .scans()
        .insert(uuid, ScanProgress::Running, Instant::now());

    let task_state = state.clone();
    let handle = tokio::spawn(async move {
        let outcome = crate::scan::run_once_for_item(&task_state, &item).await;
        // Record before returning. Past `MANUAL_SCAN_WAIT` the handler has
        // already dropped this `JoinHandle`, so the mailbox is the only
        // place the verdict survives.
        let progress = match &outcome {
            Ok(summary) => ScanProgress::Done(summary.clone()),
            Err(e) => ScanProgress::Failed(e.to_string()),
        };
        task_state.scans().insert(uuid, progress, Instant::now());
        outcome
    });
    let summary = match tokio::time::timeout(MANUAL_SCAN_WAIT, handle).await {
        Ok(Ok(Ok(summary))) => summary,
        Ok(Ok(Err(e))) => return Err(e),
        Ok(Err(join)) => {
            return Err(AppError::InvalidInput(format!("busca falhou: {join}")));
        }
        Err(_elapsed) => {
            // The task owns itself now. The badge it leaves behind asks
            // for the verdict on a timer instead of telling the operator
            // to reload — which was the one place in the app where a
            // background job with a known end surfaced as "press F5".
            return Ok(html_string(render_scan_running_badge(uuid)));
        }
    };

    let badge = scan_badge(&summary);
    Ok(html_string(render_status_badge(
        &format!("scan-{uuid}"),
        &badge,
    )))
}

/// Query for the narrowed sweep: `?season=3` or `?season=3&episode=7`.
#[derive(Debug, Deserialize)]
struct ScanScopeQuery {
    season: Option<i32>,
    episode: Option<i32>,
}

/// `POST /library/{id}/scan/target?season=&episode=` — the same sweep,
/// pointed at one season or one episode.
///
/// Answers the same badge as the item-wide button, so the two read
/// identically; it just reports against a `scan-season-…` /
/// `scan-ep-…` slot instead.
async fn library_scan_target(
    State(state): State<AppState>,
    Path(id): Path<String>,
    axum::extract::Query(q): axum::extract::Query<ScanScopeQuery>,
) -> Result<Response, AppError> {
    let uuid = Uuid::parse_str(&id)
        .map_err(|e| AppError::InvalidInput(format!("invalid library id: {e}")))?;
    let item = library::get_by_id(state.pool(), uuid).await?;

    let Some(season) = q.season else {
        return Err(AppError::InvalidInput(
            "busca por alvo precisa de uma temporada".to_owned(),
        ));
    };
    let scope = q.episode.map_or(crate::scan::Scope::Season(season), |ep| {
        crate::scan::Scope::Episode(season, ep)
    });
    let dom_id = q.episode.map_or_else(
        || format!("scan-season-{uuid}-{season}"),
        |ep| format!("scan-ep-{uuid}-{season}-{ep}"),
    );

    // Narrow enough to answer inline: one season is a handful of
    // searches, not the forty a series-wide sweep can be. No spawn, no
    // mailbox — the operator gets the verdict in the response.
    let summary = crate::scan::run_once_for_target(&state, &item, scope).await?;
    Ok(html_string(render_status_badge(
        &dom_id,
        &scan_badge(&summary),
    )))
}

/// `GET /library/{id}/placement` — the profile + root folder dialog.
///
/// Reads the same two lists the detail page reads; the form posts to the
/// existing `/library/{id}/profile` handler, so this route only renders.
async fn library_placement(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Response, AppError> {
    let uuid = Uuid::parse_str(&id)
        .map_err(|e| AppError::InvalidInput(format!("invalid library id: {e}")))?;
    let item = library::get_by_id(state.pool(), uuid).await?;

    let profiles = quality_profiles::list_all(state.pool())
        .await?
        .into_iter()
        .map(|p| (p.id.to_string(), p.name))
        .collect();
    let root_folders = root_folder_options(&state, item.media_type).await?;

    html(&crate::web::templates::LibraryPlacementModalPartial {
        item_id: item.id.to_string(),
        item_title: item.title,
        profiles,
        profile_id: item.profile_id.map(|p| p.to_string()).unwrap_or_default(),
        root_folders,
        root_folder: item.root_folder.unwrap_or_default(),
    })
}

/// `GET /pause-banner` — the strip that says brarr is switched off.
///
/// **A forgotten pause is the worst shape a defect can take**: every
/// screen keeps working, nothing errors, and the operator concludes the
/// feature is broken. So it is loud, it is on every page, and it
/// re-asks — a pause set from another tab shows up here within the
/// minute.
///
/// Fetched rather than rendered inline because `base.html` is inherited
/// by every template in the app, and threading a flag through all of
/// them to say one sentence is a change to thirty structs for nothing.
async fn pause_banner(State(state): State<AppState>) -> Response {
    if !settings::is_paused(state.pool()).await {
        // Still re-asks: the operator may pause from another tab.
        return html_string(
            "<div id=\"pause-banner\" hx-get=\"/pause-banner\" \
             hx-trigger=\"every 60s\" hx-swap=\"outerHTML\"></div>"
                .to_owned(),
        );
    }
    html_string(
        "<div id=\"pause-banner\" hx-get=\"/pause-banner\" hx-trigger=\"every 20s\" \
         hx-swap=\"outerHTML\" \
         class=\"app-shell px-8 py-3\">\
           <div class=\"px-4 py-3 rounded-md bg-warning-soft text-warning-soft-fg text-sm\">\
             <strong>O brarr está pausado.</strong> Nada é buscado, baixado, importado ou \
             vinculado. A leitura continua normal. \
             <a href=\"/settings?s=acesso\" class=\"underline\">Retomar em Configurações</a>.\
           </div>\
         </div>"
            .to_owned(),
    )
}

/// `GET /library/{id}/sources` — who owns this series' shape, and what
/// else is on offer.
///
/// Replaces the numbering panel, and the difference is the sentence that
/// can no longer be said. That screen promised "nada é renumerado na
/// biblioteca" and kept its word: it stored a translation beside the
/// tree and moved only the coordinate that went to the indexer. This one
/// **rebuilds the tree** in the chosen source's numbering — which is
/// what makes the translation stop having a subject, and what obliges
/// the screen to show what the write would do before doing it.
async fn library_sources(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Response, AppError> {
    let item = series_for_structure(&state, &id).await?;
    sources_panel(&state, &item, None, None).await
}

/// Read the item and refuse anything that is not a series.
///
/// A film has no tree, and a structure source recorded for one is a
/// claim nothing can honour.
async fn series_for_structure(
    state: &AppState,
    id: &str,
) -> Result<library::LibraryItem, AppError> {
    let uuid = Uuid::parse_str(id)
        .map_err(|e| AppError::InvalidInput(format!("invalid library id: {e}")))?;
    let item = library::get_by_id(state.pool(), uuid).await?;
    if item.media_type != library::MediaType::Tv {
        return Err(AppError::InvalidInput(
            "só séries têm estrutura de temporadas".to_owned(),
        ));
    }
    Ok(item)
}

/// Render the structure panel. Shared by the read route and by every
/// answer that shows it again, so a form error and a fresh open cannot
/// drift apart — the same discipline the numbering panel had.
async fn sources_panel(
    state: &AppState,
    item: &library::LibraryItem,
    preview: Option<crate::web::templates::StructurePreview>,
    error: Option<String>,
) -> Result<Response, AppError> {
    let pool = state.pool();
    let owner = structure::owner(pool, item.id).await?;
    let registry = Registry::build(pool).await?;
    let ids = item_ids::for_item(pool, item.id).await?;
    let stored = library::episodes(pool, item.id).await?;

    let mut options: Vec<crate::web::templates::SourceOption> = Vec::new();
    let mut unavailable: Vec<String> = Vec::new();

    for provider in registry.for_structure(MediaType::Tv) {
        let source = provider.source();
        let Some(known) = ids.iter().find(|s| s.id.source() == source) else {
            // Never guessed. A series brarr holds only under TMDB has no
            // TheTVDB id to search with either, and saying so is more
            // use than an option that fails when clicked.
            unavailable.push(format!(
                "{} não aparece aqui: o título não guarda um id dela.",
                source.display_name()
            ));
            continue;
        };

        options.push(source_option(
            &owner,
            source,
            OrderingFamily::Default,
            "",
            "ordenação própria",
            None,
        ));

        match provider.variants(&known.id).await {
            Ok(found) => {
                for v in found {
                    options.push(source_option(
                        &owner,
                        source,
                        v.family,
                        &v.handle,
                        &v.name,
                        v.coverage
                            .and_then(|(covered, _)| i32::try_from(covered).ok()),
                    ));
                }
            }
            // Reported, never swallowed: a provider that could not be
            // asked is different from one with nothing to offer, and the
            // panel must not spell them the same way.
            Err(e) => unavailable.push(format!("{}: {e}", source.display_name())),
        }
    }

    let mut counts: BTreeMap<i32, i32> = BTreeMap::new();
    for e in &stored {
        if e.season_number > 0 {
            *counts.entry(e.season_number).or_default() += 1;
        }
    }
    let declared = structure::ordering_of(&owner);
    let sizes_in_force: BTreeMap<i32, String> = match &declared {
        Ordering::Manual { blocks } => {
            let mut per: BTreeMap<i32, Vec<String>> = BTreeMap::new();
            for b in blocks {
                per.entry(b.season).or_default().push(b.size.to_string());
            }
            per.into_iter().map(|(k, v)| (k, v.join(", "))).collect()
        }
        _ => BTreeMap::new(),
    };
    let seasons = counts
        .into_iter()
        .map(
            |(season, episodes)| crate::web::templates::NumberingSeasonRow {
                season,
                episodes,
                sizes: sizes_in_force.get(&season).cloned().unwrap_or_default(),
                first_season: season,
            },
        )
        .collect();

    html(&crate::web::templates::LibrarySourcesModalPartial {
        item_id: item.id.to_string(),
        item_title: item.title.clone(),
        episodes: i32::try_from(stored.len()).unwrap_or(0),
        current_source: owner.source.map(|s| s.display_name().to_owned()),
        current_ordering: ordering_label(&declared, owner.handle.as_deref()),
        pinned: owner.pinned,
        options,
        unavailable,
        seasons,
        preview,
        error,
        descriptive_current: item
            .descriptive_source
            .unwrap_or(brarr_core::MetadataSource::Tmdb)
            .display_name()
            .to_owned(),
        descriptive_options: descriptive_options(&registry, item, &ids),
    })
}

/// The providers that could describe this title.
///
/// Filtered on two things, and both matter: the provider has to declare
/// it describes this media kind — TheTVDB has films and this client does
/// not read them — and the item has to carry an id that provider answers
/// to. An option that fails when clicked is worse than one that is not
/// offered.
fn descriptive_options(
    registry: &Registry,
    item: &library::LibraryItem,
    ids: &[item_ids::StoredId],
) -> Vec<crate::web::templates::DescriptiveOption> {
    let current = item
        .descriptive_source
        .unwrap_or(brarr_core::MetadataSource::Tmdb);
    brarr_core::MetadataSource::all()
        .filter(|source| {
            registry
                .get(*source)
                .is_some_and(|p| p.capabilities().descriptive.covers(item.media_type))
                && ids.iter().any(|stored| stored.id.source() == *source)
        })
        .map(|source| crate::web::templates::DescriptiveOption {
            value: source.label().to_owned(),
            label: source.display_name().to_owned(),
            current: source == current,
        })
        .collect()
}

/// `POST /library/{id}/descriptive` — move the descriptive facet.
///
/// **Applied immediately, unlike a structure choice.** There is no
/// preview and no gate because there is nothing to lose: the write
/// touches title, synopsis, artwork and status, all of which the next
/// refresh would rewrite anyway. Moving the *tree* is the one that
/// re-points acquisitions, and that one keeps its preview.
///
/// A provider with no poster does not blank the one already stored, so a
/// title that moves to a source without artwork keeps the image it has —
/// and keeps it readable, because the stored source travels with it.
async fn library_descriptive(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Form(form): Form<DescriptiveForm>,
) -> Result<Response, AppError> {
    let uuid = Uuid::parse_str(&id)
        .map_err(|e| AppError::InvalidInput(format!("invalid library id: {e}")))?;
    let source = brarr_core::MetadataSource::parse(form.source.trim())
        .ok_or_else(|| AppError::InvalidInput(format!("fonte desconhecida: {}", form.source)))?;

    library::set_descriptive_source(state.pool(), uuid, source).await?;

    let registry = Registry::build(state.pool()).await?;
    let refreshed = match crate::metadata::owned::description(state.pool(), &registry, uuid).await {
        Ok(described) => {
            library::apply_description(state.pool(), uuid, &described).await?;
            None
        }
        // The choice is recorded either way: a provider that is down
        // must not cost the operator the decision, and the next refresh
        // picks it up. Saying so beats a silent half-success.
        Err(e) => Some(format!(
            "fonte trocada, mas a atualização falhou: {e}. A próxima varredura tenta de novo."
        )),
    };

    let item = library::get_by_id(state.pool(), uuid).await?;
    if item.media_type != library::MediaType::Tv {
        return Ok(hx_refresh());
    }
    sources_panel(&state, &item, None, refreshed).await
}

/// What the descriptive picker posts.
#[derive(Debug, Deserialize)]
struct DescriptiveForm {
    source: String,
}

/// One offered ordering, marked active when it is the one in force.
fn source_option(
    owner: &library::StructureOwner,
    source: MetadataSource,
    family: OrderingFamily,
    handle: &str,
    name: &str,
    coverage: Option<i32>,
) -> crate::web::templates::SourceOption {
    let handle_matches = if handle.is_empty() {
        owner.handle.is_none()
    } else {
        owner.handle.as_deref() == Some(handle)
    };
    crate::web::templates::SourceOption {
        source: source.label().to_owned(),
        source_name: source.display_name().to_owned(),
        family: family.label().to_owned(),
        handle: handle.to_owned(),
        name: name.to_owned(),
        coverage,
        active: owner.source == Some(source) && owner.family == Some(family) && handle_matches,
        renumbers: family.renumbers(),
    }
}

/// An ordering in words, for the sentence at the top of the panel.
fn ordering_label(ordering: &Ordering, handle: Option<&str>) -> String {
    match ordering {
        Ordering::Default => "ordenação própria da fonte".to_owned(),
        Ordering::Manual { blocks } => {
            format!("blocos declarados por você ({} no total)", blocks.len())
        }
        Ordering::Named { family, .. } => match handle {
            Some(h) => format!("{} ({h})", family.label()),
            None => family.label().to_owned(),
        },
    }
}

/// `POST /library/{id}/structure` — preview a choice, then commit it.
///
/// **Two passes through the same handler, and the first one writes
/// nothing.** A tree write re-points every acquisition hanging off the
/// item and every way it can go wrong is invisible on screen, so the
/// operator sees the real plan — computed against the real tree, fetched
/// from the real provider — before there is a button. A plan the gates
/// would refuse never grows one.
///
/// `Form<Vec<(String, String)>>` because the hand-declared cut carries
/// one field per season and `serde_urlencoded` collapses repeated keys,
/// the same reason `/library/bulk` takes its selection this way.
async fn library_structure(
    State(state): State<AppState>,
    Path(id): Path<String>,
    axum::extract::Form(fields): axum::extract::Form<Vec<(String, String)>>,
) -> Result<Response, AppError> {
    let item = series_for_structure(&state, &id).await?;
    let value = |key: &str| {
        fields
            .iter()
            .find(|(name, _)| name == key)
            .map_or("", |(_, v)| v.trim())
    };

    let Some(source) = MetadataSource::parse(value("source")) else {
        return sources_panel(
            &state,
            &item,
            None,
            Some("fonte desconhecida — recarregue o painel".to_owned()),
        )
        .await;
    };
    let family = OrderingFamily::parse(value("family")).unwrap_or(OrderingFamily::Default);
    let handle = value("handle").to_owned();
    let pinned = !value("pinned").is_empty();
    let confirm = value("confirm") == "1";

    let (ordering, recipe) = match family {
        OrderingFamily::Manual => match declared_cut(&fields) {
            Ok(recipe) => (
                Ordering::Manual {
                    blocks: recipe.blocks(),
                },
                Some(recipe),
            ),
            Err(why) => return sources_panel(&state, &item, None, Some(why)).await,
        },
        OrderingFamily::Default => (Ordering::Default, None),
        other => (
            Ordering::Named {
                family: other,
                handle: handle.as_str().into(),
            },
            None,
        ),
    };

    let pool = state.pool();
    let ids = item_ids::for_item(pool, item.id).await?;
    let Some(known) = ids.iter().find(|s| s.id.source() == source) else {
        let why = format!(
            "o título não guarda um id da {} — resolva o id antes de trocar a fonte",
            source.display_name()
        );
        return sources_panel(&state, &item, None, Some(why)).await;
    };

    let registry = Registry::build(pool).await?;
    let provider = match registry.require(source) {
        Ok(p) => p,
        Err(e) => return sources_panel(&state, &item, None, Some(e.to_string())).await,
    };
    // The real tree, not a description of one. A cut that does not add
    // up surfaces here, from `recut`, which is the only place that can
    // know the provider's own episode counts.
    let incoming = match provider.tree(&known.id, &ordering).await {
        Ok(tree) => tree,
        Err(e) => return sources_panel(&state, &item, None, Some(e.to_string())).await,
    };

    if confirm {
        let intent = structure::Intent::Choice { pinned, recipe };
        return match structure::apply_with(pool, item.id, &incoming, &intent).await {
            // The whole page: the tree behind the season accordion, the
            // coverage chips and the episode rows all just changed, and
            // a fragment would leave every one of them stale.
            Ok(_) => Ok(hx_refresh()),
            Err(e) => sources_panel(&state, &item, None, Some(e.to_string())).await,
        };
    }

    let plan = structure::plan(pool, item.id, &incoming).await?;
    let refusal = structure::refusal(&plan);
    let preview = crate::web::templates::StructurePreview {
        source_name: source.display_name().to_owned(),
        ordering_name: ordering_label(&ordering, ordering.handle()),
        paired: plan.pairs.len(),
        orphans: plan.orphans.len(),
        added: plan.added,
        grabs_at_risk: plan.grabs_at_risk(),
        stored_coverage: percent(plan.air_date_coverage.0),
        incoming_coverage: percent(plan.air_date_coverage.1),
        packs: plan
            .packs_affected
            .iter()
            .map(|p| crate::web::templates::StructurePreviewPack {
                season: p.season,
                was: p.was,
                now: p.now,
                grabs: p.grabs,
            })
            .collect(),
        would_commit: refusal.is_none(),
        refusal,
        source: source.label().to_owned(),
        family: family.label().to_owned(),
        handle,
        pinned,
    };
    sources_panel(&state, &item, Some(preview), None).await
}

/// A 0.0..=1.0 ratio as a whole percentage, for the screen.
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "a ratio of episode counts, rendered as a percentage"
)]
fn percent(ratio: f32) -> i32 {
    (ratio * 100.0).round() as i32
}

/// Read the hand-declared cut out of the form.
///
/// A season left blank is not cut, which is what lets an operator
/// declare the one season that is wrong without restating the series.
/// Every field blank is an error rather than an empty recipe: an empty
/// `Ordering::Manual` is the provider's own tree wearing a label that
/// says otherwise, and the next refresh would find nothing to re-apply.
fn declared_cut(fields: &[(String, String)]) -> Result<structure::Recipe, String> {
    let mut seasons: Vec<structure::RecipeSeason> = Vec::new();
    for (name, raw) in fields {
        let Some(number) = name.strip_prefix("sizes_") else {
            continue;
        };
        if raw.trim().is_empty() {
            continue;
        }
        let Ok(season) = number.parse::<i32>() else {
            continue;
        };
        let sizes = Block::parse_sizes(raw).map_err(|e| format!("temporada {season}: {e}"))?;
        if !sizes.is_empty() {
            seasons.push(structure::RecipeSeason { season, sizes });
        }
    }
    if seasons.is_empty() {
        return Err(
            "declare o tamanho de pelo menos um bloco, ou escolha uma ordenação acima".to_owned(),
        );
    }
    seasons.sort_by_key(|s| s.season);
    Ok(structure::Recipe { seasons })
}

/// How often the badge asks whether its sweep has finished.
///
/// Faster than the queue's cadence because this is one cheap in-memory
/// read, not a round of calls to the download clients — and because the
/// operator is looking straight at it, having just clicked.
const SCAN_POLL: Duration = Duration::from_secs(3);

/// htmx's "stop polling" status. Any 2xx is still swapped; this one also
/// cancels the trigger.
///
/// Right here and wrong on `/queue`: a sweep genuinely ends, and only a
/// new click starts another. A queue refills on its own.
const HX_STOP_POLLING: u16 = 286;

/// The badge shown while a sweep is still running — it replaces itself
/// every [`SCAN_POLL`] until the status route says it is done.
fn render_scan_running_badge(item: Uuid) -> String {
    let badge = PingBadge {
        ok: true,
        label: "buscando…".to_string(),
        detail: "a varredura continua em segundo plano; o resultado aparece aqui".to_string(),
    };
    render_status_badge_with(
        &format!("scan-{item}"),
        &badge,
        &format!(
            r#" hx-get="/library/{item}/scan/status" hx-trigger="every {}s" hx-swap="outerHTML""#,
            SCAN_POLL.as_secs()
        ),
    )
}

/// `GET /library/{id}/scan/status` — what the spawned sweep is doing.
///
/// Answers `286` on anything terminal so the badge stops asking. A
/// mailbox that has expired reads as "nothing running", which is the
/// truth by then: the sweep's durable record is its grabs.
async fn library_scan_status(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Response, AppError> {
    let uuid = Uuid::parse_str(&id)
        .map_err(|e| AppError::InvalidInput(format!("invalid library id: {e}")))?;

    let (body, done) = match state.scans().get(&uuid, Instant::now()) {
        Some(ScanProgress::Running) => (render_scan_running_badge(uuid), false),
        Some(ScanProgress::Done(summary)) => (
            render_status_badge(&format!("scan-{uuid}"), &scan_badge(&summary)),
            true,
        ),
        Some(ScanProgress::Failed(reason)) => {
            let badge = PingBadge {
                ok: false,
                label: "erro".to_string(),
                detail: reason,
            };
            (render_status_badge(&format!("scan-{uuid}"), &badge), true)
        }
        // Nothing running and nothing recent: leave the slot empty
        // rather than inventing a verdict, and stop asking.
        None => (
            format!(
                r#"<span id="scan-{uuid}" class="text-xs italic text-fg-muted"></span>"#,
                uuid = crate::web::templates::escape(&uuid.to_string())
            ),
            true,
        ),
    };

    let mut resp = html_string(body);
    if done {
        *resp.status_mut() = StatusCode::from_u16(HX_STOP_POLLING).unwrap_or(StatusCode::OK);
    }
    Ok(resp)
}

/// Turn a finished sweep into the badge that reports it.
///
/// Shared by the synchronous answer and the polled one, so a sweep that
/// beat [`MANUAL_SCAN_WAIT`] and one that did not read identically.
fn scan_badge(summary: &crate::scan::ScanSummary) -> PingBadge {
    // First, and above even the grab count: a paused brarr did nothing
    // for a reason the operator set and may well have forgotten. Every
    // other answer here would be a lie about the trackers.
    if summary.paused {
        return PingBadge {
            ok: false,
            label: "pausado".to_string(),
            detail: "o brarr está pausado em Configurações — nada é buscado, baixado ou importado"
                .to_string(),
        };
    }
    if summary.grabbed > 0 {
        PingBadge {
            ok: true,
            label: format!("{} grab(s)", summary.grabbed),
            detail: format!("{} alvo(s), {} busca(s)", summary.targets, summary.searches),
        }
    } else if let Some((target, reason)) = summary.failures.first() {
        PingBadge {
            ok: false,
            label: "erro".to_string(),
            detail: format!("{target}: {reason}"),
        }
    } else if summary.no_search_axis {
        PingBadge {
            ok: false,
            label: "sem id de busca".to_string(),
            // Name the id when brarr holds one it cannot use. "Atualize
            // os metadados" is the right advice for a title with nothing
            // at all and useless for one whose id is simply in a
            // namespace no indexer accepts — a distinction a third
            // provider makes common rather than rare.
            detail: if summary.axis_rejections.is_empty() {
                "a série não tem id TVDB, que é o eixo da busca por episódio — atualize os metadados"
                    .to_string()
            } else {
                summary.axis_rejections.join("; ")
            },
        }
    } else if summary.targets == 0 && summary.skipped_unmonitored > 0 {
        // The two honest answers to "I clicked and nothing happened".
        // Without them the badge said "nada encontrado", which is a lie
        // — nothing was even searched.
        PingBadge {
            ok: false,
            label: "pausado".to_string(),
            detail: format!(
                "{} episódio(s) neste alvo estão pausados; a varredura respeita o monitoramento. Use a lupa para buscar mesmo assim.",
                summary.skipped_unmonitored
            ),
        }
    } else if summary.targets == 0 && summary.skipped_unaired > 0 {
        PingBadge {
            ok: true,
            label: "ainda não estreou".to_string(),
            detail: format!(
                "{} episódio(s) neste alvo ainda não foram ao ar; não há o que procurar",
                summary.skipped_unaired
            ),
        }
    } else if summary.targets == 0 && summary.skipped_unreleased > 0 {
        // Kept apart from the unaired branch because the fix differs: an
        // episode is a date to wait for, a film may simply be carrying a
        // production status the operator can go and look at.
        PingBadge {
            ok: true,
            label: "ainda não estreou".to_string(),
            detail:
                "o filme ainda está em produção ou o lançamento digital não chegou; não há o que procurar"
                    .to_string(),
        }
    } else if summary.skipped_covered > 0 && summary.searches == 0 {
        PingBadge {
            ok: true,
            label: "já coberto".to_string(),
            detail: format!(
                "{} alvo(s) já têm grab; nada a buscar",
                summary.skipped_covered
            ),
        }
    } else if summary.exhausted > 0 {
        // The answer the operator actually hit: releases were found, they
        // did pass, and every one of them had already been tried and
        // marked `failed` on an earlier sweep — so the barrier refused
        // them all. Folded into "nada encontrado" this read as a tracker
        // problem, while the magnifier right beside it listed nine
        // releases. The fix for this is in the grab history, never in the
        // profile.
        PingBadge {
            ok: false,
            label: "releases esgotadas".to_string(),
            detail: format!(
                "{} alvo(s): todas as releases encontradas já foram tentadas e falharam antes. \
                 Veja o histórico de grabs do título — o motivo da falha está lá. \
                 A lupa mostra o que a busca encontra agora.",
                summary.exhausted
            ),
        }
    } else {
        PingBadge {
            ok: false,
            label: "nada encontrado".to_string(),
            // Two causes, and the sentence used to claim only the second
            // — which made it a lie whenever the providers simply had
            // nothing, and a worse one when they had plenty and the
            // barrier was the wall (that case is the branch above now).
            detail: format!(
                "{} busca(s): os providers não devolveram nada para este alvo, \
                 ou nada passou do threshold do perfil",
                summary.searches
            ),
        }
    }
}

/// `POST /library/verify` — reconcile the catalogue with the disk now.
///
/// The pass also runs on its own every six hours; this is the "I just
/// deleted something, notice" button. It is `stat` per imported file, so
/// it answers inline rather than needing the spawn-and-wait dance the
/// search button does.
/// Query for `GET /library/import`.
#[derive(Debug, Deserialize)]
struct ImportQuery {
    /// Folder to scan. Absent on the first open, when the handler falls
    /// back to a registered root.
    folder: Option<String>,
    /// Pin every row to one catalogue entry — how the dialog opens from
    /// a title's own page.
    item: Option<Uuid>,
    /// Show the ignored list instead of the folder.
    ignored: Option<String>,
    /// Read the folder now. Absent means navigate: the dialog opens on
    /// the browser so a visit costs one `read_dir`, not a full walk.
    scan: Option<String>,
}

/// `GET /library/import` — what importing this folder would do.
///
/// Writes nothing. The plan is rebuilt on every open and again on
/// confirm, so this is a report and never a promise.
async fn library_import(
    State(state): State<AppState>,
    axum::extract::Query(q): axum::extract::Query<ImportQuery>,
) -> Result<Response, AppError> {
    let folder = match q.folder.filter(|f| !f.trim().is_empty()) {
        Some(f) => f,
        None => default_import_folder(&state, q.item).await?,
    };
    html(
        &import_modal(
            &state,
            ImportView {
                folder,
                item: q.item,
                showing_ignored: q.ignored.is_some(),
                scan: q.scan.is_some(),
                error: None,
            },
        )
        .await?,
    )
}

/// Where the dialog points before the operator says otherwise: the
/// item's own root folder, else the first registered one. Empty when
/// none exists, which the dialog turns into a form error rather than a
/// scan of `/`.
async fn default_import_folder(state: &AppState, item: Option<Uuid>) -> Result<String, AppError> {
    if let Some(id) = item {
        let entry = library::get_by_id(state.pool(), id).await?;
        if let Some(root) = crate::import::resolve_root(state, &entry).await? {
            return Ok(root.path.to_string_lossy().to_string());
        }
    }
    let roots = root_folders::list_all(state.pool()).await?;
    Ok(roots
        .first()
        .map(|r| r.path.to_string_lossy().to_string())
        .unwrap_or_default())
}

/// Build the dialog. Shared by the open, the rescan and every action
/// that re-renders it.
/// What the dialog is being asked to show.
struct ImportView {
    /// Folder in the path field.
    folder: String,
    /// Item the dialog is pinned to.
    item: Option<Uuid>,
    /// Show the ignored list instead of the folder.
    showing_ignored: bool,
    /// Read the folder. `false` navigates instead — the dialog opens
    /// this way so a visit does not walk thousands of files to answer a
    /// question the operator has not asked yet.
    scan: bool,
    /// Message to show inside the dialog.
    error: Option<String>,
}

async fn import_modal(state: &AppState, ctx: ImportView) -> Result<ImportModalPartial, AppError> {
    let folder = ctx.folder.as_str();
    let item = ctx.item;
    let showing_ignored = ctx.showing_ignored;
    let error = ctx.error;
    let ignored = crate::db::ignored_paths::list(state.pool())
        .await?
        .into_iter()
        .map(|row| ImportIgnoredView {
            name: std::path::Path::new(&row.path)
                .file_name()
                .map_or_else(|| row.path.clone(), |n| n.to_string_lossy().to_string()),
            path: row.path,
        })
        .collect();
    let item_title = match item {
        Some(id) => Some(library::get_by_id(state.pool(), id).await?.title),
        None => None,
    };

    let mut view = ImportModalPartial {
        folder: folder.to_owned(),
        item_id: item.map(|i| i.to_string()),
        item_title,
        rows: Vec::new(),
        ready: 0,
        undecided: 0,
        covered: 0,
        over_cap: 0,
        max_files: crate::adopt::MAX_PREVIEW_FILES,
        ignored_here: 0,
        ignored,
        showing_ignored,
        browsing: !ctx.scan,
        entries: Vec::new(),
        parent: None,
        shortcuts: root_folders::list_all(state.pool())
            .await?
            .into_iter()
            .map(|r| ImportDirEntry {
                name: r.path.file_name().map_or_else(
                    || r.path.to_string_lossy().to_string(),
                    |n| n.to_string_lossy().to_string(),
                ),
                path: r.path.to_string_lossy().to_string(),
            })
            .collect(),
        oob: false,
        error,
    };
    if folder.trim().is_empty() {
        view.error = Some(
            "Informe o caminho da pasta a importar, ou escolha uma pasta raiz abaixo — em Docker, \
             o caminho é o de dentro do contêiner do brarr."
                .to_owned(),
        );
        return Ok(view);
    }
    if showing_ignored {
        return Ok(view);
    }

    if !ctx.scan {
        let here = std::path::Path::new(folder);
        view.parent = here
            .parent()
            .map(|p| p.to_string_lossy().to_string())
            .filter(|p| !p.is_empty());
        match crate::adopt::list_dirs(here).await {
            Ok(dirs) => {
                view.entries = dirs
                    .into_iter()
                    .map(|(name, path)| ImportDirEntry {
                        name,
                        path: path.to_string_lossy().to_string(),
                    })
                    .collect();
            }
            Err(AppError::InvalidInput(why)) => view.error = Some(why),
            Err(other) => return Err(other),
        }
        return Ok(view);
    }

    match crate::adopt::plan(state, std::path::Path::new(folder), item).await {
        Ok(plan) => {
            view.ready = plan.ready();
            view.undecided = plan.undecided();
            view.covered = plan.covered();
            view.over_cap = plan.over_cap;
            view.ignored_here = plan.ignored;
            view.rows = plan
                .files
                .into_iter()
                .enumerate()
                .map(|(idx, file)| import_row(idx, file))
                .collect();
        }
        // A folder that cannot be read is a form error, not a 500: the
        // operator retypes the path in the field that is already there.
        Err(AppError::InvalidInput(why)) => view.error = Some(why),
        Err(other) => return Err(other),
    }
    Ok(view)
}

fn import_row(idx: usize, file: crate::adopt::PlannedFile) -> ImportRowView {
    ImportRowView {
        idx,
        token: file.token,
        path: file.path.to_string_lossy().to_string(),
        name: file.name,
        size: humanize_bytes(file.size),
        item: file.item_id.map(|i| i.to_string()).unwrap_or_default(),
        title: file.title,
        is_series: file.is_series,
        season: file.season,
        episode_label: file.episode_label,
        reason: file.reason,
        effect: file.effect,
        covered: file.covered,
    }
}

/// `POST /library/import` — confirm the selection, or set it aside.
///
/// Two submit buttons share one form so both act on the same checkboxes;
/// `action` says which was pressed. Repeated `sel` and `fp` fields are
/// why this takes the raw pair list: `serde_urlencoded` hands every
/// repetition over, in order.
async fn library_import_submit(
    State(state): State<AppState>,
    Form(fields): Form<Vec<(String, String)>>,
) -> Result<Response, AppError> {
    let value = |key: &str| {
        fields
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.clone())
    };
    let folder = value("folder").unwrap_or_default();
    let item = value("item").and_then(|raw| Uuid::parse_str(&raw).ok());
    let ticked = selected_rows(&fields);
    let selected: std::collections::HashSet<&str> =
        ticked.iter().map(|(_, path)| path.as_str()).collect();

    if value("action").as_deref() == Some("ignore") {
        let paths: Vec<String> = ticked.iter().map(|(_, path)| path.clone()).collect();
        crate::db::ignored_paths::ignore(state.pool(), &paths).await?;
        return html(
            &import_modal(
                &state,
                ImportView {
                    folder,
                    item,
                    showing_ignored: false,
                    scan: true,
                    error: None,
                },
            )
            .await?,
        );
    }

    // Only rows the operator kept ticked, and only those that carried a
    // target. The fingerprint inside each token is what the re-plan
    // compares against, so a file that changed since the preview is
    // skipped rather than adopted under stale numbers.
    let picks: Vec<crate::adopt::Pick> = fields
        .iter()
        .filter(|(k, _)| k == "fp")
        .filter_map(|(_, v)| crate::adopt::Pick::decode(v))
        .filter(|pick| selected.contains(pick.path.as_str()))
        .collect();
    if picks.is_empty() {
        return html(
            &import_modal(
                &state,
                ImportView {
                    folder,
                    item,
                    showing_ignored: false,
                    scan: true,
                    error: Some("Nada marcado para importar.".to_owned()),
                },
            )
            .await?,
        );
    }

    let report = crate::adopt::commit(&state, std::path::Path::new(&folder), item, &picks).await?;
    html(&import_report(&report))
}

fn import_report(report: &crate::adopt::Report) -> ImportReportPartial {
    use crate::adopt::CommitStatus;
    ImportReportPartial {
        in_place: report.count(CommitStatus::InPlace),
        linked: report.count(CommitStatus::Linked),
        skipped: report.count(CommitStatus::Skipped),
        appeared: report.appeared,
        outcomes: report
            .outcomes
            .iter()
            .map(|o| ImportOutcomeView {
                name: o.name.clone(),
                label: match o.status {
                    CommitStatus::InPlace => "adotado no lugar".to_owned(),
                    CommitStatus::Linked => "vinculado".to_owned(),
                    CommitStatus::Skipped => "pulado".to_owned(),
                },
                detail: o.detail.clone(),
                skipped: o.status == CommitStatus::Skipped,
                linked: o.status == CommitStatus::Linked,
            })
            .collect(),
    }
}

/// Rows the operator ticked, as `(index, path)`, in DOM order.
///
/// The checkbox value is `{idx}|{path}`: the index rides along so a bulk
/// action can aim its out-of-band swap at the right row. `splitn(2, '|')`
/// because a path may legitimately contain a pipe.
///
/// **Every** reader of `sel` goes through here. Having one caller decode
/// the value and another compare it raw is exactly the bug that made
/// "Importar" answer "nada marcado" with fifty rows ticked, and would
/// have written `12|/midias/…` into `ignored_paths` — a row that matches
/// no file, so ignoring would have looked like it worked and done
/// nothing, forever.
fn selected_rows(fields: &[(String, String)]) -> Vec<(usize, String)> {
    fields
        .iter()
        .filter(|(k, _)| k == "sel")
        .filter_map(|(_, v)| {
            let mut parts = v.splitn(2, '|');
            let idx = parts.next()?.parse().ok()?;
            Some((idx, parts.next()?.to_owned()))
        })
        .collect()
}

/// Query shared by the pickers and the single-row render.
#[derive(Debug, Deserialize)]
struct PickQuery {
    /// Folder being imported — the authorisation boundary for `path`.
    folder: String,
    /// File the choice applies to.
    path: String,
    /// Row to swap when the operator picks.
    idx: usize,
    /// Item the whole dialog is pinned to, carried through so the
    /// re-rendered row keeps its links.
    item: Option<Uuid>,
    /// Catalogue entry chosen for *this file*.
    target: Option<Uuid>,
    /// Episode chosen for this file.
    episode: Option<Uuid>,
    /// Season shown by the episode picker.
    season: Option<i32>,
    /// Filter text in the title picker.
    q: Option<String>,
    /// Apply the choice to every ticked row instead of to one file.
    bulk: Option<String>,
}

/// `GET /library/import/pick-title` — the library, to choose from.
///
/// Only the library: adding a title is a different decision, with a root
/// folder, a profile and a monitoring scope attached, and burying that
/// inside an import is how those become invisible defaults.
async fn library_import_pick_title(
    State(state): State<AppState>,
    axum::extract::Query(q): axum::extract::Query<PickQuery>,
) -> Result<Response, AppError> {
    let all = library::list(state.pool()).await?;
    let total = all.len();
    let query = q.q.unwrap_or_default();
    let needle = query.trim().to_lowercase();

    let mut titles = Vec::new();
    for item in all {
        if !needle.is_empty() && !item.title.to_lowercase().contains(&needle) {
            continue;
        }
        let is_series = item.media_type == crate::db::library::MediaType::Tv;
        // What tells two similar entries apart: a series is described by
        // its tree, a movie by where it would land.
        let meta = if is_series {
            let episodes = library::episodes(state.pool(), item.id).await?;
            let seasons: std::collections::HashSet<i32> = episodes
                .iter()
                .filter(|e| e.season_number > 0)
                .map(|e| e.season_number)
                .collect();
            let aired = episodes.iter().filter(|e| e.season_number > 0).count();
            format!("{} temporada(s) · {aired} episódio(s)", seasons.len())
        } else {
            item.root_folder.clone().unwrap_or_default()
        };
        titles.push(PickTitleView {
            id: item.id.to_string(),
            title: item.title,
            year: item.year,
            is_series,
            meta,
        });
    }

    html(&ImportPickTitlePartial {
        file_name: file_name_of(&q.path),
        path: q.path,
        idx: q.idx,
        folder: q.folder,
        item_id: q.item.map(|i| i.to_string()),
        query,
        titles,
        total,
        bulk: q.bulk.is_some(),
    })
}

/// `GET /library/import/pick-episode` — the season's slots, free and
/// taken alike.
async fn library_import_pick_episode(
    State(state): State<AppState>,
    axum::extract::Query(q): axum::extract::Query<PickQuery>,
) -> Result<Response, AppError> {
    let target = q
        .target
        .ok_or_else(|| AppError::InvalidInput("escolha o título primeiro".to_owned()))?;
    let item = library::get_by_id(state.pool(), target).await?;
    let all = crate::adopt::episode_slots(&state, target, None).await?;
    let mut seasons: Vec<i32> = all.iter().map(|s| s.season).collect();
    seasons.sort_unstable();
    seasons.dedup();
    // Default to the first season rather than to everything: a long
    // series would otherwise open on a list of hundreds.
    let season = q.season.or_else(|| seasons.first().copied());

    let shown: Vec<&crate::adopt::EpisodeSlot> = all
        .iter()
        .filter(|s| season.is_none_or(|want| s.season == want))
        .collect();
    let free = shown.iter().filter(|s| !s.taken).count();

    html(&ImportPickEpisodePartial {
        file_name: file_name_of(&q.path),
        path: q.path,
        idx: q.idx,
        folder: q.folder,
        item_id: q.item.map(|i| i.to_string()),
        target_item: target.to_string(),
        target_title: item.title,
        seasons,
        season,
        bulk: q.bulk.is_some(),
        slots: shown
            .iter()
            .map(|s| PickEpisodeView {
                id: s.id.to_string(),
                code: format!("S{:02}E{:02}", s.season, s.number),
                title: s.title.clone().unwrap_or_else(|| "—".to_owned()),
                taken: s.taken,
            })
            .collect(),
        free,
    })
}

/// `GET /library/import/row` — one row, rebuilt after a picker assigned
/// a target.
///
/// Swapping a single row is what keeps the rest of the operator's work:
/// re-rendering the whole dialog would reset every checkbox and every
/// assignment already made.
async fn library_import_row(
    State(state): State<AppState>,
    axum::extract::Query(q): axum::extract::Query<PickQuery>,
) -> Result<Response, AppError> {
    let file = crate::adopt::plan_one(
        &state,
        std::path::Path::new(&q.folder),
        std::path::Path::new(&q.path),
        q.target,
        q.episode,
    )
    .await?;
    html(&ImportRowPartial {
        row: import_row(q.idx, file),
        folder: q.folder,
        item_id: q.item.map(|i| i.to_string()),
        // Targeted swap: the picker aimed at this row's id, so no
        // out-of-band marker is needed.
        oob: false,
    })
}

fn file_name_of(path: &str) -> String {
    std::path::Path::new(path)
        .file_name()
        .map_or_else(|| path.to_owned(), |n| n.to_string_lossy().to_string())
}

/// `POST /library/import/bulk` — one decision applied to every ticked
/// row.
///
/// The answer is a set of out-of-band row swaps rather than a fresh
/// dialog. Re-rendering the dialog would reset every checkbox and every
/// assignment the operator had already made, and nothing on the server
/// holds that state: the plan is rebuilt from disk on each request, on
/// purpose.
async fn library_import_bulk(
    State(state): State<AppState>,
    Form(fields): Form<Vec<(String, String)>>,
) -> Result<Response, AppError> {
    let value = |key: &str| {
        fields
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.clone())
    };
    let folder = value("folder").unwrap_or_default();
    let item = value("item").and_then(|raw| Uuid::parse_str(&raw).ok());
    let action = value("action").unwrap_or_default();
    let target = value("target").and_then(|raw| Uuid::parse_str(&raw).ok());
    let target =
        target.ok_or_else(|| AppError::InvalidInput("nenhum título escolhido".to_owned()))?;

    // Selected rows, in the order the form sent them — which is DOM
    // order, which is scan order. The sequence action depends on it.
    let selected = selected_rows(&fields);

    // For "sequence", walk the season's slots from the chosen one and
    // hand them out in order. Anything past the end of the season simply
    // gets nothing — better a row still asking than a row pointing at an
    // episode that does not exist.
    let mut queue: Vec<Uuid> = Vec::new();
    if action == "sequence" {
        let start = value("episode").and_then(|raw| Uuid::parse_str(&raw).ok());
        let slots = crate::adopt::episode_slots(&state, target, None).await?;
        if let Some(start) = start {
            let from = slots.iter().position(|s| s.id == start).unwrap_or(0);
            queue = slots[from..].iter().map(|s| s.id).collect();
        }
    }

    let mut body = String::new();
    for (offset, (idx, path)) in selected.iter().enumerate() {
        let episode = if action == "sequence" {
            queue.get(offset).copied()
        } else {
            None
        };
        let file = crate::adopt::plan_one(
            &state,
            std::path::Path::new(&folder),
            std::path::Path::new(path),
            Some(target),
            episode,
        )
        .await?;
        let partial = ImportRowPartial {
            row: import_row(*idx, file),
            folder: folder.clone(),
            item_id: item.map(|i| i.to_string()),
            oob: true,
        };
        body.push_str(&partial.render()?);
    }
    Ok(html_string(body))
}

/// `DELETE /library/adoption/{grab_id}` — take an adoption back.
///
/// The page reloads afterwards because undoing changes the grab
/// history, the coverage and the file check at once, and re-rendering
/// one row would leave the other two stale.
async fn library_adopt_undo(
    State(state): State<AppState>,
    Path(grab_id): Path<Uuid>,
) -> Result<Response, AppError> {
    let outcome = crate::adopt::undo(&state, grab_id).await?;
    info!(target: "brarr_orchestrator::web", grab = %grab_id, outcome = %outcome, "adoption undone");
    Ok(hx_refresh())
}

/// Form for `POST /library/import/unignore`.
#[derive(Debug, Deserialize)]
struct UnignoreForm {
    path: String,
    folder: String,
    item: Option<Uuid>,
}

/// `POST /library/import/unignore` — offer a path again.
async fn library_import_unignore(
    State(state): State<AppState>,
    Form(form): Form<UnignoreForm>,
) -> Result<Response, AppError> {
    crate::db::ignored_paths::unignore(state.pool(), &form.path).await?;
    html(
        &import_modal(
            &state,
            ImportView {
                folder: form.folder,
                item: form.item,
                showing_ignored: true,
                scan: true,
                error: None,
            },
        )
        .await?,
    )
}

async fn library_verify(State(state): State<AppState>) -> Result<Response, AppError> {
    let summary = crate::verify::run_once(&state).await?;
    let badge = if summary.checked == 0 {
        PingBadge {
            ok: true,
            label: "nada importado ainda".to_string(),
            detail: "não há arquivos para conferir".to_string(),
        }
    } else if summary.missing == 0 {
        PingBadge {
            ok: true,
            label: format!("{} ok", summary.checked),
            detail: "todos os arquivos importados continuam no lugar".to_string(),
        }
    } else {
        PingBadge {
            ok: false,
            label: format!("{} sumiram", summary.missing),
            detail: format!(
                "{} de {} arquivos não estão mais no disco; esses itens voltaram a ser procurados",
                summary.missing, summary.checked
            ),
        }
    };
    Ok(html_string(render_status_badge("verify-result", &badge)))
}

/// Empty 200 carrying `HX-Refresh`, which makes HTMX reload the page.
/// Used where returning a fragment would mean maintaining the same row
/// markup in two layouts.
fn hx_refresh() -> Response {
    let mut resp = StatusCode::OK.into_response();
    resp.headers_mut()
        .insert("HX-Refresh", HeaderValue::from_static("true"));
    resp
}

/// `GET /queue` — what the download clients are doing right now.
///
/// Every row is read live from its client. The background sync
/// ([`crate::queue`]) persists the state transitions; the numbers on
/// this page are never stored, so they cannot be stale.
async fn queue_index(State(state): State<AppState>) -> Result<Response, AppError> {
    let live = queue_live_view(&state).await?;
    html(&crate::web::templates::QueueTemplate {
        entries: live.entries,
        summary: live.summary,
        total_speed: live.total_speed,
        poll_secs: live.poll_secs,
    })
}

/// The fragment `/queue` polls. Same data, same template — the page
/// renders it through `{% include %}`, so the first paint and every
/// refresh cannot disagree.
async fn queue_live(State(state): State<AppState>) -> Result<Response, AppError> {
    html(&queue_live_view(&state).await?)
}

/// Read the clients and assemble the live view.
///
/// Picks the next poll interval as a side effect of counting the rows:
/// a queue with something moving asks again soon, one that is empty or
/// merely waiting on the importer asks again slowly.
async fn queue_live_view(
    state: &AppState,
) -> Result<crate::web::templates::QueueLiveTemplate, AppError> {
    let entries = crate::queue::snapshot(state).await?;
    // One query for every title in the queue rather than one per row.
    let titles: std::collections::HashMap<Uuid, String> = library::list(state.pool())
        .await?
        .into_iter()
        .map(|item| (item.id, item.title))
        .collect();

    let mut downloading = 0usize;
    let mut done = 0usize;
    let mut total_speed = 0u64;
    let views: Vec<_> = entries
        .iter()
        .map(|entry| {
            let view = queue_entry_view(entry, &titles);
            match view.tone.as_str() {
                "ok" => done += 1,
                "err" => {}
                _ => downloading += 1,
            }
            if let crate::queue::Probe::Known(status) = &entry.probe {
                total_speed += status.speed_bytes.unwrap_or(0);
            }
            view
        })
        .collect();

    let mut parts = Vec::new();
    if downloading > 0 {
        parts.push(format!("{downloading} em andamento"));
    }
    if done > 0 {
        parts.push(format!("{done} concluído(s)"));
    }
    let interval = if downloading > 0 {
        crate::queue::LIVE_POLL_ACTIVE
    } else {
        crate::queue::LIVE_POLL_IDLE
    };
    Ok(crate::web::templates::QueueLiveTemplate {
        entries: views,
        summary: parts.join(" · "),
        total_speed: if total_speed > 0 {
            format!("{}/s", humanize_bytes(total_speed))
        } else {
            String::new()
        },
        poll_secs: interval.as_secs(),
    })
}

fn queue_entry_view(
    entry: &crate::queue::QueueEntry,
    titles: &std::collections::HashMap<Uuid, String>,
) -> crate::web::templates::QueueEntryView {
    use crate::queue::Probe;
    use brarr_download_client::DownloadState;

    let (percent, speed, eta, size, status, tone, detail) = match &entry.probe {
        Probe::Known(s) => {
            let percent = (s.progress * 100.0).clamp(0.0, 100.0).round();
            #[allow(
                clippy::cast_possible_truncation,
                clippy::cast_sign_loss,
                reason = "clamped to 0..=100 on the line above"
            )]
            let percent = percent as u8;
            let (status, tone) = match s.state {
                DownloadState::Queued => ("na fila", "neutral"),
                DownloadState::Downloading => ("baixando", "info"),
                DownloadState::Completed => ("concluído", "ok"),
                DownloadState::Failed => ("falhou", "err"),
            };
            (
                percent,
                s.speed_bytes
                    .map(humanize_bytes)
                    .map_or_else(String::new, |b| format!("{b}/s")),
                s.eta_seconds.map_or_else(String::new, humanize_eta),
                s.size_bytes.map_or_else(String::new, humanize_bytes),
                status.to_string(),
                tone.to_string(),
                s.detail.clone(),
            )
        }
        // Nothing was learned, so the grab's own state is all there is.
        Probe::Unknown | Probe::Unreachable(_) => {
            let detail = match &entry.probe {
                Probe::Unreachable(why) => Some(why.clone()),
                _ => Some("o cliente não conhece este download".to_string()),
            };
            (
                0,
                String::new(),
                String::new(),
                String::new(),
                entry.grab.status.label().to_string(),
                "neutral".to_string(),
                detail,
            )
        }
    };

    crate::web::templates::QueueEntryView {
        title: titles
            .get(&entry.grab.item_id)
            .cloned()
            .unwrap_or_else(|| "(removido da biblioteca)".to_string()),
        item_id: entry.grab.item_id.to_string(),
        release_name: entry.grab.release_name.clone(),
        provider_name: entry.grab.provider_name.clone(),
        protocol: entry.grab.protocol.label().to_string(),
        client_name: entry.client_name.clone().unwrap_or_else(|| "—".to_string()),
        size,
        percent,
        speed,
        eta,
        status,
        tone,
        detail,
    }
}

/// `540` → `9 min restantes`.
fn humanize_eta(seconds: u64) -> String {
    if seconds < 60 {
        return format!("{seconds} s restantes");
    }
    let minutes = seconds / 60;
    if minutes < 60 {
        return format!("{minutes} min restantes");
    }
    let hours = minutes / 60;
    let rest = minutes % 60;
    format!("{hours} h {rest} min restantes")
}

/// `GET /health` — provider fan-out health + Torznab/Newznab endpoint
/// latency over the last 24h. Answers "which upstream API is the
/// bottleneck" (per-provider p95/timeouts) and "is the slowness in
/// brarr's endpoints or in the trackers" (endpoint latency vs provider
/// latency, cache hit rate).
async fn health_index(State(state): State<AppState>) -> Result<Response, AppError> {
    const WINDOW_HOURS: u32 = 24;
    let pool = state.pool();
    let since = OffsetDateTime::now_utc().unix_timestamp() - i64::from(WINDOW_HOURS) * 3600;

    let providers = crate::db::metrics::provider_stats(pool, since)
        .await?
        .into_iter()
        .map(|s| ProviderHealthView {
            healthy: s.errors == 0 && s.timeouts == 0,
            name: s.provider_name,
            kind: s.provider_kind,
            total: s.total,
            ok: s.ok,
            errors: s.errors,
            timeouts: s.timeouts,
            avg_ms: s.avg_ms,
            p50_ms: s.p50_ms,
            p95_ms: s.p95_ms,
            max_ms: s.max_ms,
            releases: s.releases,
            last_error: s.last_error,
            last_seen: OffsetDateTime::from_unix_timestamp(s.last_seen_unix)
                .map(format_ts)
                .unwrap_or_default(),
        })
        .collect();

    let endpoints = crate::db::metrics::endpoint_stats(pool, since)
        .await?
        .into_iter()
        .map(|s| {
            let searches = s.cache_hits + s.cache_misses;
            EndpointHealthView {
                hit_rate_pct: (s.cache_hits * 100)
                    .checked_div(searches)
                    .and_then(|pct| u32::try_from(pct).ok())
                    .unwrap_or(0),
                endpoint: s.endpoint,
                function: s.function,
                total: s.total,
                errors: s.errors,
                cache_hits: s.cache_hits,
                cache_misses: s.cache_misses,
                avg_ms: s.avg_ms,
                p50_ms: s.p50_ms,
                p95_ms: s.p95_ms,
                max_ms: s.max_ms,
            }
        })
        .collect();

    let recent = crate::db::metrics::recent_endpoint_requests(pool, 30)
        .await?
        .into_iter()
        .map(|r| EndpointRequestView {
            recorded_at: format_ts(r.recorded_at),
            // Redirects (3xx) are the download proxy's success path.
            ok: r.status < 400,
            endpoint: r.endpoint,
            function: r.function,
            status: r.status,
            duration_ms: r.duration_ms,
            cache: match r.cache_hit {
                Some(true) => "hit".to_string(),
                Some(false) => "miss".to_string(),
                None => "—".to_string(),
            },
        })
        .collect();

    html(&HealthTemplate {
        window_hours: WINDOW_HOURS,
        providers,
        endpoints,
        recent,
    })
}

/// Clamp a webhook payload to a bounded preview for the expandable
/// detail cell. Real Connect payloads are a few KiB; we show the head.
fn truncate_payload(payload: &str) -> String {
    const MAX: usize = 600;
    if payload.len() <= MAX {
        return payload.to_string();
    }
    let mut end = MAX;
    while !payload.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &payload[..end])
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
        webhook_driven: row.webhook_driven,
        sync_source: row.sync_source,
        synced_at: row.synced_at.and_then(|t| t.format(&Iso8601::DEFAULT).ok()),
        // Filled by `fill_webhook_urls` at the *arr-table render sites
        // (needs the request origin + auth token, which the row lacks).
        webhook_url: String::new(),
        webhook_has_token: false,
        created_at: row
            .created_at
            .format(&Iso8601::DEFAULT)
            .unwrap_or_else(|_| String::from("?")),
    }
}

/// Stamp the ready-to-paste inbound webhook URL onto each *arr row.
/// `headers` is `Some` on the full-page handler (so the origin can be
/// derived from `Host` when no public URL is configured) and `None` on
/// the HTMX partials (which fall back to the configured public URL).
fn fill_webhook_urls(
    state: &AppState,
    headers: Option<&axum::http::HeaderMap>,
    instances: &mut [ArrInstanceView],
) {
    let base = match headers {
        Some(h) => crate::push::derive_request_base(state, h),
        None => crate::push::state_public_base_url(state).unwrap_or_default(),
    };
    let base = base.trim_end_matches('/');
    let token = state.auth_token_owned();
    let token = token.as_deref().filter(|t| !t.is_empty());
    for v in instances.iter_mut() {
        v.webhook_has_token = token.is_some();
        v.webhook_url = match token {
            Some(t) => format!(
                "{base}/webhooks/{}/{}?apikey={}",
                v.kind,
                v.id,
                crate::auth::AuthConfig::encode_token_for_query(t)
            ),
            None => format!("{base}/webhooks/{}/{}", v.kind, v.id),
        };
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
    render_status_badge(&format!("ping-{provider_id}"), b)
}

/// Render one connectivity verdict as a `<span class="badge …">`.
///
/// Inline HTML — small enough that pulling it through Askama would add
/// more ceremony than value. `dom_id` has to match the id of the cell
/// the fragment replaces, so HTMX's `hx-target` still resolves on every
/// click after the first swap. Detail is escaped: it carries raw
/// upstream error text.
///
/// The colours are design tokens, not a palette scale. They used to be
/// `bg-emerald-100 text-emerald-800`, which Tailwind generated on
/// demand and the hand-authored stylesheet never defined — so from the
/// migration onward every one of these badges rendered uncoloured. The
/// `css_coverage` test only scanned templates, so nothing caught it;
/// it scans this file now too.
fn render_status_badge(dom_id: &str, b: &PingBadge) -> String {
    render_status_badge_with(dom_id, b, "")
}

/// Same badge with `extra` appended verbatim to the `<span>`'s
/// attributes.
///
/// `extra` is **not** escaped, so it is for attributes this file builds
/// — the polling wiring on a running scan — and never for anything
/// derived from a request.
fn render_status_badge_with(dom_id: &str, b: &PingBadge, extra: &str) -> String {
    let (bg, fg) = if b.ok {
        ("bg-success-soft", "text-success-soft-fg")
    } else {
        ("bg-danger-soft", "text-danger-soft-fg")
    };
    let detail = crate::web::templates::escape(&b.detail);
    let label = crate::web::templates::escape(&b.label);
    let dom_id = crate::web::templates::escape(dom_id);
    format!(r#"<span id="{dom_id}" class="badge {bg} {fg}" title="{detail}"{extra}>{label}</span>"#)
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
        Ok(r) => {
            if let Err(errs) = r.validate() {
                return Ok(html_string(format!(
                    r#"<p class="text-sm text-danger-soft-fg">Regras inválidas: {}</p>"#,
                    crate::web::templates::escape(&errs.join("; "))
                )));
            }
            brarr_decision_service::Engine::from_profile_rules(r)
        }
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
    // Reject malformed `title_matches` regexes before persisting so a
    // bad pattern never silently disables a leaf in production.
    if let Err(errs) = rules.validate() {
        let row = quality_profiles::get_by_id(state.pool(), uuid).await?;
        return html(&ProfileEditorTemplate {
            id: row.id.to_string(),
            name: form.name,
            description: form.description.unwrap_or_default(),
            push_threshold: form.push_threshold,
            is_preset: row.is_preset,
            rules_json: form.rules_json,
            error_message: Some(format!("Regras inválidas: {}", errs.join("; "))),
            preview_html: String::new(),
        });
    }
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

fn search_row_view(s: searches::SearchRow) -> RecentSearchView {
    RecentSearchView {
        id: s.id.to_string(),
        tmdb_id: s.tmdb_id.map_or_else(|| "-".to_string(), |v| v.to_string()),
        imdb_id: s.imdb_id.unwrap_or_else(|| "-".to_string()),
        tvdb_id: s.tvdb_id.map_or_else(|| "-".to_string(), |v| v.to_string()),
        season: s.season.map_or_else(String::new, |v| v.to_string()),
        episode: s.episode.map_or_else(String::new, |v| v.to_string()),
        submitted_at: format_ts(s.submitted_at),
        result_count: s.result_count,
    }
}

/// Query string for `GET /searches`. Every filter is optional; absent
/// or empty values don't constrain. `page` is 1-indexed, `size` is
/// clamped to [`MIN_SEARCHES_PAGE_SIZE`]..=200 with the page-size
/// `<select>` defaulting to 50.
#[derive(Debug, Default, Deserialize)]
struct SearchesIndexQuery {
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
    #[serde(default)]
    from: Option<String>,
    #[serde(default)]
    to: Option<String>,
    #[serde(default)]
    has_kept_decision: Option<String>,
    #[serde(default)]
    page: Option<u32>,
    #[serde(default)]
    size: Option<u32>,
}

const DEFAULT_SEARCHES_PAGE_SIZE: u32 = 50;
const MIN_SEARCHES_PAGE_SIZE: u32 = 10;
const MAX_SEARCHES_PAGE_SIZE: u32 = 200;

#[allow(
    clippy::similar_names,
    reason = "tmdb/tvdb/imdb are domain-canonical identifier triplets; renaming hurts readability more than the lint helps"
)]
async fn searches_index(
    State(state): State<AppState>,
    axum::extract::Query(q): axum::extract::Query<SearchesIndexQuery>,
) -> Result<Response, AppError> {
    let pool = state.pool();

    let tmdb = nonblank(q.tmdb_id.as_deref()).and_then(|s| s.parse::<u32>().ok());
    let imdb_raw = nonblank(q.imdb_id.as_deref()).map(str::to_string);
    let tvdb = nonblank(q.tvdb_id.as_deref()).and_then(|s| s.parse::<u32>().ok());
    let season = nonblank(q.season.as_deref()).and_then(|s| s.parse::<u16>().ok());
    let episode = nonblank(q.episode.as_deref()).and_then(|s| s.parse::<u16>().ok());
    let from_unix = nonblank(q.from.as_deref()).and_then(parse_iso_date_start);
    let to_unix = nonblank(q.to.as_deref()).and_then(parse_iso_date_end);
    let kept_choice = nonblank(q.has_kept_decision.as_deref()).unwrap_or("any");
    let has_kept_decision = match kept_choice {
        "yes" => Some(true),
        "no" => Some(false),
        _ => None,
    };

    let page = q.page.unwrap_or(1).max(1);
    let size = q
        .size
        .unwrap_or(DEFAULT_SEARCHES_PAGE_SIZE)
        .clamp(MIN_SEARCHES_PAGE_SIZE, MAX_SEARCHES_PAGE_SIZE);
    let offset = page.saturating_sub(1).saturating_mul(size);

    let params = searches::FilterParams {
        tmdb_id: tmdb,
        imdb_id: imdb_raw.clone(),
        tvdb_id: tvdb,
        season,
        episode,
        from_unix,
        to_unix,
        has_kept_decision,
        limit: size,
        offset,
    };

    let total_count = searches::count_filtered(pool, &params).await?;
    let rows = searches::filter(pool, params).await?;
    let recent_searches: Vec<RecentSearchView> = rows.into_iter().map(search_row_view).collect();

    let total_pages = {
        let size_u64 = u64::from(size);
        let raw = total_count.div_ceil(size_u64);
        u32::try_from(raw.max(1)).unwrap_or(u32::MAX)
    };
    let page = page.min(total_pages);
    let has_prev = page > 1;
    let has_next = page < total_pages;

    let filters = SearchesFilterView {
        tmdb_id: q.tmdb_id.clone().unwrap_or_default(),
        imdb_id: q.imdb_id.clone().unwrap_or_default(),
        tvdb_id: q.tvdb_id.clone().unwrap_or_default(),
        season: q.season.clone().unwrap_or_default(),
        episode: q.episode.clone().unwrap_or_default(),
        from_date: q.from.clone().unwrap_or_default(),
        to_date: q.to.clone().unwrap_or_default(),
        has_kept_decision: kept_choice.to_string(),
        page_size: size.to_string(),
    };

    let base_query = build_search_filter_query(&q, size);
    let prev_href = if has_prev {
        format!("/searches?{base_query}&page={}", page - 1)
    } else {
        String::new()
    };
    let next_href = if has_next {
        format!("/searches?{base_query}&page={}", page + 1)
    } else {
        String::new()
    };

    let tmpl = SearchesIndexTemplate {
        recent_searches,
        filters,
        page,
        total_pages,
        has_prev,
        has_next,
        prev_href,
        next_href,
        total_count,
    };
    html(&tmpl)
}

/// Convert `Option<&str>` to `Option<&str>` only when the trimmed
/// value is non-empty. Centralises the "empty form input = no
/// filter" treatment.
fn nonblank(s: Option<&str>) -> Option<&str> {
    s.map(str::trim).filter(|t| !t.is_empty())
}

/// Parse a `YYYY-MM-DD` form input to the Unix timestamp at the start
/// of that day (UTC). Used for the `from=` filter lower bound.
fn parse_iso_date_start(raw: &str) -> Option<i64> {
    let date = time::Date::parse(
        raw,
        time::macros::format_description!("[year]-[month]-[day]"),
    )
    .ok()?;
    Some(date.midnight().assume_utc().unix_timestamp())
}

/// Parse a `YYYY-MM-DD` form input to the Unix timestamp at the end
/// of that day (UTC, 23:59:59). Used for the `to=` filter upper
/// bound so the day boundary is inclusive.
fn parse_iso_date_end(raw: &str) -> Option<i64> {
    let date = time::Date::parse(
        raw,
        time::macros::format_description!("[year]-[month]-[day]"),
    )
    .ok()?;
    let next = date.next_day()?;
    Some(next.midnight().assume_utc().unix_timestamp() - 1)
}

/// Re-encode the filter portion of the search-index query so the
/// pagination links preserve every active filter except `page`.
fn build_search_filter_query(q: &SearchesIndexQuery, size: u32) -> String {
    let mut parts: Vec<String> = Vec::new();
    if let Some(v) = nonblank(q.tmdb_id.as_deref()) {
        parts.push(format!("tmdb_id={}", urlencoding(v)));
    }
    if let Some(v) = nonblank(q.imdb_id.as_deref()) {
        parts.push(format!("imdb_id={}", urlencoding(v)));
    }
    if let Some(v) = nonblank(q.tvdb_id.as_deref()) {
        parts.push(format!("tvdb_id={}", urlencoding(v)));
    }
    if let Some(v) = nonblank(q.season.as_deref()) {
        parts.push(format!("season={}", urlencoding(v)));
    }
    if let Some(v) = nonblank(q.episode.as_deref()) {
        parts.push(format!("episode={}", urlencoding(v)));
    }
    if let Some(v) = nonblank(q.from.as_deref()) {
        parts.push(format!("from={}", urlencoding(v)));
    }
    if let Some(v) = nonblank(q.to.as_deref()) {
        parts.push(format!("to={}", urlencoding(v)));
    }
    if let Some(v) = nonblank(q.has_kept_decision.as_deref()) {
        parts.push(format!("has_kept_decision={}", urlencoding(v)));
    }
    parts.push(format!("size={size}"));
    parts.join("&")
}

/// Minimal URL-component encoder for the small set of characters
/// these filter values may contain. Avoids pulling in a dedicated
/// crate for what's effectively numeric + ISO-date strings.
fn urlencoding(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for byte in s.bytes() {
        let safe = byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~' | b':');
        if safe {
            out.push(byte as char);
        } else {
            let _ = std::fmt::Write::write_fmt(&mut out, format_args!("%{byte:02X}"));
        }
    }
    out
}

// ---- settings page -------------------------------------------------

const MIN_POLL_INTERVAL_SECS: u64 = 60;

/// Which side-menu group `/settings` shows. Unknown or absent falls
/// back to the first one rather than an empty page.
#[derive(Debug, Default, Deserialize)]
struct SettingsQuery {
    #[serde(default)]
    s: Option<String>,
}

/// The groups the settings side menu offers, in order.
const SETTINGS_SECTIONS: &[&str] = &[
    "acesso",
    "automacao",
    "metadados",
    "dados",
    "integracao",
    "diagnostico",
];

fn settings_section(raw: Option<&str>) -> String {
    let wanted = raw.unwrap_or("").trim();
    SETTINGS_SECTIONS
        .iter()
        .find(|s| **s == wanted)
        .map_or_else(|| SETTINGS_SECTIONS[0].to_string(), |s| (*s).to_string())
}

async fn settings_index(
    State(state): State<AppState>,
    axum::extract::Query(q): axum::extract::Query<SettingsQuery>,
) -> Result<Response, AppError> {
    let values = load_settings_values(&state).await?;
    let tmpl = SettingsTemplate {
        section: settings_section(q.s.as_deref()),
        values,
        flash: None,
    };
    html(&tmpl)
}

async fn load_settings_values(state: &AppState) -> Result<SettingsValues, AppError> {
    let map = settings::get_all(state.pool()).await?;
    let get = |k: &str| -> String { map.get(k).cloned().unwrap_or_default() };
    // Build the indexer base URLs from the configured public URL (the
    // page tells the operator to set it when empty). Two separate base
    // URLs — torrents vs usenet — registered as distinct *arr indexers.
    let base = crate::push::state_public_base_url(state).unwrap_or_default();
    let base = base.trim_end_matches('/');
    let torznab_base = format!("{base}/torznab/api");
    let newznab_base = format!("{base}/newznab/api");
    let indexer_apikey = state.auth_token_owned().unwrap_or_default();
    let profiles = quality_profiles::list_all(state.pool())
        .await?
        .into_iter()
        .map(|p| (p.id.to_string(), p.name))
        .collect();
    Ok(SettingsValues {
        paused: settings::is_paused(state.pool()).await,
        auth_enabled: state.auth().is_enabled(),
        bypass_auth_from: get(settings::KEY_BYPASS_AUTH_FROM),
        trusted_proxies: get(settings::KEY_TRUSTED_PROXIES),
        public_url: get(settings::KEY_PUBLIC_URL),
        poll_interval_secs: if map.contains_key(settings::KEY_POLL_INTERVAL_SECS) {
            get(settings::KEY_POLL_INTERVAL_SECS)
        } else {
            // Show the live (env- or default-derived) value so the
            // box doesn't pretend the orchestrator is using whatever
            // the form happens to be pre-filled with.
            state.poll_interval().as_secs().to_string()
        },
        arr_sync_interval_secs: {
            let stored = get(settings::KEY_ARR_SYNC_INTERVAL_SECS);
            if stored.is_empty() {
                // Same rationale as the poller: show what the task is
                // actually using rather than an empty box.
                crate::arr_import::DEFAULT_SYNC_INTERVAL
                    .as_secs()
                    .to_string()
            } else {
                stored
            }
        },
        scan_searches_per_cycle: {
            let stored = get(settings::KEY_SCAN_SEARCHES_PER_CYCLE);
            if stored.is_empty() {
                // Same rationale as the poller: show what the sweep is
                // actually using rather than an empty box.
                crate::scan::DEFAULT_SEARCHES_PER_CYCLE.to_string()
            } else {
                stored
            }
        },
        decisions_retention_days: if map.contains_key(settings::KEY_DECISIONS_RETENTION_DAYS) {
            get(settings::KEY_DECISIONS_RETENTION_DAYS)
        } else {
            // Same rationale as poll_interval: reflect the live window.
            state.retention_days().to_string()
        },
        // Reflect the live runtime value so the checkbox always matches
        // what the pipeline is actually doing.
        persist_rejected: state.persist_rejected(),
        import_mode: crate::import::ImportMode::from_label(&get(crate::import::KEY_IMPORT_MODE))
            .label()
            .to_string(),
        log_level: get(settings::KEY_LOG_LEVEL),
        backtrace: {
            let v = get(settings::KEY_BACKTRACE);
            if v.is_empty() { "0".to_string() } else { v }
        },
        torznab_base,
        newznab_base,
        indexer_apikey,
        profiles,
        // Reflects the effective state, so an operator who set
        // BRARR_TMDB_TOKEN in the environment sees "configurado" rather
        // than an empty box implying nothing is set.
        tmdb_configured: crate::tmdb_sync::load_config(state.pool())
            .await?
            .is_configured(),
        tmdb_language: get(settings::KEY_TMDB_LANGUAGE),
        tmdb_country: get(settings::KEY_TMDB_COUNTRY),
        tmdb_ttl_days: get(settings::KEY_TMDB_TTL_DAYS),
        // Asked of the registry, which is what actually decides whether
        // a provider can be called — the screen and the dispatch now read
        // the same fact rather than two functions that agreed by habit.
        tvdb_configured: Registry::build(state.pool())
            .await?
            .get(brarr_core::MetadataSource::Tvdb)
            .is_some(),
        tvdb_pin: get(settings::KEY_TVDB_PIN),
    })
}

fn settings_flash_render(
    state: &AppState,
    flash: SettingsFlash,
) -> futures::future::BoxFuture<'_, Result<Response, AppError>> {
    Box::pin(async move {
        let mut values = load_settings_values(state).await?;
        // Echo back the just-submitted (possibly invalid) values
        // doesn't happen here — load_settings_values already reflects
        // whatever the save actually persisted.
        values.auth_enabled = state.auth().is_enabled();
        let tmpl = SettingsTemplate {
            // A flash always comes from a save, and every save posts
            // from a section — but the redirect target is not known
            // here, so land on the first group with the banner visible.
            section: settings_section(None),
            values,
            flash: Some(flash),
        };
        html(&tmpl)
    })
}

#[derive(Debug, Deserialize)]
struct SettingsGeneralForm {
    #[serde(default)]
    bypass_auth_from: String,
    #[serde(default)]
    trusted_proxies: String,
    #[serde(default)]
    public_url: String,
    #[serde(default)]
    poll_interval_secs: String,
    #[serde(default)]
    arr_sync_interval_secs: String,
    #[serde(default)]
    scan_searches_per_cycle: String,
    #[serde(default)]
    decisions_retention_days: String,
    // Checkbox: present (`"1"`) when ticked, absent (`""`) when not.
    #[serde(default)]
    persist_rejected: String,
    #[serde(default)]
    import_mode: String,
    #[serde(default)]
    log_level: String,
    #[serde(default)]
    backtrace: String,
    // Blank means "leave the stored credential alone" — the form never
    // echoes it back, so an empty box must not wipe it.
    #[serde(default)]
    tmdb_token: String,
    #[serde(default)]
    tmdb_language: String,
    #[serde(default)]
    tmdb_country: String,
    #[serde(default)]
    tmdb_ttl_days: String,
    /// Checkbox: present when ticked, absent when not — so it is written
    /// unconditionally from its presence rather than from a value.
    #[serde(default)]
    paused: String,
    // Same contract as `tmdb_token`: blank keeps the stored key.
    #[serde(default)]
    tvdb_api_key: String,
    #[serde(default)]
    tvdb_pin: String,
}

#[allow(
    clippy::too_many_lines,
    reason = "settings save is one linear validate→persist→swap story; splitting hides the per-field cancel paths"
)]
async fn settings_general(
    State(state): State<AppState>,
    Form(form): Form<SettingsGeneralForm>,
) -> Result<Response, AppError> {
    // Parse + validate every field BEFORE persisting anything so a
    // typo in one box doesn't half-apply the form.
    let bypass_spec = form.bypass_auth_from.trim();
    let proxies_spec = form.trusted_proxies.trim();
    let peers = if bypass_spec.is_empty() {
        TrustedPeers::default()
    } else {
        match TrustedPeers::parse(bypass_spec) {
            Ok(p) => p,
            Err(e) => {
                return settings_flash_render(
                    &state,
                    SettingsFlash {
                        kind: "err".to_string(),
                        message: format!("Whitelist inválida: {e}"),
                    },
                )
                .await;
            }
        }
    };
    let proxies = if proxies_spec.is_empty() {
        TrustedPeers::default()
    } else {
        match TrustedPeers::parse(proxies_spec) {
            Ok(p) => p,
            Err(e) => {
                return settings_flash_render(
                    &state,
                    SettingsFlash {
                        kind: "err".to_string(),
                        message: format!("Proxies confiáveis inválidos: {e}"),
                    },
                )
                .await;
            }
        }
    };

    let public_url = form.public_url.trim();
    let public_url_opt = if public_url.is_empty() {
        None
    } else {
        Some(public_url.trim_end_matches('/').to_string())
    };

    let interval_secs = if form.poll_interval_secs.trim().is_empty() {
        None
    } else {
        match form.poll_interval_secs.trim().parse::<u64>() {
            Ok(secs) if secs >= MIN_POLL_INTERVAL_SECS => Some(secs),
            Ok(secs) => {
                return settings_flash_render(
                    &state,
                    SettingsFlash {
                        kind: "err".to_string(),
                        message: format!(
                            "Intervalo do poller {secs}s abaixo do mínimo de {MIN_POLL_INTERVAL_SECS}s."
                        ),
                    },
                )
                .await;
            }
            Err(e) => {
                return settings_flash_render(
                    &state,
                    SettingsFlash {
                        kind: "err".to_string(),
                        message: format!("Intervalo inválido: {e}"),
                    },
                )
                .await;
            }
        }
    };

    // Blank is "use the default", so it is stored blank rather than
    // rejected. A value below the floor is a typo worth naming: the task
    // would clamp it silently and the box would keep showing a number it
    // is not using.
    let arr_sync_secs = if form.arr_sync_interval_secs.trim().is_empty() {
        None
    } else {
        match form.arr_sync_interval_secs.trim().parse::<u64>() {
            Ok(secs) if secs >= MIN_POLL_INTERVAL_SECS => Some(secs),
            Ok(secs) => {
                return settings_flash_render(
                    &state,
                    SettingsFlash {
                        kind: "err".to_string(),
                        message: format!(
                            "Intervalo da sincronização *arr muito curto: {secs}s (mínimo {MIN_POLL_INTERVAL_SECS}s)"
                        ),
                    },
                )
                .await;
            }
            Err(e) => {
                return settings_flash_render(
                    &state,
                    SettingsFlash {
                        kind: "err".to_string(),
                        message: format!("Intervalo da sincronização *arr inválido: {e}"),
                    },
                )
                .await;
            }
        }
    };

    // Zero is refused rather than clamped, because it is the one value
    // an operator could type meaning "stop searching" — and the pause
    // switch is what says that. Clamping silently would leave the box
    // showing a number the sweep is not using.
    let scan_budget = if form.scan_searches_per_cycle.trim().is_empty() {
        None
    } else {
        match form.scan_searches_per_cycle.trim().parse::<usize>() {
            Ok(n) if n >= crate::scan::MIN_SEARCHES_PER_CYCLE => Some(n),
            Ok(n) => {
                return settings_flash_render(
                    &state,
                    SettingsFlash {
                        kind: "err".to_string(),
                        message: format!(
                            "Buscas por ciclo muito baixo: {n} (mínimo {}). Para parar a varredura use a pausa.",
                            crate::scan::MIN_SEARCHES_PER_CYCLE
                        ),
                    },
                )
                .await;
            }
            Err(e) => {
                return settings_flash_render(
                    &state,
                    SettingsFlash {
                        kind: "err".to_string(),
                        message: format!("Buscas por ciclo inválido: {e}"),
                    },
                )
                .await;
            }
        }
    };

    let retention_days = if form.decisions_retention_days.trim().is_empty() {
        None
    } else {
        match form.decisions_retention_days.trim().parse::<u32>() {
            Ok(days) => Some(days),
            Err(e) => {
                return settings_flash_render(
                    &state,
                    SettingsFlash {
                        kind: "err".to_string(),
                        message: format!("Retenção inválida (dias): {e}"),
                    },
                )
                .await;
            }
        }
    };

    let persist_rejected = settings::parse_flag(&form.persist_rejected);

    let log_spec = form.log_level.trim();
    if !log_spec.is_empty()
        && let Err(e) = state.runtime().log_reload.apply(log_spec)
    {
        return settings_flash_render(
            &state,
            SettingsFlash {
                kind: "err".to_string(),
                message: format!("Log level inválido: {e}"),
            },
        )
        .await;
    }

    let backtrace = match form.backtrace.trim() {
        "" | "0" => "0".to_string(),
        "1" => "1".to_string(),
        "full" => "full".to_string(),
        other => {
            return settings_flash_render(
                &state,
                SettingsFlash {
                    kind: "err".to_string(),
                    message: format!("Valor inválido pra backtrace: {other}"),
                },
            )
            .await;
        }
    };

    // Persist everything.
    let pool = state.pool();
    settings::set(pool, settings::KEY_BYPASS_AUTH_FROM, bypass_spec).await?;
    settings::set(pool, settings::KEY_TRUSTED_PROXIES, proxies_spec).await?;
    settings::set(pool, settings::KEY_PUBLIC_URL, public_url).await?;
    settings::set(
        pool,
        settings::KEY_POLL_INTERVAL_SECS,
        &interval_secs.map(|s| s.to_string()).unwrap_or_default(),
    )
    .await?;
    settings::set(
        pool,
        settings::KEY_ARR_SYNC_INTERVAL_SECS,
        &arr_sync_secs.map(|s| s.to_string()).unwrap_or_default(),
    )
    .await?;
    settings::set(
        pool,
        settings::KEY_SCAN_SEARCHES_PER_CYCLE,
        &scan_budget.map(|n| n.to_string()).unwrap_or_default(),
    )
    .await?;
    settings::set(
        pool,
        settings::KEY_DECISIONS_RETENTION_DAYS,
        &retention_days.map(|d| d.to_string()).unwrap_or_default(),
    )
    .await?;
    settings::set(
        pool,
        settings::KEY_PERSIST_REJECTED,
        if persist_rejected { "1" } else { "0" },
    )
    .await?;
    settings::set(
        pool,
        crate::import::KEY_IMPORT_MODE,
        crate::import::ImportMode::from_label(&form.import_mode).label(),
    )
    .await?;
    settings::set(pool, settings::KEY_LOG_LEVEL, log_spec).await?;
    settings::set(pool, settings::KEY_BACKTRACE, &backtrace).await?;

    // TMDB. The credential is write-only from the UI: the form never
    // echoes it back, so a blank box means "leave it alone" rather than
    // "clear it". The other three are plain overrides and blanking them
    // legitimately falls back to the defaults.
    // A checkbox is absent when unticked, so this writes from presence.
    // Unconditional on purpose: it is the one setting whose *off* state
    // has to be storable, or brarr could be paused and never resumed.
    settings::set(
        pool,
        settings::KEY_PAUSED,
        if form.paused.trim().is_empty() {
            "0"
        } else {
            "1"
        },
    )
    .await?;

    // The PIN is not a secret the way the key is — it is per-subscriber
    // and only meaningful for a user-supported key — so it is written
    // unconditionally, blank included, and clearing it is possible.
    settings::set(pool, settings::KEY_TVDB_PIN, form.tvdb_pin.trim()).await?;
    let tvdb_api_key = form.tvdb_api_key.trim();
    if !tvdb_api_key.is_empty() {
        settings::set(pool, settings::KEY_TVDB_API_KEY, tvdb_api_key).await?;
    }

    let tmdb_token = form.tmdb_token.trim();
    if !tmdb_token.is_empty() {
        settings::set(pool, settings::KEY_TMDB_TOKEN, tmdb_token).await?;
    }
    settings::set(pool, settings::KEY_TMDB_LANGUAGE, form.tmdb_language.trim()).await?;
    settings::set(pool, settings::KEY_TMDB_COUNTRY, form.tmdb_country.trim()).await?;
    settings::set(pool, settings::KEY_TMDB_TTL_DAYS, form.tmdb_ttl_days.trim()).await?;

    // Swap runtime config — atomic per-field.
    state
        .runtime()
        .bypass
        .store(Arc::new(BypassConfig { peers, proxies }));
    state.runtime().public_url.store(Arc::new(public_url_opt));
    if let Some(secs) = interval_secs {
        state
            .runtime()
            .poll_interval
            .store(Arc::new(Duration::from_secs(secs)));
    }
    if let Some(days) = retention_days {
        state.runtime().retention_days.store(Arc::new(days));
    }
    state
        .runtime()
        .persist_rejected
        .store(Arc::new(persist_rejected));

    settings_flash_render(
        &state,
        SettingsFlash {
            kind: "ok".to_string(),
            message: "Configurações salvas e aplicadas. Backtrace exige restart.".to_string(),
        },
    )
    .await
}

/// `POST /settings/maintenance/prune` — run the retention prune now at
/// the live window, reclaim freed pages, and re-render with a flash.
async fn settings_maintenance_prune(State(state): State<AppState>) -> Result<Response, AppError> {
    let pool = state.pool();
    let days = state.retention_days();
    let outcome = crate::db::maintenance::run_prune(pool, days).await?;
    // Reclaim is best-effort: a failed checkpoint must not hide the
    // prune result the operator just triggered.
    if let Err(e) = crate::db::maintenance::checkpoint_wal(pool).await {
        info!(target: "brarr_orchestrator::web", error = %e, "manual prune: wal checkpoint failed");
    }
    if let Err(e) = crate::db::maintenance::incremental_vacuum(pool).await {
        info!(target: "brarr_orchestrator::web", error = %e, "manual prune: incremental vacuum failed");
    }
    let message = if days == 0 {
        "Retenção desativada (0 dias) — nada podado.".to_string()
    } else {
        format!(
            "Poda concluída: {} decisão(ões), {} busca(s) e {} métrica(s) removidas (janela de {days} dia(s)).",
            outcome.decisions_deleted, outcome.searches_deleted, outcome.metrics_deleted
        )
    };
    settings_flash_render(
        &state,
        SettingsFlash {
            kind: "ok".to_string(),
            message,
        },
    )
    .await
}

/// `POST /settings/maintenance/vacuum` — full `VACUUM` to physically
/// shrink the database file. Expensive (exclusive lock); surfaced as an
/// explicit button rather than run on a schedule.
async fn settings_maintenance_vacuum(State(state): State<AppState>) -> Result<Response, AppError> {
    crate::db::maintenance::full_vacuum(state.pool()).await?;
    settings_flash_render(
        &state,
        SettingsFlash {
            kind: "ok".to_string(),
            message: "VACUUM concluído — espaço livre devolvido ao disco.".to_string(),
        },
    )
    .await
}

#[derive(Debug, Deserialize)]
#[allow(
    clippy::struct_field_names,
    reason = "all three values ARE tokens; renaming muddies the form mapping"
)]
struct SettingsTokenForm {
    #[serde(default)]
    current_token: String,
    new_token: String,
    confirm_token: String,
}

async fn settings_token(
    State(state): State<AppState>,
    Form(form): Form<SettingsTokenForm>,
) -> Result<Response, AppError> {
    let new = form.new_token.trim();
    let confirm = form.confirm_token.trim();

    if new.is_empty() {
        return settings_flash_render(
            &state,
            SettingsFlash {
                kind: "err".to_string(),
                message: "Novo token não pode ser vazio.".to_string(),
            },
        )
        .await;
    }
    if new != confirm {
        return settings_flash_render(
            &state,
            SettingsFlash {
                kind: "err".to_string(),
                message: "Confirmação não bate com o novo token.".to_string(),
            },
        )
        .await;
    }

    // When auth is already enabled, require the current token as
    // confirmation — prevents an accidental rotation lockout when a
    // session is still valid but the operator was just experimenting.
    if state.auth().is_enabled() && !state.auth().token_matches(form.current_token.trim()) {
        return settings_flash_render(
            &state,
            SettingsFlash {
                kind: "err".to_string(),
                message: "Token atual incorreto.".to_string(),
            },
        )
        .await;
    }

    settings::set(state.pool(), settings::KEY_AUTH_TOKEN, new).await?;
    let new_cfg = AuthConfig::from_optional(Some(new));
    state.runtime().auth.store(Arc::new(new_cfg));

    info!(
        target: "brarr_orchestrator",
        "admin token rotated via /settings — existing sessions invalidated"
    );

    settings_flash_render(
        &state,
        SettingsFlash {
            kind: "ok".to_string(),
            message:
                "Token trocado. Sua sessão atual fica inválida no próximo request — entre de novo."
                    .to_string(),
        },
    )
    .await
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

/// `GET /providers/{id}/edit` — return the edit modal pre-filled
/// with the row's current values. HTMX swaps it into `#modal-target`.
async fn providers_edit(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Response, AppError> {
    let uuid = Uuid::parse_str(&id)
        .map_err(|e| AppError::InvalidInput(format!("invalid provider id: {e}")))?;
    let row = providers::get_by_id(state.pool(), uuid).await?;
    html(&EditProviderModalPartial {
        id: row.id.to_string(),
        name: row.name,
        base_url: row.base_url.to_string(),
        api_token: row.api_token,
        kind: row.kind,
        plugin_path: row
            .plugin_path
            .as_ref()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default(),
    })
}

#[derive(Debug, Deserialize)]
struct UpdateProviderForm {
    name: String,
    base_url: String,
    api_token: String,
    #[serde(default)]
    kind: Option<String>,
    #[serde(default)]
    plugin_path: Option<String>,
}

/// `PUT /providers/{id}` — apply edits. Returns the refreshed list
/// partial so HTMX can swap the table in place.
async fn providers_update(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Form(form): Form<UpdateProviderForm>,
) -> Result<Response, AppError> {
    let uuid = Uuid::parse_str(&id)
        .map_err(|e| AppError::InvalidInput(format!("invalid provider id: {e}")))?;
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
    providers::update(
        state.pool(),
        uuid,
        providers::ProviderUpdate {
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

/// `POST /arr-instances/{id}/toggle` — mirror of providers_toggle for
/// the *arr instances list.
async fn arr_instances_toggle(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Response, AppError> {
    let uuid = Uuid::parse_str(&id)
        .map_err(|e| AppError::InvalidInput(format!("invalid arr_instance id: {e}")))?;
    let current = arr_instances::get_by_id(state.pool(), uuid).await?;
    arr_instances::set_enabled(state.pool(), uuid, !current.enabled).await?;
    render_arr_instances_partial(&state).await
}

/// `POST /arr-instances/{id}/webhook-driven` — flip the webhook-driven
/// flag. When on, the scheduled poller skips this instance (the manual
/// "rodar agora" button still works).
async fn arr_instances_webhook_driven_toggle(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Response, AppError> {
    let uuid = Uuid::parse_str(&id)
        .map_err(|e| AppError::InvalidInput(format!("invalid arr_instance id: {e}")))?;
    let current = arr_instances::get_by_id(state.pool(), uuid).await?;
    arr_instances::set_webhook_driven(state.pool(), uuid, !current.webhook_driven).await?;
    render_arr_instances_partial(&state).await
}

/// `POST /arr-instances/{id}/sync-source` — flip whether brarr reads
/// this catalogue into its own library.
///
/// Deliberately separate from the enabled toggle: they are different
/// questions, and the operator's three instances answer them differently
/// — disabled for the deprecated push path, on as a source.
async fn arr_instances_sync_source_toggle(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Response, AppError> {
    let uuid = Uuid::parse_str(&id)
        .map_err(|e| AppError::InvalidInput(format!("invalid arr_instance id: {e}")))?;
    let current = arr_instances::get_by_id(state.pool(), uuid).await?;
    arr_instances::set_sync_source(state.pool(), uuid, !current.sync_source).await?;
    render_arr_instances_partial(&state).await
}

/// How long a manual import is waited on before the answer becomes "it
/// is still running". A 468-title migration costs one TMDB call per
/// title plus one per season, so it will not fit — the wait exists for
/// the small instance that does.
const MANUAL_IMPORT_WAIT: Duration = Duration::from_secs(20);

/// Build the preview once. Every write on the import screen re-runs it,
/// which is the point: adding the mapping that makes 292 folders visible
/// has to change the number on screen.
async fn arr_import_body(
    state: &AppState,
    instance_id: Uuid,
) -> Result<ArrImportBodyPartial, AppError> {
    let plan = crate::arr_import::plan(state, instance_id).await?;
    let folders = root_folders::list_all(state.pool()).await?;
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

    Ok(ArrImportBodyPartial {
        instance_id: plan.instance_id.to_string(),
        kind: match plan.kind {
            brarr_arr::ArrKind::Sonarr => "Sonarr".to_owned(),
            brarr_arr::ArrKind::Radarr => "Radarr".to_owned(),
        },
        unmapped_roots: plan.roots.iter().filter(|r| r.mapped_to.is_none()).count(),
        new_titles: plan.new_titles(),
        known_titles: plan.known_titles(),
        blocked_titles: plan.blocked_titles(),
        seen_folders: plan.seen_folders(),
        roots: plan
            .roots
            .iter()
            .map(|r| ArrImportRootView {
                arr_path: r.arr_path.clone(),
                mapped_to: r
                    .mapped_to
                    .as_ref()
                    .map(|p| p.to_string_lossy().into_owned()),
                mapping_id: r.mapping_id.map(|id| id.to_string()),
                reachable: r.reachable,
                titles: r.titles,
            })
            .collect(),
        titles: plan
            .titles
            .iter()
            .map(|t| ArrImportTitleView {
                title: t.title.clone(),
                year: t.year,
                tmdb_id: t.tmdb_id,
                monitored: t.monitored,
                folder_seen: t.folder_seen,
                status: t.status.label().to_owned(),
                blocked: !t.status.actionable(),
            })
            .collect(),
        root_folders: folders
            .into_iter()
            .map(|f| ArrRootOption {
                id: f.id.to_string(),
                path: f.path.to_string_lossy().into_owned(),
            })
            .collect(),
        profiles,
    })
}

/// `GET /arr-instances/{id}/import` — the preview screen.
async fn arr_import_index(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Response, AppError> {
    let uuid = Uuid::parse_str(&id)
        .map_err(|e| AppError::InvalidInput(format!("invalid arr_instance id: {e}")))?;
    let row = arr_instances::get_by_id(state.pool(), uuid).await?;
    let body = arr_import_body(&state, uuid).await?;
    html(&ArrImportTemplate::from_body(body, row.name))
}

#[derive(Debug, Deserialize)]
struct ArrRootMappingForm {
    arr_path: String,
    root_folder_id: String,
}

/// `POST /arr-instances/{id}/import/mappings` — map one \*arr root onto
/// one of brarr's.
async fn arr_import_add_mapping(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Form(form): Form<ArrRootMappingForm>,
) -> Result<Response, AppError> {
    let uuid = Uuid::parse_str(&id)
        .map_err(|e| AppError::InvalidInput(format!("invalid arr_instance id: {e}")))?;
    let root_folder_id = Uuid::parse_str(form.root_folder_id.trim())
        .map_err(|e| AppError::InvalidInput(format!("pasta raiz inválida: {e}")))?;
    crate::db::arr_root_mappings::insert(state.pool(), uuid, form.arr_path.trim(), root_folder_id)
        .await?;
    html(&arr_import_body(&state, uuid).await?)
}

/// `DELETE /arr-root-mappings/{id}` — forget one rule.
///
/// The mapping carries its instance, so the screen it re-renders is the
/// one the rule belongs to rather than whichever page issued the delete.
async fn arr_root_mapping_delete(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Response, AppError> {
    let uuid = Uuid::parse_str(&id)
        .map_err(|e| AppError::InvalidInput(format!("invalid mapping id: {e}")))?;
    let all = crate::db::arr_root_mappings::list_all(state.pool()).await?;
    let Some(row) = all.into_iter().find(|m| m.id == uuid) else {
        return Err(AppError::NotFound(format!("arr_root_mapping {uuid}")));
    };
    let instance_id = row.arr_instance_id;
    crate::db::arr_root_mappings::delete_by_id(state.pool(), uuid).await?;
    html(&arr_import_body(&state, instance_id).await?)
}

#[derive(Debug, Deserialize)]
struct ArrImportRunForm {
    #[serde(default)]
    monitoring: String,
    #[serde(default)]
    profile_id: Option<String>,
}

/// `POST /arr-instances/{id}/import/run` — read the catalogue in.
///
/// Spawned and waited on briefly, like `/library/{id}/scan`: a real
/// migration outlives any request, and the honest answer is "still
/// running, the library fills as it goes". Re-running is safe, so the
/// operator never has to know which of the two happened.
async fn arr_import_run(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Form(form): Form<ArrImportRunForm>,
) -> Result<Response, AppError> {
    let uuid = Uuid::parse_str(&id)
        .map_err(|e| AppError::InvalidInput(format!("invalid arr_instance id: {e}")))?;
    let profile_id = form
        .profile_id
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(Uuid::parse_str)
        .transpose()
        .map_err(|e| AppError::InvalidInput(format!("profile_id deve ser uuid: {e}")))?;
    let options = crate::arr_import::ImportOptions {
        monitoring: crate::arr_import::MonitorChoice::from_label(&form.monitoring),
        profile_id,
    };

    let task_state = state.clone();
    let handle =
        tokio::spawn(async move { crate::arr_import::run(&task_state, uuid, options).await });
    let report = match tokio::time::timeout(MANUAL_IMPORT_WAIT, handle).await {
        Ok(Ok(Ok(report))) => report,
        Ok(Ok(Err(e))) => return Err(e),
        Ok(Err(join)) => return Err(AppError::InvalidInput(format!("importação falhou: {join}"))),
        Err(_elapsed) => {
            // The task owns itself now; its results land in the library.
            return html(&ArrImportReportPartial {
                running: true,
                ..ArrImportReportPartial::default()
            });
        }
    };
    html(&ArrImportReportPartial {
        running: false,
        added: report.added,
        refreshed: report.refreshed,
        blocked: report.blocked,
        adopted: report.files.adopted,
        already: report.files.already,
        relinked: report.files.relinked,
        missing: report.files.missing,
        unmapped: report.files.unmapped,
        failures: report.failures,
        failed: report.failed,
    })
}

/// `GET /arr-instances/{id}/edit` — return the edit modal partial.
async fn arr_instances_edit(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Response, AppError> {
    let uuid = Uuid::parse_str(&id)
        .map_err(|e| AppError::InvalidInput(format!("invalid arr_instance id: {e}")))?;
    let row = arr_instances::get_by_id(state.pool(), uuid).await?;
    let profile_rows = quality_profiles::list_all(state.pool()).await?;
    let profiles = profile_rows
        .into_iter()
        .map(|p| ProfileView {
            id: p.id.to_string(),
            name: p.name,
            description: p.description,
            push_threshold: p.push_threshold,
            is_preset: p.is_preset,
        })
        .collect();
    html(&EditArrInstanceModalPartial {
        id: row.id.to_string(),
        name: row.name,
        kind: row.kind.label().to_string(),
        base_url: row.base_url.to_string(),
        api_key: row.api_key,
        push_threshold: row.push_threshold.to_string(),
        profiles,
        profile_id: row.profile_id.map(|u| u.to_string()).unwrap_or_default(),
    })
}

#[derive(Debug, Deserialize)]
struct UpdateArrInstanceForm {
    name: String,
    base_url: String,
    api_key: String,
    #[serde(default)]
    push_threshold: Option<String>,
    #[serde(default)]
    profile_id: Option<String>,
}

/// `PUT /arr-instances/{id}` — apply edits + return refreshed list.
async fn arr_instances_update(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Form(form): Form<UpdateArrInstanceForm>,
) -> Result<Response, AppError> {
    let uuid = Uuid::parse_str(&id)
        .map_err(|e| AppError::InvalidInput(format!("invalid arr_instance id: {e}")))?;
    let url = url::Url::parse(form.base_url.trim())
        .map_err(|e| AppError::InvalidInput(format!("invalid base_url: {e}")))?;
    let push_threshold = form
        .push_threshold
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::parse::<u32>)
        .transpose()
        .map_err(|e| AppError::InvalidInput(format!("push_threshold must be 0..=1000: {e}")))?
        .unwrap_or(150);
    let profile_id = form
        .profile_id
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(Uuid::parse_str)
        .transpose()
        .map_err(|e| AppError::InvalidInput(format!("profile_id deve ser uuid: {e}")))?;
    arr_instances::update(
        state.pool(),
        uuid,
        arr_instances::ArrInstanceUpdate {
            name: form.name.trim(),
            base_url: &url,
            api_key: form.api_key.trim(),
            push_threshold,
            profile_id,
        },
    )
    .await?;
    render_arr_instances_partial(&state).await
}

// ---------------------------------------------------------------------
// Download clients — qBittorrent / SABnzbd CRUD.
//
// Every mutation answers with the whole list partial rather than a
// single row: the table is ordered by `enabled DESC, priority, name`, so
// one edit can move a *different* row.
// ---------------------------------------------------------------------

async fn download_clients_index(State(state): State<AppState>) -> Result<Response, AppError> {
    let clients = download_client_views(&state).await?;
    let (has_torrent, has_usenet) = protocol_coverage(&clients);
    let block = path_mapping_block(&state).await?;
    html(&DownloadClientsTemplate {
        clients,
        root_folders: root_folder_views(&state).await?,
        mappings: block.mappings,
        mapping_clients: block.mapping_clients,
        stuck: block.stuck,
        has_torrent,
        has_usenet,
    })
}

/// Which transports currently have somewhere to deliver to. Only
/// enabled rows count — a drained client is exactly as useful to a grab
/// as no client at all.
fn protocol_coverage(clients: &[DownloadClientView]) -> (bool, bool) {
    let serves = |protocol: &str| clients.iter().any(|c| c.enabled && c.protocol == protocol);
    (serves("torrent"), serves("usenet"))
}

#[derive(Debug, Deserialize)]
struct DownloadClientForm {
    name: String,
    /// Only read on create — the edit modal has no kind field, because
    /// switching kind would change the protocol every linked grab was
    /// routed under.
    #[serde(default)]
    kind: Option<String>,
    base_url: String,
    #[serde(default)]
    username: Option<String>,
    #[serde(default)]
    password: Option<String>,
    #[serde(default)]
    api_key: Option<String>,
    #[serde(default)]
    category: Option<String>,
    #[serde(default)]
    priority: Option<String>,
}

impl DownloadClientForm {
    /// Trimmed field, with empty read as absent — the same convention
    /// [`crate::db::download_clients`] applies before it writes.
    fn optional(field: Option<&str>) -> Option<&str> {
        field.map(str::trim).filter(|v| !v.is_empty())
    }

    fn parsed_url(&self) -> Result<url::Url, AppError> {
        url::Url::parse(self.base_url.trim())
            .map_err(|e| AppError::InvalidInput(format!("invalid base_url: {e}")))
    }

    fn parsed_priority(&self) -> Result<Option<u32>, AppError> {
        Self::optional(self.priority.as_deref())
            .map(str::parse::<u32>)
            .transpose()
            .map_err(|e| AppError::InvalidInput(format!("prioridade deve ser um inteiro: {e}")))
    }
}

async fn download_clients_create(
    State(state): State<AppState>,
    Form(form): Form<DownloadClientForm>,
) -> Result<Response, AppError> {
    let kind_raw = form.kind.as_deref().unwrap_or_default().trim().to_string();
    let kind =
        brarr_download_client::DownloadClientKind::from_label(&kind_raw).ok_or_else(|| {
            AppError::InvalidInput(format!(
                "kind deve ser qbittorrent ou sabnzbd, veio {kind_raw:?}"
            ))
        })?;
    let url = form.parsed_url()?;
    download_clients::insert(
        state.pool(),
        download_clients::NewDownloadClient {
            name: form.name.trim(),
            kind,
            base_url: &url,
            username: DownloadClientForm::optional(form.username.as_deref()),
            password: DownloadClientForm::optional(form.password.as_deref()),
            api_key: DownloadClientForm::optional(form.api_key.as_deref()),
            category: DownloadClientForm::optional(form.category.as_deref()),
            priority: form.parsed_priority()?,
            enabled: Some(true),
        },
    )
    .await?;
    render_download_clients_partial(&state).await
}

/// `PUT /download-clients/{id}` — apply edits.
///
/// A blank password / apikey is passed through as `None`, which the db
/// layer reads as "keep the stored one". The modal never echoes a
/// secret, so treating blank as "erase" would wipe the credential of
/// anyone who edited a name.
async fn download_clients_update(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Form(form): Form<DownloadClientForm>,
) -> Result<Response, AppError> {
    let uuid = parse_download_client_id(&id)?;
    let url = form.parsed_url()?;
    download_clients::update(
        state.pool(),
        uuid,
        download_clients::DownloadClientUpdate {
            name: form.name.trim(),
            base_url: &url,
            username: DownloadClientForm::optional(form.username.as_deref()),
            password: DownloadClientForm::optional(form.password.as_deref()),
            api_key: DownloadClientForm::optional(form.api_key.as_deref()),
            category: DownloadClientForm::optional(form.category.as_deref()),
            priority: form.parsed_priority()?.unwrap_or(1),
        },
    )
    .await?;
    render_download_clients_partial(&state).await
}

async fn download_clients_delete(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Response, AppError> {
    let uuid = parse_download_client_id(&id)?;
    if !download_clients::delete_by_id(state.pool(), uuid).await? {
        return Err(AppError::NotFound(format!("download_client {uuid}")));
    }
    render_download_clients_partial(&state).await
}

/// `POST /download-clients/{id}/toggle` — drain mode on/off.
async fn download_clients_toggle(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Response, AppError> {
    let uuid = parse_download_client_id(&id)?;
    let current = download_clients::get_by_id(state.pool(), uuid).await?;
    download_clients::set_enabled(state.pool(), uuid, !current.enabled).await?;
    render_download_clients_partial(&state).await
}

/// `GET /download-clients/{id}/edit` — pre-filled modal, minus secrets.
async fn download_clients_edit(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Response, AppError> {
    let uuid = parse_download_client_id(&id)?;
    let row = download_clients::get_by_id(state.pool(), uuid).await?;
    html(&EditDownloadClientModalPartial {
        id: row.id.to_string(),
        name: row.name,
        kind: row.kind.label().to_string(),
        kind_label: row.kind.display_name().to_string(),
        base_url: row.base_url.to_string(),
        username: row.username.unwrap_or_default(),
        category: row.category.unwrap_or_default(),
        priority: row.priority,
        has_password: row.password.is_some(),
        has_api_key: row.api_key.is_some(),
    })
}

/// `POST /download-clients/{id}/test` — authenticate against the client
/// and report its version.
///
/// Both clients can refuse credentials inside a `200 OK`, which is why
/// this goes through the real client rather than a bare HTTP probe.
async fn download_clients_test(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Response, AppError> {
    let uuid = parse_download_client_id(&id)?;
    let row = download_clients::get_by_id(state.pool(), uuid).await?;
    let badge = match brarr_download_client::build(row.to_config()) {
        Ok(client) => match client.test_connection().await {
            Ok(status) => {
                let version = if status.version.is_empty() {
                    "versão não informada".to_string()
                } else {
                    status.version.clone()
                };
                PingBadge {
                    ok: true,
                    label: format!("conectado · {version}"),
                    detail: format!("{} respondeu", row.kind.display_name()),
                }
            }
            Err(e) => PingBadge {
                ok: false,
                label: "erro".to_string(),
                detail: format!("{e}"),
            },
        },
        Err(e) => PingBadge {
            ok: false,
            label: "config".to_string(),
            detail: format!("{e}"),
        },
    };
    Ok(html_string(render_status_badge(
        &format!("dl-ping-{}", row.id),
        &badge,
    )))
}

fn parse_download_client_id(id: &str) -> Result<Uuid, AppError> {
    Uuid::parse_str(id)
        .map_err(|e| AppError::InvalidInput(format!("invalid download client id: {e}")))
}

async fn download_client_views(state: &AppState) -> Result<Vec<DownloadClientView>, AppError> {
    let rows = download_clients::list_all(state.pool()).await?;
    Ok(rows.into_iter().map(download_client_view).collect())
}

async fn render_download_clients_partial(state: &AppState) -> Result<Response, AppError> {
    let clients = download_client_views(state).await?;
    let (has_torrent, has_usenet) = protocol_coverage(&clients);
    html(&DownloadClientsListPartial {
        clients,
        has_torrent,
        has_usenet,
    })
}

fn download_client_view(row: crate::db::download_clients::DownloadClientRow) -> DownloadClientView {
    DownloadClientView {
        id: row.id.to_string(),
        name: row.name,
        kind: row.kind.label().to_string(),
        kind_label: row.kind.display_name().to_string(),
        protocol: row.kind.protocol().label().to_string(),
        base_url: row.base_url.to_string(),
        category: row.category,
        priority: row.priority,
        enabled: row.enabled,
        created_at: format_ts(row.created_at),
    }
}

// ---------------------------------------------------------------------
// Media servers — Plex / Jellyfin / Emby CRUD, plus the Plex sign-in.
//
// Same shape as the download clients: every mutation answers with the
// whole list partial, because the ordering is enabled-first and one edit
// can move a different row.
// ---------------------------------------------------------------------

fn media_server_routes() -> Router<AppState> {
    Router::new()
        .route(
            "/media-servers",
            get(media_servers_index).post(media_servers_create),
        )
        .route(
            "/media-servers/{id}",
            delete(media_servers_delete).put(media_servers_update),
        )
        .route("/media-servers/{id}/edit", get(media_servers_edit))
        .route("/media-servers/{id}/test", post(media_servers_test))
        .route("/media-servers/{id}/toggle", post(media_servers_toggle))
        .route("/media-servers/{id}/rescan", post(media_servers_rescan))
        .route(
            "/media-servers/{id}/plex/login",
            post(media_servers_plex_login),
        )
        .route(
            "/media-servers/{id}/plex/login/status",
            get(media_servers_plex_login_status),
        )
        .route("/media-server-mappings", post(media_server_mappings_create))
        .route(
            "/media-server-mappings/{id}",
            delete(media_server_mappings_delete),
        )
}

async fn media_servers_index(State(state): State<AppState>) -> Result<Response, AppError> {
    let servers = media_server_views(&state).await?;
    let needs_credential = credential_gap(&servers);
    let (mappings, mapping_servers) = media_server_mapping_block(&state).await?;
    html(&MediaServersTemplate {
        servers,
        needs_credential,
        mappings,
        mapping_servers,
    })
}

/// How many enabled servers have no credential to notify with.
///
/// Only enabled rows count, for the same reason `protocol_coverage`
/// ignores drained clients: a server that hears about nothing cannot be
/// missing anything.
fn credential_gap(servers: &[MediaServerView]) -> usize {
    servers.iter().filter(|s| s.enabled && !s.has_token).count()
}

#[derive(Debug, Deserialize)]
struct MediaServerForm {
    name: String,
    /// Only read on create — the edit modal has no kind field, because
    /// switching kind would change both the dialect and the
    /// authentication scheme under a credential obtained for the old one.
    #[serde(default)]
    kind: Option<String>,
    base_url: String,
    #[serde(default)]
    token: Option<String>,
}

impl MediaServerForm {
    /// Trimmed field, with empty read as absent — the convention
    /// [`crate::db::media_servers`] applies before it writes, and what
    /// makes a blank credential mean "keep the stored one".
    fn optional(field: Option<&str>) -> Option<&str> {
        field.map(str::trim).filter(|v| !v.is_empty())
    }
}

async fn media_servers_create(
    State(state): State<AppState>,
    Form(form): Form<MediaServerForm>,
) -> Result<Response, AppError> {
    let kind_raw = form.kind.as_deref().unwrap_or_default().trim().to_owned();
    let kind = brarr_media_server::MediaServerKind::from_label(&kind_raw).ok_or_else(|| {
        AppError::InvalidInput(format!(
            "tipo deve ser plex, jellyfin ou emby, veio {kind_raw:?}"
        ))
    })?;
    media_servers::insert(
        state.pool(),
        media_servers::NewMediaServer {
            name: form.name.trim(),
            kind,
            base_url: form.base_url.trim(),
            token: MediaServerForm::optional(form.token.as_deref()),
        },
    )
    .await?;
    render_media_servers_partial(&state).await
}

/// Edit a server. A blank credential means "keep the stored one" — the
/// modal never echoed it back, so blank cannot mean "erase".
async fn media_servers_update(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Form(form): Form<MediaServerForm>,
) -> Result<Response, AppError> {
    let uuid = parse_media_server_id(&id)?;
    media_servers::update(
        state.pool(),
        uuid,
        media_servers::MediaServerUpdate {
            name: form.name.trim(),
            base_url: form.base_url.trim(),
            token: MediaServerForm::optional(form.token.as_deref()),
        },
    )
    .await?;
    render_media_servers_partial(&state).await
}

async fn media_servers_delete(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Response, AppError> {
    let uuid = parse_media_server_id(&id)?;
    if !media_servers::delete_by_id(state.pool(), uuid).await? {
        return Err(AppError::NotFound(format!("media_server {id}")));
    }
    render_media_servers_partial(&state).await
}

async fn media_servers_toggle(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Response, AppError> {
    let uuid = parse_media_server_id(&id)?;
    let row = media_servers::get_by_id(state.pool(), uuid).await?;
    media_servers::set_enabled(state.pool(), uuid, !row.enabled).await?;
    render_media_servers_partial(&state).await
}

async fn media_servers_edit(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Response, AppError> {
    let uuid = parse_media_server_id(&id)?;
    let row = media_servers::get_by_id(state.pool(), uuid).await?;
    html(&EditMediaServerModalPartial {
        id: row.id.to_string(),
        name: row.name.clone(),
        kind: row.kind.label().to_owned(),
        kind_label: row.kind.display_name().to_owned(),
        base_url: row.base_url.clone(),
        // A boolean, never the value.
        has_token: row.has_token(),
        uses_plex_login: row.kind.uses_plex_login(),
    })
}

/// Prove the credential and report what the server serves.
///
/// Goes through the real client rather than a bare HTTP probe for the
/// same reason the download-client test does: Plex answers `200` on
/// `/identity` to anyone, so a reachability check would paint a wrong
/// token green.
async fn media_servers_test(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Response, AppError> {
    let uuid = parse_media_server_id(&id)?;
    let row = media_servers::get_by_id(state.pool(), uuid).await?;
    let dom_id = format!("ms-ping-{}", row.id);

    let badge = match row.to_config().map(brarr_media_server::build) {
        Ok(Ok(client)) => match client.test_connection().await {
            Ok(status) => {
                let version = if status.version.is_empty() {
                    "versão não informada".to_owned()
                } else {
                    status.version.clone()
                };
                let names: Vec<&str> = status.libraries.iter().map(|l| l.title.as_str()).collect();
                // The credential is only half of it. A server whose
                // libraries do not contain brarr's root folders refuses
                // every notification, and stopping at the token reports
                // that as healthy — which it did, until a real import
                // went unnoticed.
                let roots: Vec<std::path::PathBuf> = root_folders::list_all(state.pool())
                    .await?
                    .into_iter()
                    .map(|r| r.path)
                    .collect();
                let rules = media_server_mappings::rules_for_server(state.pool(), row.id).await?;
                let uncovered = crate::notify::uncovered_roots(&roots, &rules, &status.libraries);

                if names.is_empty() {
                    PingBadge {
                        ok: false,
                        label: "sem biblioteca".to_owned(),
                        detail: format!(
                            "o {} respondeu ({version}) mas não serve biblioteca nenhuma, \
                             então não há o que avisar",
                            row.kind
                        ),
                    }
                } else if uncovered.is_empty() {
                    PingBadge {
                        ok: true,
                        label: format!("conectado · {version}"),
                        detail: format!("bibliotecas: {}", names.join(", ")),
                    }
                } else {
                    // Not a failure: without a mapping brarr re-anchors
                    // the path onto the server's own folder, which is
                    // what both *arr do and what makes them work with no
                    // configuration. A mapping only buys precision —
                    // addressing one library instead of every candidate.
                    let known: Vec<&str> = status
                        .libraries
                        .iter()
                        .flat_map(|l| l.locations.iter().map(String::as_str))
                        .collect();
                    PingBadge {
                        ok: true,
                        label: format!("conectado · {version}"),
                        detail: format!(
                            "bibliotecas: {}. Nenhuma delas cobre {} — o brarr re-ancora o caminho em {} na hora de avisar, que é o que o *arr faz. Funciona assim; um mapeamento de caminho troca semelhança por exatidão.",
                            names.join(", "),
                            uncovered.join(", "),
                            known.join(", ")
                        ),
                    }
                }
            }
            Err(e) => PingBadge {
                ok: false,
                label: "falhou".to_owned(),
                detail: e.to_string(),
            },
        },
        Ok(Err(e)) => PingBadge {
            ok: false,
            label: "sem credencial".to_owned(),
            detail: e.to_string(),
        },
        Err(e) => PingBadge {
            ok: false,
            label: "config inválida".to_owned(),
            detail: e.to_string(),
        },
    };
    Ok(html_string(render_status_badge(&dom_id, &badge)))
}

/// Ask a server to rescan every root folder — the recovery button.
///
/// Exists because the automatic notification fires once, from the import
/// pass, and cannot be replayed. A notification refused for a reason the
/// operator then fixes had nothing to retry it: correcting the mapping
/// did nothing for the titles already on disk.
async fn media_servers_rescan(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Response, AppError> {
    let uuid = parse_media_server_id(&id)?;
    crate::notify::rescan_all(&state, uuid).await?;
    // The row carries the outcome, so re-read rather than guess: a
    // failure landed in `last_error` and the badge has to show it.
    let row = media_servers::get_by_id(state.pool(), uuid).await?;
    let badge = match row.last_error.as_deref() {
        None => PingBadge {
            ok: true,
            label: "avisado".to_owned(),
            detail: "o servidor foi mandado varrer as bibliotecas dele".to_owned(),
        },
        Some(error) => PingBadge {
            ok: false,
            label: "falhou".to_owned(),
            detail: error.to_owned(),
        },
    };
    Ok(html_string(render_status_badge(
        &format!("ms-ping-{uuid}"),
        &badge,
    )))
}

/// How often the sign-in fragment re-asks. See
/// [`crate::notify::PLEX_LOGIN_POLL`].
fn plex_login_poll_secs() -> u64 {
    crate::notify::PLEX_LOGIN_POLL.as_secs()
}

/// The fragment shown while a Plex sign-in is waiting on the operator.
///
/// It replaces itself every few seconds until the status route says the
/// token landed. Same mechanism as `render_scan_running_badge`, and the
/// same family: a login genuinely ends, so the terminal answer carries
/// [`HX_STOP_POLLING`] and the asking stops.
fn render_plex_login_pending(server: Uuid, sign_in_url: &str) -> String {
    let dom_id = crate::web::templates::escape(&format!("ms-ping-{server}"));
    let href = crate::web::templates::escape(sign_in_url);
    let poll = plex_login_poll_secs();
    format!(
        r#"<span id="{dom_id}" class="inline-flex items-center gap-2" hx-get="/media-servers/{server}/plex/login/status" hx-trigger="every {poll}s" hx-swap="outerHTML">"#
    ) + &format!(
        r#"<a href="{href}" target="_blank" rel="noopener" class="btn btn-sm btn-brand">autorizar no plex.tv</a>"#
    ) + r#"<span class="text-xs italic text-fg-muted">esperando você autorizar…</span></span>"#
}

/// Start a Plex sign-in: ask plex.tv for a PIN and hand back the link.
async fn media_servers_plex_login(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Response, AppError> {
    let uuid = parse_media_server_id(&id)?;
    let row = media_servers::get_by_id(state.pool(), uuid).await?;
    if !row.kind.uses_plex_login() {
        return Err(AppError::InvalidInput(format!(
            "{} não usa o login do plex.tv — a credencial dele é uma API key",
            row.kind
        )));
    }
    let dom_id = format!("ms-ping-{uuid}");

    // A second click must not burn a second PIN: the one in flight is
    // still valid, and plex.tv rate-limits its auth endpoints.
    if let Some(pending) = state.plex_logins().get(&uuid, std::time::Instant::now()) {
        if !pending.is_expired(std::time::Instant::now()) {
            let login = plex_login_client(&state).await?;
            return Ok(html_string(render_plex_login_pending(
                uuid,
                &login.sign_in_url(&pending.code),
            )));
        }
    }

    let login = plex_login_client(&state).await?;
    let pin = match login.create_pin().await {
        Ok(pin) => pin,
        Err(e) => {
            return Ok(html_string(render_status_badge(
                &dom_id,
                &PingBadge {
                    ok: false,
                    label: "plex.tv não respondeu".to_owned(),
                    detail: e.to_string(),
                },
            )));
        }
    };

    // plex.tv's own clock, not one chosen here: it reports `expiresIn`
    // and neither *arr reads it, which is why their polls never stop.
    let lifetime = pin
        .expires_in_seconds
        .and_then(|secs| u64::try_from(secs).ok())
        .map_or(
            crate::notify::PLEX_LOGIN_TTL,
            std::time::Duration::from_secs,
        );
    let sign_in_url = login.sign_in_url(&pin.code);
    state.plex_logins().insert(
        uuid,
        crate::notify::PendingPlexLogin {
            pin_id: pin.id,
            code: pin.code.clone(),
            deadline: std::time::Instant::now() + lifetime,
        },
        std::time::Instant::now(),
    );
    Ok(html_string(render_plex_login_pending(uuid, &sign_in_url)))
}

/// Has the operator authorised the PIN yet?
///
/// Answers [`HX_STOP_POLLING`] on anything terminal — token stored, PIN
/// expired, mailbox gone — so the fragment stops asking. Sonarr's own
/// poll has no stop condition at all and spins forever when the operator
/// closes the tab.
async fn media_servers_plex_login_status(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Response, AppError> {
    let uuid = parse_media_server_id(&id)?;
    let dom_id = format!("ms-ping-{uuid}");
    let now = std::time::Instant::now();

    let Some(pending) = state.plex_logins().get(&uuid, now) else {
        return Ok(stop_polling(
            &dom_id,
            &PingBadge {
                ok: false,
                label: "login expirou".to_owned(),
                detail: "o PIN do plex.tv vale meia hora; clique em entrar com o Plex de novo"
                    .to_owned(),
            },
        ));
    };
    if pending.is_expired(now) {
        return Ok(stop_polling(
            &dom_id,
            &PingBadge {
                ok: false,
                label: "login expirou".to_owned(),
                detail: "o PIN do plex.tv vale meia hora; clique em entrar com o Plex de novo"
                    .to_owned(),
            },
        ));
    }

    let login = plex_login_client(&state).await?;
    match login.poll_pin(pending.pin_id).await {
        Ok(brarr_media_server::plex::PinState::Authorized(token)) => {
            media_servers::set_token(state.pool(), uuid, &token).await?;
            Ok(stop_polling(
                &dom_id,
                &PingBadge {
                    ok: true,
                    label: "conectado".to_owned(),
                    detail: "token guardado; use \"testar\" para listar as bibliotecas".to_owned(),
                },
            ))
        }
        Ok(brarr_media_server::plex::PinState::Expired) => Ok(stop_polling(
            &dom_id,
            &PingBadge {
                ok: false,
                label: "login expirou".to_owned(),
                detail: "o plex.tv não conhece mais este PIN; clique em entrar com o Plex de novo"
                    .to_owned(),
            },
        )),
        Ok(brarr_media_server::plex::PinState::Pending) => {
            // Still waiting: re-render the same fragment, which keeps
            // its own trigger and so schedules the next ask.
            Ok(html_string(render_plex_login_pending(
                uuid,
                &login.sign_in_url(&pending.code),
            )))
        }
        // A transient plex.tv failure keeps the poll alive on purpose —
        // giving up on one bad response would strand a login the
        // operator is in the middle of.
        Err(e) => {
            tracing::warn!(
                target: "brarr_orchestrator::notify",
                error = %e,
                "plex.tv poll failed; still waiting"
            );
            Ok(html_string(render_plex_login_pending(
                uuid,
                &login.sign_in_url(&pending.code),
            )))
        }
    }
}

/// A badge that also cancels the trigger that asked for it.
fn stop_polling(dom_id: &str, badge: &PingBadge) -> Response {
    let body = render_status_badge(dom_id, badge);
    let mut res = (
        axum::http::StatusCode::from_u16(HX_STOP_POLLING).unwrap_or(axum::http::StatusCode::OK),
        body,
    )
        .into_response();
    res.headers_mut().insert(
        axum::http::header::CONTENT_TYPE,
        axum::http::HeaderValue::from_static("text/html; charset=utf-8"),
    );
    res
}

/// The plex.tv client, carrying this install's persisted identity.
async fn plex_login_client(
    state: &AppState,
) -> Result<brarr_media_server::plex::PlexLogin, AppError> {
    let identity = crate::notify::plex_identity(state.pool()).await?;
    brarr_media_server::plex::PlexLogin::new(identity)
        .map_err(|e| AppError::InvalidInput(format!("não consegui falar com o plex.tv: {e}")))
}

#[derive(Debug, Deserialize)]
struct MediaServerMappingForm {
    server_id: String,
    remote_prefix: String,
    local_prefix: String,
}

async fn media_server_mappings_create(
    State(state): State<AppState>,
    Form(form): Form<MediaServerMappingForm>,
) -> Result<Response, AppError> {
    let server_id = parse_media_server_id(&form.server_id)?;
    media_server_mappings::insert(
        state.pool(),
        media_server_mappings::NewMediaServerMapping {
            server_id,
            remote_prefix: &form.remote_prefix,
            local_prefix: &form.local_prefix,
        },
    )
    .await?;
    render_media_server_mappings_partial(&state).await
}

async fn media_server_mappings_delete(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Response, AppError> {
    let uuid = parse_media_server_id(&id)?;
    if !media_server_mappings::delete_by_id(state.pool(), uuid).await? {
        return Err(AppError::NotFound(format!("media_server_mapping {id}")));
    }
    render_media_server_mappings_partial(&state).await
}

fn parse_media_server_id(id: &str) -> Result<Uuid, AppError> {
    Uuid::parse_str(id).map_err(|e| AppError::InvalidInput(format!("id inválido: {e}")))
}

async fn media_server_views(state: &AppState) -> Result<Vec<MediaServerView>, AppError> {
    Ok(media_servers::list_all(state.pool())
        .await?
        .into_iter()
        .map(|row| MediaServerView {
            id: row.id.to_string(),
            name: row.name.clone(),
            kind: row.kind.label().to_owned(),
            kind_label: row.kind.display_name().to_owned(),
            base_url: row.base_url.clone(),
            enabled: row.enabled,
            has_token: row.has_token(),
            uses_plex_login: row.kind.uses_plex_login(),
            last_notified_at: row.last_notified_at.map(format_ts),
            last_error: row.last_error.clone(),
        })
        .collect())
}

async fn media_server_mapping_block(
    state: &AppState,
) -> Result<(Vec<MediaServerMappingView>, Vec<MediaServerOption>), AppError> {
    let servers = media_servers::list_all(state.pool()).await?;
    let names: std::collections::HashMap<Uuid, String> =
        servers.iter().map(|s| (s.id, s.name.clone())).collect();
    let mappings = media_server_mappings::list_all(state.pool())
        .await?
        .into_iter()
        .map(|m| MediaServerMappingView {
            id: m.id.to_string(),
            server_name: names
                .get(&m.server_id)
                .cloned()
                .unwrap_or_else(|| "(removido)".to_owned()),
            remote_prefix: m.remote_prefix.clone(),
            local_prefix: m.local_prefix.to_string_lossy().into_owned(),
            specificity: crate::remote_path::specificity(&m.remote_prefix),
            // Read per render rather than stored: a bind mount can go
            // away without anything writing to this table.
            reachable: m.local_prefix.is_dir(),
        })
        .collect();
    let options = servers
        .into_iter()
        .map(|s| MediaServerOption {
            id: s.id.to_string(),
            name: s.name,
        })
        .collect();
    Ok((mappings, options))
}

async fn render_media_servers_partial(state: &AppState) -> Result<Response, AppError> {
    let servers = media_server_views(state).await?;
    let needs_credential = credential_gap(&servers);
    html(&MediaServersListPartial {
        servers,
        needs_credential,
    })
}

async fn render_media_server_mappings_partial(state: &AppState) -> Result<Response, AppError> {
    let (mappings, mapping_servers) = media_server_mapping_block(state).await?;
    html(&MediaServerMappingsPartial {
        mappings,
        mapping_servers,
    })
}

// ---------------------------------------------------------------------
// Root folders — where the import will put files. Nothing here touches
// the filesystem beyond checking that a path is usable.
// ---------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct RootFolderForm {
    path: String,
    /// `movie` / `tv`, or empty for a folder that serves both.
    #[serde(default)]
    media_type: Option<String>,
}

async fn root_folders_create(
    State(state): State<AppState>,
    Form(form): Form<RootFolderForm>,
) -> Result<Response, AppError> {
    let media_type = match form.media_type.as_deref().map_or("", str::trim) {
        "" => None,
        other => Some(crate::db::library::media_type_from_label(other)?),
    };
    root_folders::insert(state.pool(), &form.path, media_type).await?;
    render_root_folders_partial(&state).await
}

async fn root_folders_delete(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Response, AppError> {
    let uuid = Uuid::parse_str(&id)
        .map_err(|e| AppError::InvalidInput(format!("invalid root folder id: {e}")))?;
    if !root_folders::delete_by_id(state.pool(), uuid).await? {
        return Err(AppError::NotFound(format!("root_folder {uuid}")));
    }
    render_root_folders_partial(&state).await
}

#[derive(Debug, Deserialize)]
struct PathMappingForm {
    client_id: String,
    remote_prefix: String,
    local_prefix: String,
}

async fn path_mappings_create(
    State(state): State<AppState>,
    Form(form): Form<PathMappingForm>,
) -> Result<Response, AppError> {
    let client_id = Uuid::parse_str(form.client_id.trim())
        .map_err(|e| AppError::InvalidInput(format!("cliente inválido: {e}")))?;
    path_mappings::insert(
        state.pool(),
        crate::db::path_mappings::NewPathMapping {
            client_id,
            remote_prefix: &form.remote_prefix,
            local_prefix: &form.local_prefix,
        },
    )
    .await?;
    render_path_mappings_partial(&state).await
}

async fn path_mappings_delete(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Response, AppError> {
    let uuid = Uuid::parse_str(&id)
        .map_err(|e| AppError::InvalidInput(format!("invalid path mapping id: {e}")))?;
    if !path_mappings::delete_by_id(state.pool(), uuid).await? {
        return Err(AppError::NotFound(format!("path_mapping {uuid}")));
    }
    render_path_mappings_partial(&state).await
}

/// Put a failed import back in the queue. Nothing is downloaded again —
/// the file is still where the client left it.
async fn grab_requeue_import(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Response, AppError> {
    let uuid = Uuid::parse_str(&id)
        .map_err(|e| AppError::InvalidInput(format!("invalid grab id: {e}")))?;
    // A `false` here is not an error: the operator double-clicked, or
    // another tab already requeued it. The refreshed block tells the
    // truth either way.
    grabs::requeue_import(state.pool(), uuid).await?;
    render_path_mappings_partial(&state).await
}

async fn render_path_mappings_partial(state: &AppState) -> Result<Response, AppError> {
    let block = path_mapping_block(state).await?;
    html(&block)
}

async fn path_mapping_block(state: &AppState) -> Result<PathMappingsPartial, AppError> {
    let clients = download_clients::list_all(state.pool()).await?;
    let names: std::collections::HashMap<Uuid, String> =
        clients.iter().map(|c| (c.id, c.name.clone())).collect();

    let mappings = path_mappings::list_all(state.pool())
        .await?
        .into_iter()
        .map(|m| PathMappingView {
            id: m.id.to_string(),
            client_name: names
                .get(&m.client_id)
                .cloned()
                .unwrap_or_else(|| "—".to_owned()),
            specificity: crate::remote_path::specificity(&m.remote_prefix),
            remote_prefix: m.remote_prefix,
            // One `metadata` per row, same trade as the root-folder
            // table: the list is tiny and a cached answer would go stale
            // the moment a bind mount changed.
            reachable: std::fs::metadata(&m.local_prefix).is_ok_and(|meta| meta.is_dir()),
            local_prefix: m.local_prefix.to_string_lossy().into_owned(),
        })
        .collect();

    let titles = library::titles_by_id(state.pool()).await?;
    let stuck = grabs::retryable_imports(state.pool())
        .await?
        .into_iter()
        .map(|g| StuckImportView {
            id: g.id.to_string(),
            release_name: g.release_name,
            item_title: titles
                .get(&g.item_id)
                .cloned()
                .unwrap_or_else(|| "item removido".to_owned()),
            error: g
                .error
                .unwrap_or_else(|| "sem motivo registrado".to_owned()),
            client_name: g
                .client_id
                .and_then(|id| names.get(&id).cloned())
                .unwrap_or_else(|| "—".to_owned()),
        })
        .collect();

    Ok(PathMappingsPartial {
        mappings,
        mapping_clients: clients
            .into_iter()
            .map(|c| PathMappingClientOption {
                id: c.id.to_string(),
                name: c.name,
            })
            .collect(),
        stuck,
    })
}

async fn render_root_folders_partial(state: &AppState) -> Result<Response, AppError> {
    html(&RootFoldersListPartial {
        root_folders: root_folder_views(state).await?,
    })
}

async fn root_folder_views(state: &AppState) -> Result<Vec<RootFolderView>, AppError> {
    Ok(root_folders::list_all(state.pool())
        .await?
        .into_iter()
        .map(|folder| {
            // Reading free space hits the filesystem once per row. The
            // list is a handful of entries and the alternative — caching
            // a figure that changes with every download — would be worse.
            let usage = folder.usage();
            RootFolderView {
                id: folder.id.to_string(),
                path: folder.path.to_string_lossy().into_owned(),
                content: match folder.media_type {
                    Some(crate::db::library::MediaType::Movie) => "Filmes".to_string(),
                    Some(crate::db::library::MediaType::Tv) => "Séries".to_string(),
                    None => "Filmes e séries".to_string(),
                },
                free: usage.map_or_else(
                    || "caminho inacessível".to_string(),
                    |u| {
                        format!(
                            "{} livres de {}",
                            humanize_bytes(u.available),
                            humanize_bytes(u.total)
                        )
                    },
                ),
                used_percent: usage.map_or(0, crate::db::root_folders::DiskUsage::used_percent),
                reachable: usage.is_some(),
            }
        })
        .collect())
}

/// Build the `ArrInstancesListPartial` shared by the toggle + update
/// handlers — same join-against-profiles pattern the index uses, but
/// returned as a partial so HTMX can swap just the table.
async fn render_arr_instances_partial(state: &AppState) -> Result<Response, AppError> {
    let rows = arr_instances::list_all(state.pool()).await?;
    let profile_rows = quality_profiles::list_all(state.pool()).await?;
    let profile_by_id: std::collections::HashMap<
        Uuid,
        &crate::db::quality_profiles::QualityProfileRow,
    > = profile_rows.iter().map(|p| (p.id, p)).collect();
    let mut instances: Vec<_> = rows
        .iter()
        .map(|r| arr_instance_view_with_profile(r, &profile_by_id))
        .collect();
    fill_webhook_urls(state, None, &mut instances);
    html(&ArrInstancesListPartial { instances })
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

    use super::{audio_chips_from_languages, scan_badge, subtitle_chips_from_languages};
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

    /// The badge the operator reported: the automatic search said
    /// "nada encontrado — nenhuma release passou do threshold" while the
    /// magnifier on the same episode listed nine releases, seven of them
    /// above the line. Both halves were false. `TargetOutcome::Nothing`
    /// used to land in the same counter as "nothing passed", so a sweep
    /// blocked entirely by the barrier blamed the trackers and the
    /// profile — the two places the fix is *not*.
    #[test]
    fn a_sweep_the_barrier_blocked_does_not_blame_the_trackers() {
        let summary = crate::scan::ScanSummary {
            targets: 1,
            searches: 1,
            exhausted: 1,
            ..crate::scan::ScanSummary::default()
        };
        let badge = scan_badge(&summary);
        assert!(!badge.ok);
        assert_eq!(badge.label, "releases esgotadas");
        assert!(
            badge.detail.contains("histórico de grabs"),
            "the badge has to point at where the answer is: {}",
            badge.detail
        );
        assert!(
            !badge.detail.contains("threshold"),
            "the releases passed the threshold; saying otherwise is what sent              the operator to the profile editor: {}",
            badge.detail
        );
    }

    /// And the genuine empty-handed sweep keeps its own answer, now
    /// naming both of the things that produce it.
    #[test]
    fn a_sweep_that_really_found_nothing_still_says_so() {
        let summary = crate::scan::ScanSummary {
            targets: 1,
            searches: 1,
            no_candidate: 1,
            ..crate::scan::ScanSummary::default()
        };
        let badge = scan_badge(&summary);
        assert_eq!(badge.label, "nada encontrado");
        assert!(badge.detail.contains("não devolveram nada"));
        assert!(badge.detail.contains("threshold"));
    }
}
