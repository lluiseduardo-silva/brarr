//! Auth integration tests for the HTTP UI and gRPC service.
//!
//! Two halves:
//! 1. HTTP — spin up the Axum router with `AuthConfig::Enabled` and
//!    verify that protected routes redirect to `/login` until the
//!    session cookie matches the configured token. Health and login
//!    endpoints stay open.
//! 2. gRPC — call [`grpc::auth_interceptor`] directly with a fake
//!    `tonic::Request<()>` and assert it accepts / rejects the right
//!    bearer values. Skips the transport so the test stays cheap.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::similar_names,
    clippy::doc_markdown
)]

use std::net::SocketAddr;
use std::time::Duration;

use brarr_decision_service::Engine;
use brarr_orchestrator::{AppState, AuthConfig, db, grpc, web};

const TOKEN: &str = "test-token-1234";

async fn spawn_with_auth(auth: AuthConfig) -> SocketAddr {
    let pool = db::open_memory().await.expect("open in-memory db");
    let state = AppState::with_auth(pool, Engine::baseline(), auth);
    let static_dir = std::env::temp_dir().join("brarr-orchestrator-auth-test-static");
    let _ = tokio::fs::create_dir_all(&static_dir).await;
    let router = web::router(state, &static_dir);

    let listener = tokio::net::TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0)))
        .await
        .expect("bind");
    let addr = listener.local_addr().expect("local addr");
    tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });
    tokio::time::sleep(Duration::from_millis(50)).await;
    addr
}

fn no_redirect_client() -> reqwest::Client {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap()
}

#[tokio::test]
async fn protected_route_redirects_to_login_when_no_cookie() {
    let addr = spawn_with_auth(AuthConfig::from_optional(Some(TOKEN))).await;
    let client = no_redirect_client();
    let resp = client
        .get(format!("http://{addr}/"))
        .send()
        .await
        .expect("send");
    assert_eq!(resp.status(), 303);
    assert_eq!(
        resp.headers().get("location").and_then(|v| v.to_str().ok()),
        Some("/login")
    );
}

#[tokio::test]
async fn protected_route_redirects_when_cookie_wrong() {
    let addr = spawn_with_auth(AuthConfig::from_optional(Some(TOKEN))).await;
    let client = no_redirect_client();
    let resp = client
        .get(format!("http://{addr}/providers"))
        .header("cookie", "brarr_session=wrong-value")
        .send()
        .await
        .expect("send");
    assert_eq!(resp.status(), 303);
}

#[tokio::test]
async fn login_post_sets_cookie_and_redirects_home() {
    let addr = spawn_with_auth(AuthConfig::from_optional(Some(TOKEN))).await;
    let client = no_redirect_client();
    let resp = client
        .post(format!("http://{addr}/login"))
        .form(&[("token", TOKEN)])
        .send()
        .await
        .expect("send");
    assert_eq!(resp.status(), 303);
    let cookie = resp
        .headers()
        .get("set-cookie")
        .and_then(|v| v.to_str().ok())
        .expect("set-cookie header");
    assert!(cookie.contains(&format!("brarr_session={TOKEN}")));
    assert!(cookie.contains("HttpOnly"));
    assert!(cookie.contains("SameSite=Strict"));
}

#[tokio::test]
async fn login_post_wrong_token_returns_401() {
    let addr = spawn_with_auth(AuthConfig::from_optional(Some(TOKEN))).await;
    let client = no_redirect_client();
    let resp = client
        .post(format!("http://{addr}/login"))
        .form(&[("token", "nope")])
        .send()
        .await
        .expect("send");
    assert_eq!(resp.status(), 401);
    let body = resp.text().await.unwrap();
    assert!(body.contains("inválido"));
}

#[tokio::test]
async fn cookie_matching_token_unlocks_dashboard() {
    let addr = spawn_with_auth(AuthConfig::from_optional(Some(TOKEN))).await;
    let client = no_redirect_client();
    let resp = client
        .get(format!("http://{addr}/"))
        .header("cookie", format!("brarr_session={TOKEN}"))
        .send()
        .await
        .expect("send");
    assert_eq!(resp.status(), 200);
    let body = resp.text().await.unwrap();
    assert!(body.contains("Dashboard"));
}

#[tokio::test]
async fn healthz_is_always_open() {
    let addr = spawn_with_auth(AuthConfig::from_optional(Some(TOKEN))).await;
    let client = reqwest::Client::new();
    let resp = client
        .get(format!("http://{addr}/healthz"))
        .send()
        .await
        .expect("send");
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.text().await.unwrap(), "ok");
}

#[tokio::test]
async fn logout_clears_cookie_with_max_age_zero() {
    let addr = spawn_with_auth(AuthConfig::from_optional(Some(TOKEN))).await;
    let client = no_redirect_client();
    let resp = client
        .post(format!("http://{addr}/logout"))
        .header("cookie", format!("brarr_session={TOKEN}"))
        .send()
        .await
        .expect("send");
    assert_eq!(resp.status(), 303);
    let cookie = resp
        .headers()
        .get("set-cookie")
        .and_then(|v| v.to_str().ok())
        .expect("set-cookie");
    assert!(cookie.contains("Max-Age=0"));
}

#[tokio::test]
async fn disabled_mode_lets_anything_through() {
    let addr = spawn_with_auth(AuthConfig::Disabled).await;
    let client = no_redirect_client();
    let resp = client
        .get(format!("http://{addr}/providers"))
        .send()
        .await
        .expect("send");
    assert_eq!(resp.status(), 200);
}

// ---------- gRPC interceptor ----------

#[tokio::test]
async fn grpc_interceptor_rejects_missing_token_when_auth_enabled() {
    let pool = db::open_memory().await.expect("db");
    let state = AppState::with_auth(
        pool,
        Engine::baseline(),
        AuthConfig::from_optional(Some(TOKEN)),
    );
    let req: tonic::Request<()> = tonic::Request::new(());
    let err = grpc::auth_interceptor(&state, req).unwrap_err();
    assert_eq!(err.code(), tonic::Code::Unauthenticated);
}

#[tokio::test]
async fn grpc_interceptor_accepts_correct_bearer() {
    let pool = db::open_memory().await.expect("db");
    let state = AppState::with_auth(
        pool,
        Engine::baseline(),
        AuthConfig::from_optional(Some(TOKEN)),
    );
    let mut req: tonic::Request<()> = tonic::Request::new(());
    req.metadata_mut()
        .insert("authorization", format!("Bearer {TOKEN}").parse().unwrap());
    let ok = grpc::auth_interceptor(&state, req);
    assert!(ok.is_ok());
}

#[tokio::test]
async fn grpc_interceptor_rejects_wrong_bearer() {
    let pool = db::open_memory().await.expect("db");
    let state = AppState::with_auth(
        pool,
        Engine::baseline(),
        AuthConfig::from_optional(Some(TOKEN)),
    );
    let mut req: tonic::Request<()> = tonic::Request::new(());
    req.metadata_mut()
        .insert("authorization", "Bearer something-else".parse().unwrap());
    let err = grpc::auth_interceptor(&state, req).unwrap_err();
    assert_eq!(err.code(), tonic::Code::Unauthenticated);
}

#[tokio::test]
async fn grpc_interceptor_passthrough_when_auth_disabled() {
    let pool = db::open_memory().await.expect("db");
    let state = AppState::with_auth(pool, Engine::baseline(), AuthConfig::Disabled);
    let req: tonic::Request<()> = tonic::Request::new(());
    assert!(grpc::auth_interceptor(&state, req).is_ok());
}
