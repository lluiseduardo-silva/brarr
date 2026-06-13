//! Integration tests for the inbound `/webhooks/{radarr,sonarr}/{id}`
//! routes. Spins up the real Axum router against an in-memory sqlite
//! pool, inserts a fake `arr_instances` row per test, and POSTs
//! Radarr/Sonarr Connect payloads captured in
//! `tests/fixtures/webhooks/`.
//!
//! Coverage:
//! - apikey gating (missing → 401, valid → accepted, trusted-peer
//!   bypass → accepted)
//! - kind mismatch (Radarr payload to a Sonarr URL) → 400
//! - `Test` event → 200, audit row but no search row
//! - `MovieAdded` / `EpisodeAdded` → 202, audit row, search row
//!   appears once the spawned task lands
//! - unknown event type → 200, audit row, no search

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::similar_names,
    clippy::doc_markdown
)]

use std::net::SocketAddr;
use std::time::Duration;

use brarr_arr::ArrKind;
use brarr_decision_service::Engine;
use brarr_orchestrator::db::{arr_instances, webhook_events};
use brarr_orchestrator::{AppState, AuthConfig, BypassConfig, TrustedPeers, db, web};
use sqlx::Row;
use uuid::Uuid;

const TOKEN: &str = "webhook-test-token";

