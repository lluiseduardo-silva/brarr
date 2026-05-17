//! HTTP integration tests for the admin UI.
//!
//! Builds the Axum router against an in-memory SQLite, spawns it on a
//! random local port, and exercises endpoints via real reqwest calls.
//! Catches wiring bugs the unit tests miss: route matching, handler
//! state, template rendering, HTMX form parsing.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::similar_names,
    clippy::doc_markdown
)]

use std::net::SocketAddr;
use std::time::Duration;

use brarr_decision_service::Engine;
use brarr_orchestrator::{AppState, db, web};

async fn spawn() -> (SocketAddr, AppState) {
    let pool = db::open_memory().await.expect("open in-memory db");
    let state = AppState::new(pool, Engine::baseline());
    let static_dir = std::env::temp_dir().join("brarr-orchestrator-test-static");
    let _ = tokio::fs::create_dir_all(&static_dir).await;
    let router = web::router(state.clone(), &static_dir);

    let listener = tokio::net::TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0)))
        .await
        .expect("bind");
    let addr = listener.local_addr().expect("local addr");
    tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });
    // Give the listener a beat to start accepting.
    tokio::time::sleep(Duration::from_millis(50)).await;
    (addr, state)
}

#[tokio::test]
async fn healthz_returns_ok() {
    let (addr, _state) = spawn().await;
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
async fn dashboard_renders_with_zero_state() {
    let (addr, _state) = spawn().await;
    let client = reqwest::Client::new();
    let resp = client
        .get(format!("http://{addr}/"))
        .send()
        .await
        .expect("send");
    assert_eq!(resp.status(), 200);
    let body = resp.text().await.unwrap();
    assert!(body.contains("Dashboard"));
    assert!(body.contains("Providers configurados"));
    assert!(body.contains("Ainda não há buscas"));
}

#[tokio::test]
async fn providers_index_renders_empty_state() {
    let (addr, _state) = spawn().await;
    let client = reqwest::Client::new();
    let resp = client
        .get(format!("http://{addr}/providers"))
        .send()
        .await
        .expect("send");
    assert_eq!(resp.status(), 200);
    let body = resp.text().await.unwrap();
    assert!(body.contains("Adicionar provider"));
    assert!(body.contains("Nenhum provider configurado"));
}

#[tokio::test]
async fn create_then_delete_provider_roundtrip() {
    let (addr, _state) = spawn().await;
    let client = reqwest::Client::new();

    // POST /providers form
    let resp = client
        .post(format!("http://{addr}/providers"))
        .form(&[
            ("name", "capybara"),
            ("base_url", "https://capybarabr.com/"),
            ("api_token", "secret-token"),
            ("kind", "unit3d"),
        ])
        .send()
        .await
        .expect("send");
    assert_eq!(resp.status(), 200);
    let body = resp.text().await.unwrap();
    // Partial returned should contain the newly added provider row.
    assert!(body.contains("capybara"));
    assert!(body.contains("https://capybarabr.com/"));

    // GET /providers should now show it.
    let resp = client
        .get(format!("http://{addr}/providers"))
        .send()
        .await
        .expect("send");
    let body = resp.text().await.unwrap();
    assert!(body.contains("capybara"));

    // Extract provider id from the row's id attribute `provider-<uuid>`.
    let marker = "id=\"provider-";
    let pos = body.find(marker).expect("provider row marker");
    let rest = &body[pos + marker.len()..];
    let end = rest.find('"').expect("closing quote");
    let id = &rest[..end];

    let resp = client
        .delete(format!("http://{addr}/providers/{id}"))
        .send()
        .await
        .expect("send");
    assert_eq!(resp.status(), 200);

    // After delete, list should be empty again.
    let resp = client
        .get(format!("http://{addr}/providers"))
        .send()
        .await
        .expect("send");
    let body = resp.text().await.unwrap();
    assert!(body.contains("Nenhum provider configurado"));
}

#[tokio::test]
async fn invalid_provider_id_returns_400() {
    let (addr, _state) = spawn().await;
    let client = reqwest::Client::new();
    let resp = client
        .delete(format!("http://{addr}/providers/not-a-uuid"))
        .send()
        .await
        .expect("send");
    assert_eq!(resp.status(), 400);
}

#[tokio::test]
async fn delete_unknown_provider_returns_404() {
    let (addr, _state) = spawn().await;
    let client = reqwest::Client::new();
    let resp = client
        .delete(format!(
            "http://{addr}/providers/00000000-0000-4000-8000-000000000000"
        ))
        .send()
        .await
        .expect("send");
    assert_eq!(resp.status(), 404);
}

#[tokio::test]
async fn search_with_no_providers_redirects_to_detail() {
    // Regression: POST /searches must return 200 + HX-Redirect (NOT a
    // 3xx with Location), otherwise the browser auto-follows the
    // Location header on the underlying XHR before HTMX can read the
    // response and trigger a client-side navigation. End result was
    // users sitting on the dashboard forever wondering why the form
    // does nothing.
    let (addr, _state) = spawn().await;
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap();
    let resp = client
        .post(format!("http://{addr}/searches"))
        .form(&[("tmdb_id", "603")])
        .send()
        .await
        .expect("send");
    assert_eq!(resp.status(), 200);
    let hx_redirect = resp.headers().get("HX-Redirect").expect("hx-redirect");
    assert!(hx_redirect.to_str().unwrap().starts_with("/searches/"));
    assert!(
        resp.headers().get("location").is_none(),
        "must NOT set Location — keeps the browser from auto-following the redirect"
    );
}

#[tokio::test]
async fn releases_index_renders_empty_state() {
    let (addr, _state) = spawn().await;
    let client = reqwest::Client::new();
    let resp = client
        .get(format!("http://{addr}/releases"))
        .send()
        .await
        .expect("send");
    assert_eq!(resp.status(), 200);
    let body = resp.text().await.unwrap();
    assert!(body.contains("Histórico de decisões"));
    assert!(body.contains("Ainda não há decisões"));
}

#[tokio::test]
async fn invalid_base_url_in_form_returns_400() {
    let (addr, _state) = spawn().await;
    let client = reqwest::Client::new();
    let resp = client
        .post(format!("http://{addr}/providers"))
        .form(&[
            ("name", "bad"),
            ("base_url", "not a url"),
            ("api_token", "tok"),
        ])
        .send()
        .await
        .expect("send");
    assert_eq!(resp.status(), 400);
}

#[tokio::test]
async fn arr_instances_index_renders_empty_state() {
    let (addr, _state) = spawn().await;
    let client = reqwest::Client::new();
    let resp = client
        .get(format!("http://{addr}/arr-instances"))
        .send()
        .await
        .expect("send");
    assert_eq!(resp.status(), 200);
    let body = resp.text().await.unwrap();
    assert!(body.contains("Adicionar instância"));
    assert!(body.contains("Nenhum *arr cadastrado"));
}

#[tokio::test]
async fn create_then_delete_arr_instance_roundtrip() {
    let (addr, _state) = spawn().await;
    let client = reqwest::Client::new();

    let resp = client
        .post(format!("http://{addr}/arr-instances"))
        .form(&[
            ("name", "radarr-main"),
            ("kind", "radarr"),
            ("base_url", "http://radarr.local:7878/"),
            ("api_key", "test-key"),
            ("push_threshold", "650"),
        ])
        .send()
        .await
        .expect("send");
    assert_eq!(resp.status(), 200);
    let body = resp.text().await.unwrap();
    assert!(body.contains("radarr-main"));
    assert!(body.contains("650"), "threshold should show, body: {body}");

    // GET /arr-instances should now show it.
    let resp = client
        .get(format!("http://{addr}/arr-instances"))
        .send()
        .await
        .expect("send");
    let body = resp.text().await.unwrap();
    assert!(body.contains("radarr-main"));

    // Pull the row id out of `id="arr-instance-<uuid>"`.
    let marker = "id=\"arr-instance-";
    let pos = body.find(marker).expect("arr-instance row marker");
    let rest = &body[pos + marker.len()..];
    let end = rest.find('"').expect("closing quote");
    let id = &rest[..end];

    let resp = client
        .delete(format!("http://{addr}/arr-instances/{id}"))
        .send()
        .await
        .expect("send");
    assert_eq!(resp.status(), 200);

    let resp = client
        .get(format!("http://{addr}/arr-instances"))
        .send()
        .await
        .expect("send");
    let body = resp.text().await.unwrap();
    assert!(body.contains("Nenhum *arr cadastrado"));
}

#[tokio::test]
async fn arr_instance_create_rejects_invalid_kind() {
    let (addr, _state) = spawn().await;
    let client = reqwest::Client::new();
    let resp = client
        .post(format!("http://{addr}/arr-instances"))
        .form(&[
            ("name", "wrong"),
            ("kind", "lidarr"),
            ("base_url", "http://x/"),
            ("api_key", "k"),
        ])
        .send()
        .await
        .expect("send");
    assert_eq!(resp.status(), 400);
}

#[tokio::test]
async fn arr_instance_create_rejects_invalid_base_url() {
    let (addr, _state) = spawn().await;
    let client = reqwest::Client::new();
    let resp = client
        .post(format!("http://{addr}/arr-instances"))
        .form(&[
            ("name", "x"),
            ("kind", "radarr"),
            ("base_url", "not a url"),
            ("api_key", "k"),
        ])
        .send()
        .await
        .expect("send");
    assert_eq!(resp.status(), 400);
}

#[tokio::test]
async fn arr_instance_delete_unknown_returns_404() {
    let (addr, _state) = spawn().await;
    let client = reqwest::Client::new();
    let resp = client
        .delete(format!(
            "http://{addr}/arr-instances/00000000-0000-4000-8000-000000000000"
        ))
        .send()
        .await
        .expect("send");
    assert_eq!(resp.status(), 404);
}

#[tokio::test]
async fn pushes_index_renders_empty_state() {
    let (addr, _state) = spawn().await;
    let client = reqwest::Client::new();
    let resp = client
        .get(format!("http://{addr}/pushes"))
        .send()
        .await
        .expect("send");
    assert_eq!(resp.status(), 200);
    let body = resp.text().await.unwrap();
    assert!(body.contains("Histórico de push"));
    assert!(body.contains("Nenhum push registrado"));
}

#[tokio::test]
async fn decisions_push_records_transport_failure_against_dead_arr() {
    // No live *arr is reachable from the test harness, so the push
    // call necessarily fails — but brarr should still persist a
    // `push_history` row marked transport_error rather than 5xx-ing
    // the request itself. Validates the "always record, never crash"
    // contract.
    use brarr_core::{ReleaseKind, Resolution};
    use brarr_orchestrator::db::{arr_instances, decisions, searches};

    let (addr, state) = spawn().await;
    let pool = state.pool();

    // Set up a decision row + a (fake) arr_instance pointing at a
    // host that will refuse connections.
    let search = searches::create(
        pool,
        searches::SearchRequestJson {
            tmdb_id: Some(603),
            ..searches::SearchRequestJson::default()
        },
    )
    .await
    .unwrap();
    let decision = decisions::insert(
        pool,
        decisions::DecisionInsert {
            search_id: search.id,
            provider_id: None,
            provider_name: "p".into(),
            release_name: "Matrix.1999.1080p-FOO".into(),
            release_id_remote: 1,
            score: 800,
            rejected: false,
            tags: vec![],
            matched_rules: vec![],
            seeders: 1,
            leechers: 0,
            size_bytes: 1,
            resolution: Resolution::P1080,
            kind: ReleaseKind::WebDl,
            download_url: None,
            details_url: None,
            provider_kind: Some("unit3d".into()),
            published_at: None,
        },
    )
    .await
    .unwrap();
    // Pick a host:port that can't possibly accept connections.
    let arr = arr_instances::insert(
        pool,
        arr_instances::NewArrInstance {
            name: "dead",
            kind: brarr_arr::ArrKind::Radarr,
            base_url: &url::Url::parse("http://127.0.0.1:1/").unwrap(),
            api_key: "x",
            push_threshold: None,
            enabled: None,
        },
    )
    .await
    .unwrap();

    let client = reqwest::Client::new();
    let resp = client
        .post(format!(
            "http://{addr}/decisions/{}/push/{}",
            decision.id, arr.id
        ))
        .send()
        .await
        .expect("send");
    assert_eq!(resp.status(), 200);
    let body = resp.text().await.unwrap();
    // Badge for a transport failure ("net" label).
    assert!(
        body.contains("net") || body.contains("http"),
        "badge should reflect failure, body = {body}"
    );

    // History page should now show one row.
    let resp = client
        .get(format!("http://{addr}/pushes"))
        .send()
        .await
        .expect("send");
    let body = resp.text().await.unwrap();
    assert!(
        body.contains("dead"),
        "push history should mention the *arr name, body = {body}"
    );
}
