//! `Brarr` gRPC service implementation backed by [`AppState`].
//!
//! Tonic generates the trait `brarr_server::Brarr` from `brarr.proto`;
//! we implement it against the same [`crate::AppState`] the HTTP router
//! uses so both surfaces remain feature-equivalent.

use std::net::SocketAddr;

use brarr_core::TmdbId;
use tonic::{Request, Response, Status, transport::Server};
use tracing::info;

use super::proto::{
    ListTrackersReply, ListTrackersRequest, RecentSearchesReply, RecentSearchesRequest,
    ReleaseOutcome, SearchReply, SearchRequest, SearchSummary, TrackerSummary,
    brarr_server::{Brarr, BrarrServer},
};
use crate::db::{decisions, searches, trackers};
use crate::search::run_tmdb_search;
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
        let tmdb = TmdbId::new(req.tmdb_id).map_err(|e| {
            Status::invalid_argument(format!("invalid tmdb_id {}: {e}", req.tmdb_id))
        })?;
        let outcome = run_tmdb_search(&self.state, tmdb)
            .await
            .map_err(Status::from)?;

        let outcomes = outcome
            .decisions
            .into_iter()
            .map(|d| ReleaseOutcome {
                tracker_name: d.tracker_name,
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

    async fn list_trackers(
        &self,
        _request: Request<ListTrackersRequest>,
    ) -> Result<Response<ListTrackersReply>, Status> {
        let rows = trackers::list_all(self.state.pool())
            .await
            .map_err(Status::from)?;
        let trackers = rows
            .into_iter()
            .map(|t| TrackerSummary {
                id: t.id.to_string(),
                name: t.name,
                base_url: t.base_url.to_string(),
                kind: t.kind,
                created_at_unix: t.created_at.unix_timestamp(),
            })
            .collect();
        Ok(Response::new(ListTrackersReply { trackers }))
    }

    async fn recent_searches(
        &self,
        request: Request<RecentSearchesRequest>,
    ) -> Result<Response<RecentSearchesReply>, Status> {
        let limit = request.into_inner().limit;
        let rows = searches::recent(self.state.pool(), limit)
            .await
            .map_err(Status::from)?;
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
            });
        }
        Ok(Response::new(RecentSearchesReply { searches: out }))
    }
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
