//! Integration tests for the trusted-peer auth bypass.
//!
//! Spins up the real Axum router wired with
//! `into_make_service_with_connect_info::<SocketAddr>()` so the
//! middleware can read the actual TCP peer. Tests then drive requests
//! from `127.0.0.1` (the test client's own address) and verify that
//! the bypass triggers when configured and does not when it isn't.
//!
//! Covers:
//! - empty bypass + no cookie → 303 to `/login`
//! - `loopback` bypass + no cookie → 200 (bypass triggered)
//! - `X-Forwarded-For` from an *untrusted* peer is ignored
//! - `X-Forwarded-For` from a *trusted* proxy is honored

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::similar_names,
    clippy::doc_markdown
)]

use std::net::SocketAddr;
use std::time::Duration;

use brarr_decision_service::Engine;
use brarr_orchestrator::{AppState, AuthConfig, BypassConfig, TrustedPeers, db, web};

const TOKEN: &str = "bypass-test-token-1234";

async fn spawn_with_bypass(auth: AuthConfig, bypass: BypassConfig) -> SocketAddr {
    let pool = db::open_memory().await.expect("open in-memory db");
    let state = AppState::with_auth_and_bypass(pool, Engine::baseline(), auth, bypass);
    let static_dir = std::env::temp_dir().join("brarr-orchestrator-bypass-test-static");
    let _ = tokio::fs::create_dir_all(&static_dir).await;
    let router = web::router(state, &static_dir);

    let listener = tokio::net::TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0)))
        .await
        .expect("bind");
    let addr = listener.local_addr().expect("local addr");
    tokio::spawn(async move {
        let _ = axum::serve(
            listener,
            router.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .await;
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
async fn empty_bypass_still_requires_cookie() {
    let addr = spawn_with_bypass(
        AuthConfig::from_optional(Some(TOKEN)),
        BypassConfig::default(),
    )
    .await;
    let client = no_redirect_client();
    let resp = client
        .get(format!("http://{addr}/"))
        .send()
        .await
        .expect("send");
    assert_eq!(resp.status(), 303, "no bypass → redirect to login");
    assert_eq!(
        resp.headers().get("location").and_then(|v| v.to_str().ok()),
        Some("/login")
    );
}

#[tokio::test]
async fn loopback_bypass_lets_request_through_without_cookie() {
    let bypass = BypassConfig {
        peers: TrustedPeers::parse("loopback").expect("loopback parses"),
        proxies: TrustedPeers::default(),
    };
    let addr = spawn_with_bypass(AuthConfig::from_optional(Some(TOKEN)), bypass).await;
    let client = no_redirect_client();
    let resp = client
        .get(format!("http://{addr}/"))
        .send()
        .await
        .expect("send");
    assert_eq!(
        resp.status(),
        200,
        "loopback peer (127.0.0.1) should be allowed in without cookie"
    );
    let body = resp.text().await.unwrap();
    assert!(body.contains("Dashboard"));
}

#[tokio::test]
async fn xff_ignored_when_peer_not_trusted_proxy() {
    // Bypass list contains a public address. The test client connects
    // from 127.0.0.1, which is NOT in the proxy list — so XFF must be
    // ignored, peer 127.0.0.1 does not match the bypass list, and the
    // request gets redirected to /login.
    let bypass = BypassConfig {
        peers: TrustedPeers::parse("203.0.113.0/24").expect("ok"),
        proxies: TrustedPeers::default(),
    };
    let addr = spawn_with_bypass(AuthConfig::from_optional(Some(TOKEN)), bypass).await;
    let client = no_redirect_client();
    let resp = client
        .get(format!("http://{addr}/"))
        .header("x-forwarded-for", "203.0.113.50")
        .send()
        .await
        .expect("send");
    assert_eq!(
        resp.status(),
        303,
        "XFF must be ignored when peer is not in the trusted-proxy list"
    );
}

#[tokio::test]
async fn xff_honored_when_peer_is_trusted_proxy() {
    // 127.0.0.1 is the trusted proxy; XFF carries the original client
    // address (203.0.113.50), which IS in the bypass list. Auth should
    // be skipped.
    let bypass = BypassConfig {
        peers: TrustedPeers::parse("203.0.113.0/24").expect("ok"),
        proxies: TrustedPeers::parse("loopback").expect("ok"),
    };
    let addr = spawn_with_bypass(AuthConfig::from_optional(Some(TOKEN)), bypass).await;
    let client = no_redirect_client();
    let resp = client
        .get(format!("http://{addr}/"))
        .header("x-forwarded-for", "203.0.113.50")
        .send()
        .await
        .expect("send");
    assert_eq!(
        resp.status(),
        200,
        "XFF should be honored when peer is a trusted proxy and the resolved client is in the bypass list"
    );
}

#[tokio::test]
async fn bypass_does_not_short_circuit_login_endpoint() {
    // /login is outside the auth middleware entirely (it has to be
    // reachable so users can submit the token form). The bypass
    // affects only the protected sub-router; the login form should
    // still render its 200 page whether or not bypass is configured.
    let bypass = BypassConfig {
        peers: TrustedPeers::parse("loopback").expect("ok"),
        proxies: TrustedPeers::default(),
    };
    let addr = spawn_with_bypass(AuthConfig::from_optional(Some(TOKEN)), bypass).await;
    let client = no_redirect_client();
    let resp = client
        .get(format!("http://{addr}/login"))
        .send()
        .await
        .expect("send");
    assert_eq!(resp.status(), 200);
}
