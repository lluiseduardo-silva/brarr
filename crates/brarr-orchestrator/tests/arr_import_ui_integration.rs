//! Integration tests for the \*arr import screen.
//!
//! The preview exists to answer one question before anything is written:
//! **does the root mapping actually reach this operator's disk?** So the
//! tests run a fake Sonarr against a real temporary directory and assert
//! the count that answers it — first with no mapping, then with one.
//!
//! Nothing here touches TMDB. The commit needs a credential and would be
//! a different test; the preview deliberately does not, which is what
//! makes it safe to open against a production instance.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::similar_names,
    clippy::doc_markdown
)]

use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use brarr_arr::ArrKind;
use brarr_decision_service::Engine;
use brarr_orchestrator::db::{arr_instances, arr_root_mappings, root_folders};
use brarr_orchestrator::{AppState, db, web};
use uuid::Uuid;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

async fn spawn() -> (SocketAddr, AppState) {
    let pool = db::open_memory().await.expect("open in-memory db");
    let state = AppState::new(pool, Engine::baseline());
    let static_dir = std::env::temp_dir().join("brarr-orchestrator-arr-import-static");
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

/// A directory that exists for the duration of one test.
fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("brarr-arrui-{name}-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// A Sonarr with two series under `/data/Series` — the operator's own
/// namespace, which is not a path that exists on this machine.
async fn fake_sonarr() -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v3/series"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
            {
                "id": 1,
                "title": "The Boys",
                "year": 2019,
                "tvdbId": 355_567,
                "tmdbId": 76_479,
                "imdbId": "tt1190634",
                "monitored": true,
                "path": "/data/Series/The Boys",
                "rootFolderPath": "/data/Series",
                "seasons": [{ "seasonNumber": 1, "monitored": true }]
            },
            {
                "id": 2,
                "title": "Sem Vínculo",
                "year": 2020,
                "tvdbId": 0,
                "tmdbId": 0,
                "imdbId": "",
                "monitored": false,
                "path": "/data/Series/Sem Vinculo",
                "rootFolderPath": "/data/Series",
                "seasons": []
            }
        ])))
        .mount(&server)
        .await;
    server
}

async fn make_arr(state: &AppState, base: &str) -> Uuid {
    let url = url::Url::parse(base).unwrap();
    arr_instances::insert(
        state.pool(),
        arr_instances::NewArrInstance {
            name: "sonarr-series",
            kind: ArrKind::Sonarr,
            base_url: &url,
            api_key: "fake",
            push_threshold: None,
            profile_id: None,
            // Disabled for the deprecated push path, which is the state
            // all three of the operator's instances are in.
            enabled: Some(false),
        },
    )
    .await
    .unwrap()
    .id
}

/// Without a mapping the preview must say so plainly: the root is
/// unmapped and not one folder was found. That is the whole point of
/// looking before importing — the alternative is discovering it seven
/// thousand records later.
#[tokio::test]
async fn an_unmapped_root_reports_zero_folders_found() {
    let (addr, state) = spawn().await;
    let sonarr = fake_sonarr().await;
    let arr_id = make_arr(&state, &sonarr.uri()).await;

    let body = reqwest::get(format!("http://{addr}/arr-instances/{arr_id}/import"))
        .await
        .unwrap()
        .text()
        .await
        .unwrap();

    assert!(body.contains("sem mapeamento"), "{body}");
    assert!(
        body.contains("nenhuma</strong> pasta"),
        "the zero-folders warning must fire"
    );
    // The title with no TMDB id is blocked, and says why.
    assert!(body.contains("sem id do TMDB no *arr"), "{body}");
}

