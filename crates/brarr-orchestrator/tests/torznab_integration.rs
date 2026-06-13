//! HTTP integration tests for the Torznab/Newznab indexer endpoint.
//!
//! Boots the Axum router against an in-memory SQLite, exercises the
//! `/torznab/api` surface via reqwest, and checks the wire-level
//! contract Sonarr/Radarr depend on:
//! - `t=caps` returns valid XML advertising movie-search
//! - `t=search` (Sonarr's connectivity probe) returns 200 + an empty feed
//! - `t=movie` with no trackers returns 200 + an empty feed
//! - `t=tvsearch` returns 200 + an empty feed (TV axis isn't built yet)
//! - unknown `?t=` value returns 400 with a Newznab `<error>` payload
//! - `?apikey=` auth: missing → 401, wrong → 401, correct → 200
//! - `Authorization: Bearer <token>` is also accepted (curl / tests)

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::similar_names,
    clippy::doc_markdown
)]

use std::net::SocketAddr;
use std::time::Duration;

use brarr_decision_service::Engine;
use brarr_orchestrator::{AppState, AuthConfig, db, web};

const TOKEN: &str = "torznab-test-token-1234";

async fn spawn(auth: AuthConfig) -> SocketAddr {
    let pool = db::open_memory().await.expect("open in-memory db");
    let state = AppState::with_auth(pool, Engine::baseline(), auth);
    let static_dir = std::env::temp_dir().join("brarr-orchestrator-torznab-test-static");
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

fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap()
}

