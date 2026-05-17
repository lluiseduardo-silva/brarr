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
    assert!(body.contains("Releases history"));
    assert!(body.contains("Sem decisões ainda"));
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
async fn profile_editor_renders_for_preset() {
    let (addr, state) = spawn().await;
    let presets = db::quality_profiles::list_all(state.pool()).await.unwrap();
    let preset = presets.iter().find(|p| p.is_preset).expect("seeded preset");
    let client = reqwest::Client::new();
    let resp = client
        .get(format!("http://{addr}/profiles/{}/edit", preset.id))
        .send()
        .await
        .expect("send");
    assert_eq!(resp.status(), 200);
    let body = resp.text().await.unwrap();
    assert!(body.contains("Editar profile"));
    assert!(
        body.contains("rules_json"),
        "editor should include the rules textarea"
    );
    assert!(
        body.contains(&preset.name),
        "editor should pre-fill the profile name"
    );
}

#[tokio::test]
async fn profile_update_persists_new_rule_list() {
    let (addr, state) = spawn().await;
    let row = db::quality_profiles::insert(
        state.pool(),
        db::quality_profiles::NewQualityProfile {
            name: "edit-roundtrip",
            description: None,
            push_threshold: 100,
        },
    )
    .await
    .unwrap();
    let new_rules = r#"{"rule":[{"name":"only-pt","when":{"audio":"pt-br"},"add_score":42,"tag":null,"reject":false}]}"#;
    let form = [
        ("name", "edit-roundtrip"),
        ("description", ""),
        ("push_threshold", "200"),
        ("rules_json", new_rules),
    ];
    let client = reqwest::Client::new();
    let resp = client
        .put(format!("http://{addr}/profiles/{}", row.id))
        .form(&form)
        .send()
        .await
        .expect("send");
    assert_eq!(resp.status(), 200);
    assert!(
        resp.headers().get("hx-redirect").is_some(),
        "successful PUT must emit HX-Redirect so HTMX reloads /profiles"
    );

    let reread = db::quality_profiles::get_by_id(state.pool(), row.id)
        .await
        .unwrap();
    assert_eq!(reread.push_threshold, 200);
    assert_eq!(reread.rules.rules.len(), 1);
    assert_eq!(reread.rules.rules[0].add_score, 42);
}

#[tokio::test]
async fn profile_update_with_bad_json_returns_editor_with_error_banner() {
    let (addr, state) = spawn().await;
    let row = db::quality_profiles::insert(
        state.pool(),
        db::quality_profiles::NewQualityProfile {
            name: "bad-json",
            description: None,
            push_threshold: 100,
        },
    )
    .await
    .unwrap();
    let form = [
        ("name", "bad-json"),
        ("description", ""),
        ("push_threshold", "100"),
        ("rules_json", "{ this is not valid json"),
    ];
    let client = reqwest::Client::new();
    let resp = client
        .put(format!("http://{addr}/profiles/{}", row.id))
        .form(&form)
        .send()
        .await
        .expect("send");
    assert_eq!(resp.status(), 200);
    let body = resp.text().await.unwrap();
    assert!(
        body.contains("JSON inválido"),
        "editor should re-render with parse error banner, body = {body}"
    );
    // Untouched DB row.
    let reread = db::quality_profiles::get_by_id(state.pool(), row.id)
        .await
        .unwrap();
    assert!(reread.rules.rules.is_empty());
}

#[tokio::test]
async fn profile_preview_evaluates_fixtures_against_form_rules() {
    let (addr, state) = spawn().await;
    let row = db::quality_profiles::insert(
        state.pool(),
        db::quality_profiles::NewQualityProfile {
            name: "preview-target",
            description: None,
            push_threshold: 100,
        },
    )
    .await
    .unwrap();
    // Rule list that gives PT-BR audio a huge bump so the verdict for
    // the bread-and-butter fixture clearly reads "kept".
    let rules = r#"{"rule":[{"name":"PT-BR jackpot","when":{"audio":"pt-br"},"add_score":500,"tag":null,"reject":false}]}"#;
    let form = [("rules_json", rules)];
    let client = reqwest::Client::new();
    let resp = client
        .post(format!("http://{addr}/profiles/{}/preview", row.id))
        .form(&form)
        .send()
        .await
        .expect("send");
    assert_eq!(resp.status(), 200);
    let body = resp.text().await.unwrap();
    // All three fixture labels must show up in the breakdown HTML.
    assert!(body.contains("PT-BR Dub"));
    assert!(body.contains("Anime JP"));
    assert!(body.contains("EN-only"));
    // Custom rule fired on the PT-BR fixture and bumped score above the
    // 150 "kept" threshold the preview uses for badge colour.
    assert!(body.contains("PT-BR jackpot"));
}

#[tokio::test]
async fn profile_preview_with_bad_json_returns_error_message() {
    let (addr, state) = spawn().await;
    let row = db::quality_profiles::insert(
        state.pool(),
        db::quality_profiles::NewQualityProfile {
            name: "preview-bad-json",
            description: None,
            push_threshold: 100,
        },
    )
    .await
    .unwrap();
    let form = [("rules_json", "not-json")];
    let client = reqwest::Client::new();
    let resp = client
        .post(format!("http://{addr}/profiles/{}/preview", row.id))
        .form(&form)
        .send()
        .await
        .expect("send");
    assert_eq!(resp.status(), 200);
    let body = resp.text().await.unwrap();
    assert!(body.contains("JSON inválido"));
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
    assert!(body.contains("Push history"));
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
            audio_languages: Vec::new(),
            subtitle_languages: Vec::new(),
            profile_scores: std::collections::HashMap::new(),
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
            profile_id: None,
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
