//! `Brarr` gRPC service implementation backed by [`AppState`].
//!
//! Tonic generates the trait `brarr_server::Brarr` from `brarr.proto`;
//! we implement it against the same [`crate::AppState`] the HTTP router
//! uses so both surfaces remain feature-equivalent.

use std::net::SocketAddr;

use brarr_core::{ImdbId, TmdbId};
use tonic::{Request, Response, Status, transport::Server};
use tracing::info;

use super::proto::{
    ListProvidersReply, ListProvidersRequest, MaintenanceReply, MaintenanceRequest,
    ProviderSummary, RecentSearchesReply, RecentSearchesRequest, ReleaseOutcome, SearchReply,
    SearchRequest, SearchSummary,
    brarr_server::{Brarr, BrarrServer},
};
use crate::db::{decisions, maintenance, providers, searches};
use crate::search::{SearchKeys, run_search};
use crate::{AppError, AppState};

/// Tonic service struct.
#[derive(Clone)]
pub struct BrarrService {
    state: AppState,
}

impl BrarrService {
    /// Build a new gRPC service wrapping `state`.
    #[must_use]
    pub const fn new(state: AppState) -> Self {
        Self { state }
    }
}

#[tonic::async_trait]
impl Brarr for BrarrService {
    async fn search(
        &self,
        request: Request<SearchRequest>,
    ) -> Result<Response<SearchReply>, Status> {
        let req = request.into_inner();
        let keys = build_search_keys(&req)?;
        let outcome = run_search(&self.state, keys).await.map_err(Status::from)?;

        let outcomes = outcome
            .decisions
            .into_iter()
            .map(|d| ReleaseOutcome {
                provider_name: d.provider_name,
                release_name: d.release_name,
                release_id_remote: d.release_id_remote,
                score: d.score,
                rejected: d.rejected,
                tags: d.tags,
                matched_rules: d.matched_rules,
                seeders: d.seeders,
                leechers: d.leechers,
                size_bytes: d.size_bytes,
                resolution: d.resolution,
                kind: d.kind,
            })
            .collect();

        Ok(Response::new(SearchReply {
            search_id: outcome.search.id.to_string(),
            outcomes,
        }))
    }

    async fn list_providers(
        &self,
        _request: Request<ListProvidersRequest>,
    ) -> Result<Response<ListProvidersReply>, Status> {
        let rows = providers::list_all(self.state.pool())
            .await
            .map_err(Status::from)?;
        let providers = rows
            .into_iter()
            .map(|p| ProviderSummary {
                id: p.id.to_string(),
                name: p.name,
                base_url: p.base_url.to_string(),
                kind: p.kind,
                created_at_unix: p.created_at.unix_timestamp(),
            })
            .collect();
        Ok(Response::new(ListProvidersReply { providers }))
    }

    async fn recent_searches(
        &self,
        request: Request<RecentSearchesRequest>,
    ) -> Result<Response<RecentSearchesReply>, Status> {
        let req = request.into_inner();
        let rows = if request_has_filters(&req) {
            let params = searches::FilterParams {
                tmdb_id: req.tmdb_id,
                imdb_id: req.imdb_id.as_ref().filter(|s| !s.is_empty()).cloned(),
                tvdb_id: req.tvdb_id,
                season: req.season.and_then(|s| u16::try_from(s).ok()),
                episode: req.episode.and_then(|e| u16::try_from(e).ok()),
                from_unix: req.from_unix,
                to_unix: req.to_unix,
                has_kept_decision: req.has_kept_decision,
                limit: req.limit,
                offset: req.offset,
            };
            searches::filter(self.state.pool(), params)
                .await
                .map_err(Status::from)?
        } else {
            searches::recent(self.state.pool(), req.limit)
                .await
                .map_err(Status::from)?
        };

        let mut out = Vec::with_capacity(rows.len());
        for r in rows {
            // Pull live decision count rather than trusting the
            // denormalized `result_count` — keeps the gRPC view fresh if
            // a future migration drifts the columns.
            let count = decisions::list_for_search(self.state.pool(), r.id)
                .await
                .map_or(r.result_count, |d| {
                    u32::try_from(d.iter().filter(|x| !x.rejected).count()).unwrap_or(0)
                });
            out.push(SearchSummary {
                id: r.id.to_string(),
                tmdb_id: r.tmdb_id.unwrap_or(0),
                submitted_at_unix: r.submitted_at.unix_timestamp(),
                result_count: count,
                imdb_id: r.imdb_id.unwrap_or_default(),
                tvdb_id: r.tvdb_id.unwrap_or(0),
                season: r.season.map_or(0, u32::from),
                episode: r.episode.map_or(0, u32::from),
            });
        }
        Ok(Response::new(RecentSearchesReply { searches: out }))
    }

    async fn run_maintenance(
        &self,
        request: Request<MaintenanceRequest>,
    ) -> Result<Response<MaintenanceReply>, Status> {
        let req = request.into_inner();
        let pool = self.state.pool();
        let retention_days = self.state.retention_days();
        let outcome = maintenance::run_prune(pool, retention_days)
            .await
            .map_err(Status::from)?;
        // Best-effort reclaim — surface prune counts even if a pragma trips.
        if let Err(e) = maintenance::checkpoint_wal(pool).await {
            info!(target: "brarr_orchestrator::grpc", error = %e, "maintenance: wal checkpoint failed");
        }
        if let Err(e) = maintenance::incremental_vacuum(pool).await {
            info!(target: "brarr_orchestrator::grpc", error = %e, "maintenance: incremental vacuum failed");
        }
        if req.full_vacuum {
            maintenance::full_vacuum(pool).await.map_err(Status::from)?;
        }
        info!(
            target: "brarr_orchestrator::grpc",
            decisions_deleted = outcome.decisions_deleted,
            searches_deleted = outcome.searches_deleted,
            retention_days,
            full_vacuum = req.full_vacuum,
            "ran maintenance via grpc"
        );
        Ok(Response::new(MaintenanceReply {
            decisions_deleted: outcome.decisions_deleted,
            searches_deleted: outcome.searches_deleted,
            retention_days,
        }))
    }
}

