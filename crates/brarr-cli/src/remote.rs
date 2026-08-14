//! Talk to a running `brarr-orchestrator` over gRPC.
//!
//! The CLI's `remote` subcommand dispatches here instead of doing a
//! local fan-out: it connects to the orchestrator, calls `Search`,
//! and renders the response with the existing local formatter
//! (`format_outcome` / `format_outcome_json`).
//!
//! Auth: when `--token` is set, the request goes out with
//! `authorization: Bearer <token>` metadata, matching what
//! `brarr_orchestrator::grpc::auth_interceptor` expects.

#![allow(
    clippy::pedantic,
    clippy::doc_markdown,
    missing_docs,
    reason = "generated tonic code lives inside the include_proto module below"
)]

pub mod proto {
    //! Tonic-generated client bindings for `brarr.v1`. Hidden behind a
    //! module so the rest of the crate sees a clean namespace.
    tonic::include_proto!("brarr.v1");
}

use std::time::Duration;

use brarr_core::{
    DecisionScore, ExternalIds, ImdbId, Release, ReleaseKind, ReleaseUrls, Resolution, TmdbId,
    TrackerSource,
};
use brarr_decision_service::DecisionOutcome;
use proto::brarr_client::BrarrClient;
use proto::{MaintenanceRequest, ReleaseOutcome, SearchRequest};
use tonic::metadata::MetadataValue;
use tonic::transport::Channel;
use tonic::transport::Endpoint;
use tracing::info;
use url::Url;

use crate::search::{ScoredRelease, SearchOutcome};

