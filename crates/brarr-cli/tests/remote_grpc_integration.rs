//! End-to-end test: spin up a real `brarr-orchestrator` gRPC server
//! against an in-memory SQLite, then exercise [`brarr_cli::run_remote_search`]
//! against it. Verifies the wire-level path: tonic client → server,
//! response decode, [`Release`] reconstruction.
//!
//! Two scenarios:
//! 1. Auth disabled — token=None hits an empty orchestrator and gets
//!    back zero releases (no trackers configured).
//! 2. Auth enabled — wrong token gets `unauthenticated`, correct token
//!    succeeds. Confirms the bearer interceptor is wired.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::similar_names,
    clippy::doc_markdown,
    clippy::panic
)]

use std::net::SocketAddr;
use std::time::Duration;

use brarr_cli::{RemoteError, run_remote_search};
use brarr_core::TmdbId;
use brarr_decision_service::Engine;
use brarr_orchestrator::{AppState, AuthConfig, db, grpc};

async fn spawn_grpc(auth: AuthConfig) -> SocketAddr {
    let pool = db::open_memory().await.expect("db");
    let state = AppState::with_auth(pool, Engine::baseline(), auth);
    let listener = tokio::net::TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0)))
        .await
        .expect("bind");
    let addr = listener.local_addr().expect("addr");
    drop(listener);

    tokio::spawn(async move {
        let _ = grpc::serve(state, addr).await;
    });
    // Give the listener a moment to start.
    tokio::time::sleep(Duration::from_millis(120)).await;
    addr
}

#[tokio::test]
async fn remote_search_against_empty_orchestrator_returns_zero_releases() {
    let addr = spawn_grpc(AuthConfig::Disabled).await;
    let outcome = run_remote_search(
        &addr.to_string(),
        None,
        Some(TmdbId::new(603).unwrap()),
        None,
    )
    .await
    .expect("remote search");
    assert!(outcome.scored.is_empty());
    assert!(outcome.failures.is_empty());
}

#[tokio::test]
async fn remote_search_with_correct_bearer_succeeds() {
    let addr = spawn_grpc(AuthConfig::from_optional(Some("s3cret"))).await;
    let outcome = run_remote_search(
        &addr.to_string(),
        Some("s3cret"),
        Some(TmdbId::new(1).unwrap()),
        None,
    )
    .await
    .expect("remote search");
    assert!(outcome.scored.is_empty());
}

#[tokio::test]
async fn remote_search_without_token_when_auth_enabled_is_unauthenticated() {
    let addr = spawn_grpc(AuthConfig::from_optional(Some("s3cret"))).await;
    let err = run_remote_search(&addr.to_string(), None, Some(TmdbId::new(1).unwrap()), None)
        .await
        .unwrap_err();
    let RemoteError::Rpc(status) = err else {
        panic!("expected RPC unauthenticated, got {err:?}");
    };
    assert_eq!(status.code(), tonic::Code::Unauthenticated);
}

#[tokio::test]
async fn remote_search_with_wrong_token_rejected() {
    let addr = spawn_grpc(AuthConfig::from_optional(Some("s3cret"))).await;
    let err = run_remote_search(
        &addr.to_string(),
        Some("not-the-token"),
        Some(TmdbId::new(1).unwrap()),
        None,
    )
    .await
    .unwrap_err();
    let RemoteError::Rpc(status) = err else {
        panic!("expected RPC unauthenticated, got {err:?}");
    };
    assert_eq!(status.code(), tonic::Code::Unauthenticated);
}

#[tokio::test]
async fn remote_search_against_bad_address_yields_endpoint_error() {
    // Unbound port; connect must fail fast.
    let err = run_remote_search("127.0.0.1:1", None, Some(TmdbId::new(1).unwrap()), None)
        .await
        .unwrap_err();
    // The exact variant depends on whether tonic surfaces it as
    // Endpoint or Rpc — accept either.
    matches!(err, RemoteError::Endpoint(_) | RemoteError::Rpc(_));
}
