//! Integration tests for the webhook *discoverability* UI:
//! - the `/webhooks` audit page (wires the previously-orphan
//!   `webhook_events::recent` query),
//! - the per-instance webhook URL on `/arr-instances`,
//! - the "movido a webhook" toggle.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::similar_names,
    clippy::doc_markdown
)]

use std::net::SocketAddr;
use std::time::Duration;

use brarr_arr::ArrKind;
use brarr_decision_service::Engine;
use brarr_orchestrator::db::{arr_instances, webhook_events};
use brarr_orchestrator::{AppState, db, web};
use uuid::Uuid;

async fn spawn() -> (SocketAddr, AppState) {
    let pool = db::open_memory().await.expect("open in-memory db");
    let state = AppState::new(pool, Engine::baseline());
    let static_dir = std::env::temp_dir().join("brarr-orchestrator-webhooks-ui-static");
    let _ = tokio::fs::create_dir_all(&static_dir).await;
    let router = web::router(state.clone(), &static_dir);
    let listener = tokio::net::TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0)))
        .await
        .expect("bind");
    let addr = listener.local_addr().expect("local addr");
    tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });
    tokio::time::sleep(Duration::from_millis(50)).await;
    (addr, state)
}

async fn make_arr(state: &AppState, name: &str, kind: ArrKind) -> Uuid {
    let url = url::Url::parse("https://arr.example/").unwrap();
    arr_instances::insert(
        state.pool(),
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

#[tokio::test]
async fn webhooks_page_lists_received_event() {
    let (addr, state) = spawn().await;
    let arr_id = make_arr(&state, "radarr-main", ArrKind::Radarr).await;
    webhook_events::insert(
        state.pool(),
        webhook_events::NewWebhookEvent {
            arr_instance_id: arr_id,
            kind: ArrKind::Radarr,
            event_type: "MovieAdded",
            payload_json: r#"{"eventType":"MovieAdded"}"#,
        },
    )
    .await
    .unwrap();

    let body = reqwest::get(format!("http://{addr}/webhooks"))
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(body.contains("Webhooks recebidos"));
    assert!(body.contains("radarr-main"), "instance name missing");
    assert!(body.contains("MovieAdded"), "event_type missing");
}

#[tokio::test]
async fn webhooks_page_empty_state() {
    let (addr, _state) = spawn().await;
    let body = reqwest::get(format!("http://{addr}/webhooks"))
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(body.contains("Nenhum webhook recebido ainda"));
}

#[tokio::test]
async fn arr_instances_page_shows_webhook_url() {
    let (addr, state) = spawn().await;
    let arr_id = make_arr(&state, "radarr-main", ArrKind::Radarr).await;
    let body = reqwest::get(format!("http://{addr}/arr-instances"))
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    // Auth is disabled in this harness → URL carries no apikey.
    assert!(
        body.contains(&format!("/webhooks/radarr/{arr_id}")),
        "webhook URL not rendered"
    );
    assert!(body.contains("poll: auto"), "poll toggle missing");
}

#[tokio::test]
async fn webhook_driven_toggle_flips_flag() {
    let (addr, state) = spawn().await;
    let arr_id = make_arr(&state, "sonarr-main", ArrKind::Sonarr).await;
    let client = reqwest::Client::new();
    let body = client
        .post(format!(
            "http://{addr}/arr-instances/{arr_id}/webhook-driven"
        ))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    // Partial reflects the new state...
    assert!(body.contains("poll: webhook"));
    // ...and it actually persisted.
    let row = arr_instances::get_by_id(state.pool(), arr_id)
        .await
        .unwrap();
    assert!(row.webhook_driven);
}