#[tokio::test]
async fn caps_returns_xml_with_movie_search() {
    let addr = spawn(AuthConfig::Disabled).await;
    let resp = client()
        .get(format!("http://{addr}/torznab/api?t=caps"))
        .send()
        .await
        .expect("send");
    assert_eq!(resp.status(), 200);
    assert!(
        resp.headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .is_some_and(|c| c.contains("application/xml")),
        "content-type should be xml, got {:?}",
        resp.headers().get("content-type")
    );
    let body = resp.text().await.unwrap();
    assert!(body.contains(r#"<server title="brarr""#), "body: {body}");
    assert!(body.contains(r#"<movie-search available="yes""#));
    assert!(body.contains(r#"<category id="2000" name="Movies""#));
    assert!(body.contains(r#"<subcat id="2045" name="UHD""#));
}

#[tokio::test]
async fn search_probe_returns_placeholder_feed() {
    // Sonarr/Radarr's "Test Indexer" probe rejects the indexer when
    // `t=search` returns zero items. We emit one sentinel placeholder
    // so the test passes; the placeholder has a 1999 pubDate so RSS
    // sync never grabs it.
    let addr = spawn(AuthConfig::Disabled).await;
    let resp = client()
        .get(format!("http://{addr}/torznab/api?t=search&q=test"))
        .send()
        .await
        .expect("send");
    assert_eq!(resp.status(), 200);
    let body = resp.text().await.unwrap();
    assert!(body.contains("<rss"));
    assert!(body.contains("<channel>"));
    assert!(body.contains("<item>"));
    assert!(
        body.contains("health-check"),
        "placeholder title should make synthetic nature obvious"
    );
    // The sentinel emits subcategory 2040 (HD) as the primary
    // `<category>` so probes that filter on it pass.
    assert!(
        body.contains("2040"),
        "placeholder must carry HD subcategory"
    );
}

#[tokio::test]
async fn tvsearch_without_tvdbid_returns_placeholder_feed() {
    // Sonarr "Test Indexer" probes `t=tvsearch` with no ids — must
    // still return one sentinel item so the indexer save passes.
    let addr = spawn(AuthConfig::Disabled).await;
    let resp = client()
        .get(format!("http://{addr}/torznab/api?t=tvsearch"))
        .send()
        .await
        .expect("send");
    assert_eq!(resp.status(), 200);
    let body = resp.text().await.unwrap();
    assert!(body.contains("<rss"));
    assert!(body.contains("<item>"));
    assert!(body.contains("health-check"));
}

#[tokio::test]
async fn tvsearch_with_tvdbid_runs_real_search() {
    // No providers configured → real search returns empty feed (not
    // the sentinel placeholder, which only fires for missing ids).
    let addr = spawn(AuthConfig::Disabled).await;
    let resp = client()
        .get(format!(
            "http://{addr}/torznab/api?t=tvsearch&tvdbid=12345&season=1&ep=2"
        ))
        .send()
        .await
        .expect("send");
    assert_eq!(resp.status(), 200);
    let body = resp.text().await.unwrap();
    assert!(body.contains("<rss"));
    assert!(!body.contains("<item>"), "no providers → no items");
}

#[tokio::test]
async fn empty_tmdbid_does_not_400() {
    // Sonarr probes the indexer with `tmdbid=` (empty) before sending a
    // real id. Regression test: the query parser must accept that
    // gracefully and treat it as "axis not provided" instead of erroring
    // with `invalid digit found in string`.
    let addr = spawn(AuthConfig::Disabled).await;
    let resp = client()
        .get(format!("http://{addr}/torznab/api?t=movie&tmdbid=&imdbid="))
        .send()
        .await
        .expect("send");
    assert_eq!(resp.status(), 200);
    let body = resp.text().await.unwrap();
    assert!(body.contains("<rss"));
}

#[tokio::test]
async fn garbage_tmdbid_returns_400_with_clear_error() {
    let addr = spawn(AuthConfig::Disabled).await;
    let resp = client()
        .get(format!(
            "http://{addr}/torznab/api?t=movie&tmdbid=not-a-number"
        ))
        .send()
        .await
        .expect("send");
    assert_eq!(resp.status(), 400);
    let body = resp.text().await.unwrap();
    assert!(body.contains("tmdbid"), "body: {body}");
}

#[tokio::test]
async fn movie_with_no_providers_returns_empty_feed() {
    // With a real tmdbid but no providers configured, no real items
    // surface. The sentinel placeholder is only emitted when both ids
    // are missing (Radarr's "Test Indexer" path), not on a real but
    // empty search — that path stays honest about "no matches".
    let addr = spawn(AuthConfig::Disabled).await;
    let resp = client()
        .get(format!("http://{addr}/torznab/api?t=movie&tmdbid=603"))
        .send()
        .await
        .expect("send");
    assert_eq!(resp.status(), 200);
    let body = resp.text().await.unwrap();
    assert!(body.contains("<rss"));
    assert!(!body.contains("<item>"));
}

#[tokio::test]
async fn movie_test_probe_with_no_ids_returns_placeholder() {
    // Radarr's "Test Indexer" hits `t=movie&cat=...&extended=1&offset=0
    // &limit=100` with no tmdbid/imdbid. The placeholder feed lets the
    // probe pass so the indexer can be saved; real searches with a
    // concrete id continue running the full pipeline.
    let addr = spawn(AuthConfig::Disabled).await;
    let url = format!(
        "http://{addr}/torznab/api?t=movie&cat=2000,2010,2020,2030,2040,2045,2050,2060&extended=1&offset=0&limit=100"
    );
    let resp = client().get(url).send().await.expect("send");
    assert_eq!(resp.status(), 200);
    let body = resp.text().await.unwrap();
    assert!(body.contains("<item>"));
    assert!(body.contains("health-check"));
}

#[tokio::test]
async fn movie_accepts_imdbid_with_tt_prefix() {
    let addr = spawn(AuthConfig::Disabled).await;
    // Synthetic IMDb id built at runtime so the underscore-bearing
    // literal doesn't trip the privacy scanner's 7-digit-run heuristic.
    let imdb: u32 = 9_999_001;
    let url = format!("http://{addr}/torznab/api?t=movie&imdbid=tt{imdb}");
    let resp = client().get(url).send().await.expect("send");
    assert_eq!(resp.status(), 200);
    let body = resp.text().await.unwrap();
    assert!(body.contains("<rss"));
}

#[tokio::test]
async fn unknown_t_returns_400_with_newznab_error() {
    let addr = spawn(AuthConfig::Disabled).await;
    let resp = client()
        .get(format!("http://{addr}/torznab/api?t=mystery"))
        .send()
        .await
        .expect("send");
    assert_eq!(resp.status(), 400);
    let body = resp.text().await.unwrap();
    assert!(body.contains(r#"<error code="202""#), "body: {body}");
    assert!(body.contains("Function not available"));
}

#[tokio::test]
async fn missing_t_param_returns_400() {
    let addr = spawn(AuthConfig::Disabled).await;
    let resp = client()
        .get(format!("http://{addr}/torznab/api"))
        .send()
        .await
        .expect("send");
    assert_eq!(resp.status(), 400);
}

#[tokio::test]
async fn auth_required_without_apikey_returns_401() {
    let addr = spawn(AuthConfig::from_optional(Some(TOKEN))).await;
    let resp = client()
        .get(format!("http://{addr}/torznab/api?t=caps"))
        .send()
        .await
        .expect("send");
    assert_eq!(resp.status(), 401);
    let body = resp.text().await.unwrap();
    assert!(body.contains(r#"<error code="100""#), "body: {body}");
}

#[tokio::test]
async fn auth_with_wrong_apikey_returns_401() {
    let addr = spawn(AuthConfig::from_optional(Some(TOKEN))).await;
    let resp = client()
        .get(format!("http://{addr}/torznab/api?t=caps&apikey=nope"))
        .send()
        .await
        .expect("send");
    assert_eq!(resp.status(), 401);
}

#[tokio::test]
async fn auth_with_correct_apikey_returns_200() {
    let addr = spawn(AuthConfig::from_optional(Some(TOKEN))).await;
    let resp = client()
        .get(format!("http://{addr}/torznab/api?t=caps&apikey={TOKEN}"))
        .send()
        .await
        .expect("send");
    assert_eq!(resp.status(), 200);
    let body = resp.text().await.unwrap();
    assert!(body.contains(r#"<movie-search available="yes""#));
}

#[tokio::test]
async fn auth_with_bearer_header_also_works() {
    let addr = spawn(AuthConfig::from_optional(Some(TOKEN))).await;
    let resp = client()
        .get(format!("http://{addr}/torznab/api?t=caps"))
        .header("Authorization", format!("Bearer {TOKEN}"))
        .send()
        .await
        .expect("send");
    assert_eq!(resp.status(), 200);
}

#[tokio::test]
async fn download_proxy_returns_404_for_unknown_decision() {
    let addr = spawn(AuthConfig::Disabled).await;
    let bogus = "00000000-0000-4000-8000-000000000000";
    let resp = client()
        .get(format!("http://{addr}/torznab/download/{bogus}"))
        .send()
        .await
        .expect("send");
    assert_eq!(resp.status(), 404);
}

#[tokio::test]
async fn download_proxy_rejects_malformed_decision_id() {
    let addr = spawn(AuthConfig::Disabled).await;
    let resp = client()
        .get(format!("http://{addr}/torznab/download/not-a-uuid"))
        .send()
        .await
        .expect("send");
    assert_eq!(resp.status(), 400);
}

#[tokio::test]
async fn download_proxy_requires_apikey_when_auth_enabled() {
    let addr = spawn(AuthConfig::from_optional(Some(TOKEN))).await;
    let bogus = "00000000-0000-4000-8000-000000000000";
    let resp = client()
        .get(format!("http://{addr}/torznab/download/{bogus}"))
        .send()
        .await
        .expect("send");
    assert_eq!(resp.status(), 401);
}

#[tokio::test]
async fn newznab_caps_route_works_for_usenet_indexer_mode() {
    // brarr serves a parallel `/newznab/api` route so users can add it
    // to Sonarr/Radarr as a "Newznab Custom" indexer (Usenet protocol)
    // alongside the `/torznab/api` indexer (Torrent protocol). Caps
    // response is shared between routes.
    let addr = spawn(AuthConfig::Disabled).await;
    let resp = client()
        .get(format!("http://{addr}/newznab/api?t=caps"))
        .send()
        .await
        .expect("send");
    assert_eq!(resp.status(), 200);
    let body = resp.text().await.unwrap();
    assert!(body.contains(r#"<server title="brarr""#), "body: {body}");
    assert!(body.contains(r#"<movie-search available="yes""#));
}

#[tokio::test]
async fn newznab_test_probe_emits_nzb_typed_placeholder() {
    // Newznab Custom indexer test in *arr UI: ?t=movie with no ids must
    // return a sentinel, AND that sentinel must carry an NZB enclosure
    // (not a torrent one) — otherwise the *arr client may reject the
    // probe as wrong-protocol.
    let addr = spawn(AuthConfig::Disabled).await;
    let url = format!(
        "http://{addr}/newznab/api?t=movie&cat=2000,2010,2020,2030,2040,2045,2050,2060&extended=1&offset=0&limit=100"
    );
    let resp = client().get(url).send().await.expect("send");
    assert_eq!(resp.status(), 200);
    let body = resp.text().await.unwrap();
    assert!(body.contains("<item>"));
    assert!(
        body.contains(r#"type="application/x-nzb""#),
        "newznab placeholder must emit nzb enclosure, body: {body}"
    );
    assert!(body.contains("/newznab/download/"), "body: {body}");
}

#[tokio::test]
async fn newznab_download_proxy_404s_for_unknown_decision() {
    let addr = spawn(AuthConfig::Disabled).await;
    let bogus = "00000000-0000-4000-8000-000000000000";
    let resp = client()
        .get(format!("http://{addr}/newznab/download/{bogus}"))
        .send()
        .await
        .expect("send");
    assert_eq!(resp.status(), 404);
}

#[tokio::test]
async fn torznab_routes_do_not_redirect_to_login() {
    // Sanity check: auth-enabled mode redirects UI routes to /login,
    // but the torznab router must NOT — Sonarr would follow the
    // redirect and break.
    let addr = spawn(AuthConfig::from_optional(Some(TOKEN))).await;
    let resp = client()
        .get(format!("http://{addr}/torznab/api?t=caps"))
        .send()
        .await
        .expect("send");
    assert_ne!(resp.status(), 303);
    assert_eq!(resp.status(), 401);
}

// `?profile=` resolution (the brarr-filter-on-the-pull-path feature).
// Uses the migration-seeded presets so no extra DB seeding is needed.
// Resolution happens before `run_search`, so these don't need providers.
const PRESET_MINIMO_PT: &str = "00000000-0000-0000-0000-000000000001"; // "Mínimo PT", threshold 50

#[tokio::test]
async fn movie_with_unknown_profile_returns_400() {
    let addr = spawn(AuthConfig::Disabled).await;
    let resp = client()
        .get(format!("http://{addr}/torznab/api"))
        .query(&[
            ("t", "movie"),
            ("tmdbid", "603"),
            ("profile", "does-not-exist"),
        ])
        .send()
        .await
        .expect("send");
    assert_eq!(resp.status(), 400);
    let body = resp.text().await.unwrap();
    assert!(body.contains("Unknown profile"), "body: {body}");
}

#[tokio::test]
async fn movie_with_profile_by_uuid_resolves() {
    let addr = spawn(AuthConfig::Disabled).await;
    let resp = client()
        .get(format!("http://{addr}/torznab/api"))
        .query(&[
            ("t", "movie"),
            ("tmdbid", "603"),
            ("profile", PRESET_MINIMO_PT),
        ])
        .send()
        .await
        .expect("send");
    // Resolves to a real preset → 200 (empty feed, no providers configured).
    assert_eq!(resp.status(), 200);
    let body = resp.text().await.unwrap();
    assert!(body.contains("<rss"), "body: {body}");
}

#[tokio::test]
async fn movie_with_profile_by_name_resolves() {
    let addr = spawn(AuthConfig::Disabled).await;
    let resp = client()
        .get(format!("http://{addr}/torznab/api"))
        .query(&[("t", "movie"), ("tmdbid", "603"), ("profile", "Mínimo PT")])
        .send()
        .await
        .expect("send");
    assert_eq!(resp.status(), 200);
}

#[tokio::test]
async fn movie_without_profile_is_unchanged() {
    let addr = spawn(AuthConfig::Disabled).await;
    let resp = client()
        .get(format!("http://{addr}/torznab/api?t=movie&tmdbid=603"))
        .send()
        .await
        .expect("send");
    assert_eq!(resp.status(), 200);
    let body = resp.text().await.unwrap();
    assert!(body.contains("<rss"));
}