/// Any filter field set ⇒ run the filtered path (which respects
/// `offset` for pagination); otherwise stay on the cheap
/// `searches::recent` path so existing clients see no behavior
/// change.
fn request_has_filters(req: &RecentSearchesRequest) -> bool {
    req.tmdb_id.is_some()
        || req.imdb_id.as_ref().is_some_and(|s| !s.is_empty())
        || req.tvdb_id.is_some()
        || req.season.is_some()
        || req.episode.is_some()
        || req.from_unix.is_some()
        || req.to_unix.is_some()
        || req.has_kept_decision.is_some()
        || req.offset > 0
}

/// Start the gRPC server on `addr` and serve until shutdown is signalled.
///
/// When [`crate::AuthConfig::Enabled`] is in effect, every RPC must
/// present a matching `authorization: Bearer <token>` metadata entry;
/// otherwise the call returns `unauthenticated`.
///
/// # Errors
///
/// Surfaces any tonic transport error.
pub async fn serve(state: AppState, addr: SocketAddr) -> Result<(), AppError> {
    info!(target: "brarr_orchestrator::grpc", %addr, "starting gRPC server");
    let auth_state = state.clone();
    let svc =
        BrarrServer::with_interceptor(BrarrService::new(state), move |req: tonic::Request<()>| {
            auth_interceptor(&auth_state, req)
        });
    Server::builder()
        .add_service(svc)
        .serve(addr)
        .await
        .map_err(|e| AppError::Io(std::io::Error::other(e)))?;
    Ok(())
}

/// Translate the protobuf `SearchRequest` into the typed [`SearchKeys`]
/// used by [`run_search`]. Accepts a TMDb id (any non-zero `u32`), an
/// IMDb id (numeric, optionally with leading `tt`), or both. Returns
/// `Status::invalid_argument` when neither id is usable so the caller
/// gets a clear error instead of an empty result.
fn build_search_keys(req: &SearchRequest) -> Result<SearchKeys, Status> {
    let tmdb = if req.tmdb_id == 0 {
        None
    } else {
        Some(TmdbId::new(req.tmdb_id).map_err(|e| {
            Status::invalid_argument(format!("invalid tmdb_id {}: {e}", req.tmdb_id))
        })?)
    };

    let imdb = if req.imdb_id.trim().is_empty() {
        None
    } else {
        let raw = req.imdb_id.trim().trim_start_matches("tt");
        let n: u32 = raw.parse().map_err(|_| {
            Status::invalid_argument(format!(
                "invalid imdb_id {:?}: expected numeric tt-id",
                req.imdb_id
            ))
        })?;
        Some(ImdbId::new(n).map_err(|e| {
            Status::invalid_argument(format!("invalid imdb_id {}: {e}", req.imdb_id))
        })?)
    };

    if tmdb.is_none() && imdb.is_none() {
        return Err(Status::invalid_argument(
            "SearchRequest must set tmdb_id or imdb_id",
        ));
    }
    Ok(SearchKeys {
        tmdb,
        imdb,
        ..SearchKeys::default()
    })
}

/// Bearer-token interceptor used by [`serve`]. Exposed for tests so
/// they can call it directly without spinning up the transport.
///
/// # Errors
///
/// Returns [`tonic::Status::unauthenticated`] when auth is enabled but
/// the request did not present the expected token.
pub fn auth_interceptor(
    state: &AppState,
    req: tonic::Request<()>,
) -> Result<tonic::Request<()>, tonic::Status> {
    if !state.auth().is_enabled() {
        return Ok(req);
    }
    let token = req
        .metadata()
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|raw| {
            raw.strip_prefix("Bearer ")
                .or_else(|| raw.strip_prefix("bearer "))
        });
    match token {
        Some(t) if state.auth().token_matches(t.trim()) => Ok(req),
        _ => Err(tonic::Status::unauthenticated(
            "missing or invalid bearer token",
        )),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use crate::db::open_memory;
    use brarr_decision_service::Engine;
    use time::OffsetDateTime;
    use uuid::Uuid;

    #[tokio::test]
    async fn run_maintenance_prunes_old_history_and_reports_counts() {
        let pool = open_memory().await.unwrap();
        let old = OffsetDateTime::now_utc().unix_timestamp() - 30 * 86_400;
        let search = Uuid::new_v4();
        sqlx::query(
            "INSERT INTO searches (id, tmdb_id, imdb_id, submitted_at, result_count, request_json) \
             VALUES (?, NULL, NULL, ?, 0, '{}')",
        )
        .bind(search.to_string())
        .bind(old)
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO decisions \
               (id, search_id, provider_id, provider_name, release_name, release_id_remote, \
                score, rejected, decided_at) \
             VALUES (?, ?, NULL, 'p', 'r', 0, 0, 0, ?)",
        )
        .bind(Uuid::new_v4().to_string())
        .bind(search.to_string())
        .bind(old)
        .execute(&pool)
        .await
        .unwrap();

        // Default RuntimeConfig → retention 7 days. full_vacuum=true also
        // exercises the VACUUM branch.
        let svc = BrarrService::new(AppState::new(pool, Engine::baseline()));
        let reply = svc
            .run_maintenance(Request::new(MaintenanceRequest { full_vacuum: true }))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(reply.retention_days, 7);
        assert_eq!(reply.decisions_deleted, 1);
        assert_eq!(reply.searches_deleted, 1);
    }
}