/// Errors specific to the remote (gRPC) search path.
#[derive(Debug, thiserror::Error)]
pub enum RemoteError {
    /// Failed to build the tonic endpoint URI.
    #[error("invalid orchestrator address: {0}")]
    Endpoint(#[from] tonic::transport::Error),
    /// Token contained bytes that can't be used as a gRPC metadata value.
    #[error("invalid auth token: must be ASCII")]
    InvalidToken,
    /// gRPC call failed (transport, unauthenticated, etc.).
    #[error("orchestrator returned error: {0}")]
    Rpc(#[from] tonic::Status),
}

/// Run a remote TMDb search against the orchestrator at `addr`.
///
/// `addr` is the bare host:port (e.g. `127.0.0.1:50051`); the function
/// prepends `http://` because tonic requires a scheme. Pass exactly
/// one of `tmdb`/`imdb` (or both — the orchestrator decides per-tracker).
///
/// # Errors
///
/// See [`RemoteError`].
pub async fn run_remote_search(
    addr: &str,
    token: Option<&str>,
    tmdb: Option<TmdbId>,
    imdb: Option<ImdbId>,
) -> Result<SearchOutcome, RemoteError> {
    let uri = if addr.starts_with("http://") || addr.starts_with("https://") {
        addr.to_string()
    } else {
        format!("http://{addr}")
    };
    let endpoint = Endpoint::from_shared(uri.clone())?
        .connect_timeout(Duration::from_secs(5))
        .timeout(Duration::from_secs(60));
    let channel: Channel = endpoint.connect().await?;
    info!(
        target: "brarr_cli::remote",
        %uri,
        tmdb = ?tmdb.map(TmdbId::get),
        imdb = ?imdb.map(ImdbId::get),
        "dispatching remote search"
    );

    let mut client = BrarrClient::new(channel);
    let mut request = tonic::Request::new(SearchRequest {
        tmdb_id: tmdb.map(TmdbId::get).unwrap_or(0),
        imdb_id: imdb.map(|i| format!("{:07}", i.get())).unwrap_or_default(),
    });
    if let Some(t) = token {
        let v = MetadataValue::try_from(format!("Bearer {t}"))
            .map_err(|_| RemoteError::InvalidToken)?;
        request.metadata_mut().insert("authorization", v);
    }

    let reply = client.search(request).await?.into_inner();

    let mut scored = Vec::with_capacity(reply.outcomes.len());
    for o in reply.outcomes {
        if let Some(sr) = remote_to_scored(&o) {
            scored.push(sr);
        }
    }

    Ok(SearchOutcome {
        scored,
        failures: Vec::new(),
    })
}

/// Row counts returned by a remote maintenance run.
#[derive(Debug, Clone, Copy)]
pub struct RemoteMaintenance {
    /// `decisions` rows the prune deleted.
    pub decisions_deleted: u64,
    /// `searches` rows the prune deleted.
    pub searches_deleted: u64,
    /// Retention window (days) the server applied.
    pub retention_days: u32,
}

/// Trigger a maintenance run on the orchestrator at `addr`: prune
/// history past the server's retention window and reclaim space,
/// optionally running a full `VACUUM`.
///
/// # Errors
///
/// See [`RemoteError`].
pub async fn run_remote_maintenance(
    addr: &str,
    token: Option<&str>,
    full_vacuum: bool,
) -> Result<RemoteMaintenance, RemoteError> {
    let uri = if addr.starts_with("http://") || addr.starts_with("https://") {
        addr.to_string()
    } else {
        format!("http://{addr}")
    };
    // A full VACUUM on a large DB can run for a while — give it room.
    let endpoint = Endpoint::from_shared(uri.clone())?
        .connect_timeout(Duration::from_secs(5))
        .timeout(Duration::from_secs(600));
    let channel: Channel = endpoint.connect().await?;
    info!(target: "brarr_cli::remote", %uri, full_vacuum, "dispatching remote maintenance");

    let mut client = BrarrClient::new(channel);
    let mut request = tonic::Request::new(MaintenanceRequest { full_vacuum });
    if let Some(t) = token {
        let v = MetadataValue::try_from(format!("Bearer {t}"))
            .map_err(|_| RemoteError::InvalidToken)?;
        request.metadata_mut().insert("authorization", v);
    }
    let reply = client.run_maintenance(request).await?.into_inner();
    Ok(RemoteMaintenance {
        decisions_deleted: reply.decisions_deleted,
        searches_deleted: reply.searches_deleted,
        retention_days: reply.retention_days,
    })
}

/// Convert a server-side [`ReleaseOutcome`] back to a local
/// [`ScoredRelease`]. Server-only fields that don't exist on
/// [`Release`] (e.g. detail URL) are left at their defaults; the local
/// formatter handles that gracefully.
fn remote_to_scored(o: &ReleaseOutcome) -> Option<ScoredRelease> {
    let tracker = TrackerSource::new(
        o.provider_name.clone(),
        Url::parse(&format!("https://{}.example/", sanitize(&o.provider_name))).ok()?,
    )
    .ok()?;
    let kind = parse_kind(&o.kind);
    let resolution = parse_resolution(&o.resolution);
    let id = if o.release_id_remote == 0 {
        "remote".to_string()
    } else {
        o.release_id_remote.to_string()
    };
    let mut release = Release::new(
        &id,
        tracker,
        &o.release_name,
        kind,
        resolution,
        o.size_bytes,
    )
    .ok()?;
    release.seeders = o.seeders;
    release.leechers = o.leechers;
    release.external_ids = ExternalIds::default();
    release.urls = ReleaseUrls::default();

    let outcome = DecisionOutcome {
        score: DecisionScore::saturating(o.score),
        tags: o.tags.clone(),
        rejected: o.rejected,
        matched_rules: o.matched_rules.clone(),
    };
    Some(ScoredRelease { release, outcome })
}

fn parse_kind(s: &str) -> ReleaseKind {
    match s {
        "WEB-DL" => ReleaseKind::WebDl,
        "BluRay" => ReleaseKind::BluRay,
        "Encode" => ReleaseKind::Encode,
        "HDTV" => ReleaseKind::HdTv,
        "DVD" => ReleaseKind::Dvd,
        other => ReleaseKind::Other(other.to_string()),
    }
}

fn parse_resolution(s: &str) -> Resolution {
    match s {
        "SD" => Resolution::Sd,
        "720p" => Resolution::P720,
        "1080p" => Resolution::P1080,
        "2160p" => Resolution::P2160,
        other => Resolution::Other(other.to_string()),
    }
}

/// Coerce a provider name into something that survives `Url::parse` in
/// the synthetic placeholder URL we feed `TrackerSource::new`. Real
/// tracker URLs aren't relayed by the gRPC response, so we just need
/// *something* legal here.
fn sanitize(name: &str) -> String {
    name.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[test]
    fn remote_to_scored_maps_canonical_fields() {
        let o = ReleaseOutcome {
            provider_name: "capybara".into(),
            release_name: "Matrix 1080p".into(),
            release_id_remote: 42,
            score: 120,
            rejected: false,
            tags: vec!["PT-BR".into()],
            matched_rules: vec!["PT-BR audio".into()],
            seeders: 10,
            leechers: 1,
            size_bytes: 1024,
            resolution: "1080p".into(),
            kind: "BluRay".into(),
        };
        let sr = remote_to_scored(&o).expect("convert");
        assert_eq!(sr.outcome.score.get(), 120);
        assert_eq!(sr.outcome.tags, vec!["PT-BR".to_string()]);
        assert_eq!(sr.release.title, "Matrix 1080p");
        assert!(matches!(sr.release.kind, ReleaseKind::BluRay));
        assert!(matches!(sr.release.resolution, Resolution::P1080));
        assert_eq!(sr.release.tracker.name, "capybara");
    }

    #[test]
    fn unknown_kind_resolution_fall_through_to_other() {
        let o = ReleaseOutcome {
            provider_name: "x".into(),
            release_name: "t".into(),
            release_id_remote: 1,
            score: 0,
            rejected: false,
            tags: vec![],
            matched_rules: vec![],
            seeders: 0,
            leechers: 0,
            size_bytes: 1,
            resolution: "8K".into(),
            kind: "Funkywunkkin".into(),
        };
        let sr = remote_to_scored(&o).unwrap();
        assert!(matches!(sr.release.kind, ReleaseKind::Other(ref s) if s == "Funkywunkkin"));
        assert!(matches!(sr.release.resolution, Resolution::Other(ref s) if s == "8K"));
    }

    #[test]
    fn sanitize_preserves_alnum_replaces_others() {
        assert_eq!(sanitize("capybara"), "capybara");
        assert_eq!(sanitize("foo.bar:7"), "foo-bar-7");
        assert_eq!(sanitize(""), "");
    }
}