/// With the mapping in place the same catalogue resolves onto a real
/// directory. This is the production shape: Sonarr answers
/// `/data/Series/…` and brarr mounts the same share elsewhere.
#[tokio::test]
async fn mapping_the_root_makes_the_folders_visible() {
    let (addr, state) = spawn().await;
    let sonarr = fake_sonarr().await;
    let arr_id = make_arr(&state, &sonarr.uri()).await;

    let library = temp_dir("visible");
    std::fs::create_dir_all(library.join("The Boys")).unwrap();
    let root = root_folders::insert(state.pool(), &library.to_string_lossy(), None)
        .await
        .unwrap();

    let client = reqwest::Client::new();
    let body = client
        .post(format!(
            "http://{addr}/arr-instances/{arr_id}/import/mappings"
        ))
        .form(&[
            ("arr_path", "/data/Series".to_owned()),
            ("root_folder_id", root.id.to_string()),
        ])
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();

    assert!(
        !body.contains("sem mapeamento"),
        "the root should be covered now: {body}"
    );
    assert!(
        body.contains(&library.to_string_lossy().to_string()),
        "the local side must be shown"
    );
    // One of the two series has its folder on disk; the other does not.
    assert!(body.contains("vista"), "{body}");
    assert!(
        !body.contains("nenhuma</strong> pasta"),
        "the zero-folders warning must be gone"
    );

    let stored = arr_root_mappings::for_instance(state.pool(), arr_id)
        .await
        .unwrap();
    assert_eq!(stored.len(), 1);
    assert_eq!(stored[0].arr_path, "/data/Series");

    std::fs::remove_dir_all(&library).ok();
}

/// Removing the rule takes the operator back to the unmapped preview,
/// re-rendered for the instance the rule belonged to — not for whatever
/// page issued the delete.
#[tokio::test]
async fn removing_the_mapping_re_renders_its_own_instance() {
    let (addr, state) = spawn().await;
    let sonarr = fake_sonarr().await;
    let arr_id = make_arr(&state, &sonarr.uri()).await;
    let library = temp_dir("remove");
    let root = root_folders::insert(state.pool(), &library.to_string_lossy(), None)
        .await
        .unwrap();
    let mapping = arr_root_mappings::insert(state.pool(), arr_id, "/data/Series", root.id)
        .await
        .unwrap();

    let client = reqwest::Client::new();
    let body = client
        .delete(format!("http://{addr}/arr-root-mappings/{}", mapping.id))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();

    assert!(body.contains("sem mapeamento"), "{body}");
    assert!(
        arr_root_mappings::for_instance(state.pool(), arr_id)
            .await
            .unwrap()
            .is_empty()
    );
    std::fs::remove_dir_all(&library).ok();
}

/// The sync flag is a different axis from `enabled`, and the row has to
/// let the operator set the combination their stack is actually in.
#[tokio::test]
async fn sync_source_toggle_flips_without_enabling_the_push_path() {
    let (addr, state) = spawn().await;
    let arr_id = make_arr(&state, "https://arr.example/").await;

    let client = reqwest::Client::new();
    let body = client
        .post(format!("http://{addr}/arr-instances/{arr_id}/sync-source"))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(body.contains("fonte: sim"), "{body}");

    let row = arr_instances::get_by_id(state.pool(), arr_id)
        .await
        .unwrap();
    assert!(row.sync_source);
    assert!(
        !row.enabled,
        "becoming a source must not switch the deprecated push path back on"
    );
}

#[tokio::test]
async fn the_instance_row_links_to_its_import_screen() {
    let (addr, state) = spawn().await;
    let arr_id = make_arr(&state, "https://arr.example/").await;
    let body = reqwest::get(format!("http://{addr}/arr-instances"))
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(body.contains(&format!("/arr-instances/{arr_id}/import")));
    assert!(body.contains("fonte: não"));
}

#[tokio::test]
async fn an_unreachable_arr_is_a_form_error_not_a_500() {
    let (addr, state) = spawn().await;
    // Port 1 on localhost refuses instantly.
    let arr_id = make_arr(&state, "http://127.0.0.1:1/").await;
    let resp = reqwest::get(format!("http://{addr}/arr-instances/{arr_id}/import"))
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);
    assert!(resp.text().await.unwrap().contains("sonarr-series"));
}

#[tokio::test]
async fn deleting_an_unknown_mapping_is_a_404() {
    let (addr, _state) = spawn().await;
    let resp = reqwest::Client::new()
        .delete(format!(
            "http://{addr}/arr-root-mappings/{}",
            Uuid::new_v4()
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 404);
}