async fn boot(pool: db::Pool, auth: AuthConfig, bypass: BypassConfig) -> SocketAddr {
    let state = AppState::with_auth_and_bypass(pool, Engine::baseline(), auth, bypass);
    let static_dir = std::env::temp_dir().join("brarr-orchestrator-webhooks-test-static");
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

async fn make_arr(pool: &db::Pool, name: &str, kind: ArrKind) -> Uuid {
    let url = url::Url::parse("https://arr.example/").unwrap();
    arr_instances::insert(
        pool,
        arr_instances::NewArrInstance {
            name,
            kind,
            base_url: &url,
            api_key: "fake",
            push_threshold: None,
            profile_id: None,
            enabled: None,
        },
    )
    .await
    .unwrap()
    .id
}

fn fixture(name: &str) -> String {
    std::fs::read_to_string(format!(
        "{}/tests/fixtures/webhooks/{name}",
        env!("CARGO_MANIFEST_DIR")
    ))
    .unwrap_or_else(|e| panic!("read fixture {name}: {e}"))
}

async fn search_row_count(pool: &db::Pool) -> i64 {
    let row = sqlx::query("SELECT COUNT(*) AS n FROM searches")
        .fetch_one(pool)
        .await
        .unwrap();
    row.try_get::<i64, _>("n").unwrap()
}

async fn wait_for_search_row(pool: &db::Pool) -> i64 {
    for _ in 0..50 {
        let n = search_row_count(pool).await;
        if n > 0 {
            return n;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    search_row_count(pool).await
}

#[tokio::test]
async fn radarr_test_event_returns_200_and_persists_audit_row_without_search() {
    let pool = db::open_memory().await.unwrap();
    let arr_id = make_arr(&pool, "radarr-main", ArrKind::Radarr).await;
    let addr = boot(
        pool.clone(),
        AuthConfig::from_optional(Some(TOKEN)),
        BypassConfig::default(),
    )
    .await;
    let client = reqwest::Client::new();
    let resp = client
        .post(format!(
            "http://{addr}/webhooks/radarr/{arr_id}?apikey={TOKEN}"
        ))
        .header("content-type", "application/json")
        .body(fixture("radarr_test.json"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);

    let events = webhook_events::recent(&pool, 10).await.unwrap();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].event_type, "Test");
    assert!(events[0].triggered_search_id.is_none());
    assert_eq!(
        search_row_count(&pool).await,
        0,
        "Test event must NOT create a search"
    );
}

#[tokio::test]
async fn radarr_movie_added_returns_202_and_triggers_search() {
    let pool = db::open_memory().await.unwrap();
    let arr_id = make_arr(&pool, "radarr-main", ArrKind::Radarr).await;
    let addr = boot(
        pool.clone(),
        AuthConfig::from_optional(Some(TOKEN)),
        BypassConfig::default(),
    )
    .await;
    let client = reqwest::Client::new();
    let resp = client
        .post(format!(
            "http://{addr}/webhooks/radarr/{arr_id}?apikey={TOKEN}"
        ))
        .header("content-type", "application/json")
        .body(fixture("radarr_movie_added.json"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 202);

    let n = wait_for_search_row(&pool).await;
    assert_eq!(n, 1, "MovieAdded should trigger exactly one search");

    let events = webhook_events::recent(&pool, 10).await.unwrap();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].event_type, "MovieAdded");
    // Wait a touch longer for the back-fill to land.
    for _ in 0..20 {
        let evs = webhook_events::recent(&pool, 10).await.unwrap();
        if evs[0].triggered_search_id.is_some() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("triggered_search_id was never back-filled");
}

#[tokio::test]
async fn sonarr_episode_added_returns_202_and_triggers_search() {
    let pool = db::open_memory().await.unwrap();
    let arr_id = make_arr(&pool, "sonarr-main", ArrKind::Sonarr).await;
    let addr = boot(
        pool.clone(),
        AuthConfig::from_optional(Some(TOKEN)),
        BypassConfig::default(),
    )
    .await;
    let client = reqwest::Client::new();
    let resp = client
        .post(format!(
            "http://{addr}/webhooks/sonarr/{arr_id}?apikey={TOKEN}"
        ))
        .header("content-type", "application/json")
        .body(fixture("sonarr_episode_added.json"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 202);

    let n = wait_for_search_row(&pool).await;
    assert_eq!(
        n, 1,
        "EpisodeAdded with one episode should trigger one search"
    );
}

#[tokio::test]
async fn sonarr_series_add_returns_202_and_triggers_search() {
    // Sonarr's real eventType is `SeriesAdd` (no "ed"). Regression for
    // the matcher that only knew `SeriesAdded` — events were audited but
    // never searched.
    let pool = db::open_memory().await.unwrap();
    let arr_id = make_arr(&pool, "sonarr-main", ArrKind::Sonarr).await;
    let addr = boot(
        pool.clone(),
        AuthConfig::from_optional(Some(TOKEN)),
        BypassConfig::default(),
    )
    .await;
    let client = reqwest::Client::new();
    let resp = client
        .post(format!(
            "http://{addr}/webhooks/sonarr/{arr_id}?apikey={TOKEN}"
        ))
        .header("content-type", "application/json")
        .body(fixture("sonarr_series_add.json"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 202);

    let n = wait_for_search_row(&pool).await;
    assert_eq!(n, 1, "SeriesAdd should trigger one series-wide search");
}

#[tokio::test]
async fn kind_mismatch_returns_400() {
    let pool = db::open_memory().await.unwrap();
    // Insert Sonarr instance, then POST a Radarr payload to /webhooks/radarr/{sonarr_id}
    let sonarr_id = make_arr(&pool, "sonarr-main", ArrKind::Sonarr).await;
    let addr = boot(
        pool,
        AuthConfig::from_optional(Some(TOKEN)),
        BypassConfig::default(),
    )
    .await;
    let client = reqwest::Client::new();
    let resp = client
        .post(format!(
            "http://{addr}/webhooks/radarr/{sonarr_id}?apikey={TOKEN}"
        ))
        .body(fixture("radarr_test.json"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);
}

#[tokio::test]
async fn missing_apikey_returns_401() {
    let pool = db::open_memory().await.unwrap();
    let arr_id = make_arr(&pool, "radarr-main", ArrKind::Radarr).await;
    let addr = boot(
        pool,
        AuthConfig::from_optional(Some(TOKEN)),
        BypassConfig::default(),
    )
    .await;
    let client = reqwest::Client::new();
    let resp = client
        .post(format!("http://{addr}/webhooks/radarr/{arr_id}"))
        .body(fixture("radarr_test.json"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 401);
}

#[tokio::test]
async fn trusted_peer_bypass_lets_request_through_without_apikey() {
    let pool = db::open_memory().await.unwrap();
    let arr_id = make_arr(&pool, "radarr-main", ArrKind::Radarr).await;
    let bypass = BypassConfig {
        peers: TrustedPeers::parse("loopback").unwrap(),
        proxies: TrustedPeers::default(),
    };
    let addr = boot(pool, AuthConfig::from_optional(Some(TOKEN)), bypass).await;
    let client = reqwest::Client::new();
    let resp = client
        .post(format!("http://{addr}/webhooks/radarr/{arr_id}"))
        .body(fixture("radarr_test.json"))
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        200,
        "loopback peer should bypass apikey check"
    );
}

#[tokio::test]
async fn unknown_arr_id_returns_404() {
    let pool = db::open_memory().await.unwrap();
    let addr = boot(
        pool,
        AuthConfig::from_optional(Some(TOKEN)),
        BypassConfig::default(),
    )
    .await;
    let client = reqwest::Client::new();
    let bogus_id = Uuid::new_v4();
    let resp = client
        .post(format!(
            "http://{addr}/webhooks/radarr/{bogus_id}?apikey={TOKEN}"
        ))
        .body(fixture("radarr_test.json"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 404);
}
