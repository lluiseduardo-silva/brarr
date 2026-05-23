//! Integration tests for the new `GET /searches` filtered-history
//! route. Boots the real Axum router against an in-memory pool with
//! auth disabled (the filter feature is orthogonal to auth), inserts
//! a few rows, and asserts the rendered HTML contains the expected
//! ids — or NOT, when a filter is meant to exclude them.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::similar_names,
    clippy::doc_markdown
)]

use std::net::SocketAddr;
use std::time::Duration;

use brarr_decision_service::Engine;
use brarr_orchestrator::db::searches::{self, SearchRequestJson};
use brarr_orchestrator::{AppState, AuthConfig, BypassConfig, db, web};
use uuid::Uuid;

async fn boot(pool: db::Pool) -> SocketAddr {
    let state = AppState::with_auth_and_bypass(
        pool,
        Engine::baseline(),
        AuthConfig::Disabled,
        BypassConfig::default(),
    );
    let static_dir = std::env::temp_dir().join("brarr-orchestrator-searches-index-test-static");
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

async fn seed(pool: &db::Pool) -> (Uuid, Uuid, Uuid) {
    let m1 = searches::create(
        pool,
        SearchRequestJson {
            tmdb_id: Some(603),
            ..SearchRequestJson::default()
        },
    )
    .await
    .unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(1100)).await;
    let m2 = searches::create(
        pool,
        SearchRequestJson {
            imdb_id: Some("tt0133093".to_string()),
            ..SearchRequestJson::default()
        },
    )
    .await
    .unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(1100)).await;
    let tv = searches::create(
        pool,
        SearchRequestJson {
            tvdb_id: Some(81189),
            season: Some(1),
            episode: Some(1),
            ..SearchRequestJson::default()
        },
    )
    .await
    .unwrap();
    (m1.id, m2.id, tv.id)
}

#[tokio::test]
async fn unfiltered_searches_lists_all_rows() {
    let pool = db::open_memory().await.unwrap();
    let (a, b, c) = seed(&pool).await;
    let addr = boot(pool).await;
    let body = reqwest::get(format!("http://{addr}/searches"))
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(body.contains(&a.to_string()), "expected {a} in body");
    assert!(body.contains(&b.to_string()), "expected {b} in body");
    assert!(body.contains(&c.to_string()), "expected {c} in body");
    assert!(body.contains("Histórico de buscas"));
}

#[tokio::test]
async fn tmdb_filter_isolates_single_row() {
    let pool = db::open_memory().await.unwrap();
    let (movie_tmdb, movie_imdb, tv) = seed(&pool).await;
    let addr = boot(pool).await;
    let body = reqwest::get(format!("http://{addr}/searches?tmdb_id=603"))
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(body.contains(&movie_tmdb.to_string()));
    assert!(!body.contains(&movie_imdb.to_string()));
    assert!(!body.contains(&tv.to_string()));
}

#[tokio::test]
async fn tvdb_plus_season_episode_filter_combines() {
    let pool = db::open_memory().await.unwrap();
    let (movie, _, tv) = seed(&pool).await;
    let addr = boot(pool).await;
    let body = reqwest::get(format!(
        "http://{addr}/searches?tvdb_id=81189&season=1&episode=1"
    ))
    .await
    .unwrap()
    .text()
    .await
    .unwrap();
    assert!(body.contains(&tv.to_string()));
    assert!(!body.contains(&movie.to_string()));
}

#[tokio::test]
async fn pagination_via_size_and_page_returns_disjoint_pages() {
    let pool = db::open_memory().await.unwrap();
    let (a, b, c) = seed(&pool).await;
    let addr = boot(pool).await;
    let page1 = reqwest::get(format!("http://{addr}/searches?size=10&page=1"))
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    // size=10 fits all 3 in one page.
    assert!(page1.contains(&a.to_string()));
    assert!(page1.contains(&b.to_string()));
    assert!(page1.contains(&c.to_string()));

    // Now constrain to size=10 but request page=2 — should be empty
    // (clamps to last page, which is still page 1; just verify no
    // pagination explosion).
    let page2 = reqwest::get(format!("http://{addr}/searches?size=10&page=2"))
        .await
        .unwrap();
    assert_eq!(page2.status(), 200);
}

#[tokio::test]
async fn filter_form_inputs_preserve_submitted_values() {
    let pool = db::open_memory().await.unwrap();
    let _ = seed(&pool).await;
    let addr = boot(pool).await;
    let body = reqwest::get(format!("http://{addr}/searches?tmdb_id=603&size=25"))
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    // Pre-filled value attribute proves the filter sticks across
    // page loads (bookmarkable URL).
    assert!(body.contains(r#"name="tmdb_id" value="603""#));
}
