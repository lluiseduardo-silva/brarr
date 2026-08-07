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
    // Shared empty-state text from `partials/search_row_list.html`
    // (dashboard + /searches now render through the same partial).
    assert!(body.contains("Nenhuma busca encontrada"));
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

/// Pull the uuid out of the first `id="<prefix><uuid>"` in a rendered
/// list, the way the provider round-trip test does inline.
fn first_row_id(body: &str, prefix: &str) -> String {
    let marker = format!("id=\"{prefix}");
    let pos = body.find(&marker).expect("row marker");
    let rest = &body[pos + marker.len()..];
    let end = rest.find('"').expect("closing quote");
    rest[..end].to_owned()
}

#[tokio::test]
async fn interactive_search_reports_when_nothing_is_found() {
    use brarr_orchestrator::db::library::{self, MediaType, NewLibraryItem};

    let (addr, state) = spawn().await;
    let item = library::upsert(
        state.pool(),
        &NewLibraryItem {
            media_type: Some(MediaType::Movie),
            tmdb_id: 603,
            title: "The Matrix".to_owned(),
            ..NewLibraryItem::default()
        },
    )
    .await
    .unwrap();

    let body = reqwest::get(format!("http://{addr}/library/{}/interactive", item.id))
        .await
        .expect("send")
        .text()
        .await
        .unwrap();
    assert!(body.contains("nenhuma release encontrada"), "body = {body}");
    // Even the empty answer is a dialog, so it can be dismissed like any
    // other. It used to be a loose div with no way out.
    assert!(body.contains("interactive-dialog"), "body = {body}");
    assert!(body.contains("<dialog"), "body = {body}");
}

#[tokio::test]
async fn grabbing_a_release_that_lost_its_provider_says_so() {
    use brarr_orchestrator::db::decisions::{self, DecisionInsert};
    use brarr_orchestrator::db::library::{self, MediaType, NewLibraryItem};
    use brarr_orchestrator::db::searches::{self, SearchRequestJson};

    let (addr, state) = spawn().await;
    let item = library::upsert(
        state.pool(),
        &NewLibraryItem {
            media_type: Some(MediaType::Movie),
            tmdb_id: 603,
            title: "The Matrix".to_owned(),
            ..NewLibraryItem::default()
        },
    )
    .await
    .unwrap();
    let search = searches::create(state.pool(), SearchRequestJson::default())
        .await
        .unwrap();
    // `provider_id: None` is what a decision looks like once its
    // provider row is deleted — the barrier has no key without one.
    let decision = decisions::insert(
        state.pool(),
        DecisionInsert {
            search_id: search.id,
            provider_id: None,
            provider_name: "sumido".into(),
            release_name: "Matrix.1999.1080p".into(),
            release_id_remote: 1,
            release_guid: Some("abc".into()),
            score: 500,
            rejected: false,
            tags: vec![],
            matched_rules: vec![],
            seeders: 10,
            leechers: 0,
            size_bytes: 1,
            resolution: brarr_core::Resolution::P1080,
            kind: brarr_core::ReleaseKind::WebDl,
            download_url: Some("https://x/1".into()),
            details_url: None,
            provider_kind: Some("unit3d".into()),
            published_at: None,
            audio_languages: vec![],
            subtitle_languages: vec![],
            profile_scores: std::collections::HashMap::new(),
        },
    )
    .await
    .unwrap();

    let badge = reqwest::Client::new()
        .post(format!(
            "http://{addr}/library/{}/grab/{}",
            item.id, decision.id
        ))
        .send()
        .await
        .expect("send")
        .text()
        .await
        .unwrap();
    assert!(badge.contains("provider removido"), "badge = {badge}");
    assert!(
        brarr_orchestrator::db::grabs::for_item(state.pool(), item.id)
            .await
            .unwrap()
            .is_empty(),
        "nothing may be reserved without a barrier key"
    );
}

#[tokio::test]
async fn queue_renders_empty_state() {
    let (addr, _state) = spawn().await;
    let resp = reqwest::get(format!("http://{addr}/queue"))
        .await
        .expect("send");
    assert_eq!(resp.status(), 200);
    let body = resp.text().await.unwrap();
    assert!(body.contains("Fila vazia"));
}

#[tokio::test]
async fn queue_lists_an_in_flight_grab_without_reaching_a_client() {
    use brarr_orchestrator::db::grabs::{self, NewGrab, Protocol};
    use brarr_orchestrator::db::library::{self, MediaType, NewLibraryItem};
    use brarr_orchestrator::db::providers::{self, NewProvider};

    let (addr, state) = spawn().await;
    let item = library::upsert(
        state.pool(),
        &NewLibraryItem {
            media_type: Some(MediaType::Movie),
            tmdb_id: 603,
            title: "The Matrix".to_owned(),
            ..NewLibraryItem::default()
        },
    )
    .await
    .unwrap();
    let provider = providers::insert(
        state.pool(),
        NewProvider {
            name: "capybara",
            base_url: &url::Url::parse("https://capybarabr.com/").unwrap(),
            api_token: "tok",
            kind: "unit3d",
            plugin_path: None,
        },
    )
    .await
    .unwrap();
    grabs::reserve(
        state.pool(),
        &NewGrab {
            item_id: item.id,
            episode_id: None,
            season_number: None,
            decision_id: None,
            provider_id: provider.id,
            provider_name: "capybara",
            release_id_remote: "abc",
            release_name: "Matrix.1999.1080p.BluRay.PT-BR",
            download_url: None,
            protocol: Protocol::Torrent,
        },
    )
    .await
    .unwrap()
    .unwrap();

    let body = reqwest::get(format!("http://{addr}/queue"))
        .await
        .expect("send")
        .text()
        .await
        .unwrap();
    assert!(body.contains("The Matrix"));
    assert!(body.contains("Matrix.1999.1080p.BluRay.PT-BR"));
    assert!(body.contains("capybara"));
    // The grab never reached a client, so the row falls back to its own
    // state instead of inventing progress.
    assert!(body.contains("reserved"));
    assert!(body.contains("sem cliente associado"));
}

#[tokio::test]
async fn the_queue_page_polls_itself_and_the_fragment_is_the_same_markup() {
    let (addr, _state) = spawn().await;

    let page = reqwest::get(format!("http://{addr}/queue"))
        .await
        .expect("send")
        .text()
        .await
        .unwrap();
    assert!(page.contains(r#"id="queue-live""#));
    assert!(page.contains(r#"hx-get="/queue/live""#));
    assert!(page.contains(r#"hx-swap="outerHTML""#));

    let fragment = reqwest::get(format!("http://{addr}/queue/live"))
        .await
        .expect("send")
        .text()
        .await
        .unwrap();
    // A fragment, not a page: swapping a whole document into the target
    // is how a poll quietly nests the layout inside itself.
    assert!(
        !fragment.contains("<!DOCTYPE"),
        "the fragment must not carry the base layout"
    );
    assert!(!fragment.contains("<nav"));
    // It carries the trigger for the next cycle — a fragment that lost
    // it would refresh exactly once and then go silent.
    assert!(fragment.contains(r#"id="queue-live""#));
    assert!(fragment.contains(r#"hx-get="/queue/live""#));
    assert!(fragment.contains("hx-trigger=\"every "));
}

#[tokio::test]
async fn an_empty_queue_asks_again_slowly() {
    let (addr, _state) = spawn().await;
    let body = reqwest::get(format!("http://{addr}/queue/live"))
        .await
        .expect("send")
        .text()
        .await
        .unwrap();
    let idle = brarr_orchestrator::queue::LIVE_POLL_IDLE.as_secs();
    assert!(
        body.contains(&format!(r#"hx-trigger="every {idle}s""#)),
        "an empty queue should back off, got: {body}"
    );
}

#[tokio::test]
async fn a_grab_still_moving_makes_the_page_ask_again_soon() {
    use brarr_orchestrator::db::grabs::{self, NewGrab, Protocol};
    use brarr_orchestrator::db::library::{self, MediaType, NewLibraryItem};
    use brarr_orchestrator::db::providers::{self, NewProvider};

    let (addr, state) = spawn().await;
    let item = library::upsert(
        state.pool(),
        &NewLibraryItem {
            media_type: Some(MediaType::Movie),
            tmdb_id: 603,
            title: "The Matrix".to_owned(),
            ..NewLibraryItem::default()
        },
    )
    .await
    .unwrap();
    let provider = providers::insert(
        state.pool(),
        NewProvider {
            name: "capybara",
            base_url: &url::Url::parse("https://capybarabr.com/").unwrap(),
            api_token: "tok",
            kind: "unit3d",
            plugin_path: None,
        },
    )
    .await
    .unwrap();
    grabs::reserve(
        state.pool(),
        &NewGrab {
            item_id: item.id,
            episode_id: None,
            season_number: None,
            decision_id: None,
            provider_id: provider.id,
            provider_name: "capybara",
            release_id_remote: "abc",
            release_name: "Matrix.1999.1080p.BluRay.PT-BR",
            download_url: None,
            protocol: Protocol::Torrent,
        },
    )
    .await
    .unwrap()
    .unwrap();

    let body = reqwest::get(format!("http://{addr}/queue/live"))
        .await
        .expect("send")
        .text()
        .await
        .unwrap();
    let active = brarr_orchestrator::queue::LIVE_POLL_ACTIVE.as_secs();
    assert!(
        body.contains(&format!(r#"hx-trigger="every {active}s""#)),
        "a grab that has not landed yet is still moving, got: {body}"
    );
}

// ---------- the manual scan reports without a reload ----------
//
// The sweep is spawned and outlives the request that started it, so past
// `MANUAL_SCAN_WAIT` the badge used to say "recarregue a página". These
// cover the mailbox that replaced that: what the badge asks, and when it
// is told to stop asking.

#[tokio::test]
async fn a_running_scan_keeps_the_badge_asking() {
    use brarr_orchestrator::scan::ScanProgress;
    use std::time::Instant;
    use uuid::Uuid;

    let (addr, state) = spawn().await;
    let id = Uuid::new_v4();
    state
        .scans()
        .insert(id, ScanProgress::Running, Instant::now());

    let resp = reqwest::get(format!("http://{addr}/library/{id}/scan/status"))
        .await
        .expect("send");
    assert_eq!(
        resp.status(),
        200,
        "a sweep still running must not carry the stop signal"
    );
    let body = resp.text().await.unwrap();
    assert!(body.contains(&format!(r#"hx-get="/library/{id}/scan/status""#)));
    assert!(body.contains("hx-trigger=\"every "));
    assert!(body.contains("buscando"));
}

#[tokio::test]
async fn a_finished_scan_reports_its_verdict_and_stops_asking() {
    use brarr_orchestrator::scan::{ScanProgress, ScanSummary};
    use std::time::Instant;
    use uuid::Uuid;

    let (addr, state) = spawn().await;
    let id = Uuid::new_v4();
    state.scans().insert(
        id,
        ScanProgress::Done(ScanSummary {
            targets: 3,
            searches: 3,
            grabbed: 2,
            ..ScanSummary::default()
        }),
        Instant::now(),
    );

    let resp = reqwest::get(format!("http://{addr}/library/{id}/scan/status"))
        .await
        .expect("send");
    // 286 is htmx's "stop polling". Right here — a sweep ends — and
    // wrong on /queue, which refills on its own.
    assert_eq!(resp.status(), 286);
    let body = resp.text().await.unwrap();
    assert!(body.contains("2 grab(s)"));
    assert!(
        !body.contains("hx-trigger"),
        "a finished sweep must not leave a live trigger behind"
    );
}

#[tokio::test]
async fn a_scan_nobody_started_answers_empty_rather_than_a_verdict() {
    use uuid::Uuid;

    let (addr, _state) = spawn().await;
    let id = Uuid::new_v4();
    let resp = reqwest::get(format!("http://{addr}/library/{id}/scan/status"))
        .await
        .expect("send");
    assert_eq!(resp.status(), 286);
    let body = resp.text().await.unwrap();
    assert!(body.contains(&format!(r#"id="scan-{id}""#)));
    assert!(!body.contains("hx-trigger"));
    assert!(
        !body.contains("badge"),
        "an expired mailbox is not a verdict — the slot goes back to empty"
    );
}

#[tokio::test]
async fn the_scan_button_leaves_its_verdict_in_the_mailbox() {
    use brarr_orchestrator::db::library::{self, MediaType, NewLibraryItem};
    use brarr_orchestrator::scan::ScanProgress;
    use std::time::Instant;

    let (addr, state) = spawn().await;
    let item = library::upsert(
        state.pool(),
        &NewLibraryItem {
            media_type: Some(MediaType::Movie),
            tmdb_id: 603,
            title: "The Matrix".to_owned(),
            ..NewLibraryItem::default()
        },
    )
    .await
    .unwrap();

    let resp = reqwest::Client::new()
        .post(format!("http://{addr}/library/{}/scan", item.id))
        .send()
        .await
        .expect("send");
    assert_eq!(resp.status(), 200);

    // With no providers configured the sweep beats the wait, so the
    // handler answers directly — and the mailbox still holds the same
    // verdict, which is what makes the slow path readable.
    let recorded = state.scans().get(&item.id, Instant::now());
    assert!(
        matches!(recorded, Some(ScanProgress::Done(_))),
        "the spawned sweep must record its outcome, got {recorded:?}"
    );
}

#[tokio::test]
async fn scan_now_reports_that_nothing_was_found() {
    use brarr_orchestrator::db::library::{self, MediaType, NewLibraryItem};

    let (addr, state) = spawn().await;
    let item = library::upsert(
        state.pool(),
        &NewLibraryItem {
            media_type: Some(MediaType::Movie),
            tmdb_id: 603,
            title: "The Matrix".to_owned(),
            ..NewLibraryItem::default()
        },
    )
    .await
    .unwrap();

    // No providers configured, so the search fans out to nobody.
    let resp = reqwest::Client::new()
        .post(format!("http://{addr}/library/{}/scan", item.id))
        .send()
        .await
        .expect("send");
    assert_eq!(resp.status(), 200);
    let badge = resp.text().await.unwrap();
    assert!(badge.contains("nada encontrado"), "badge = {badge}");
    assert!(
        badge.contains("bg-danger-soft"),
        "the badge carries defined colours, badge = {badge}"
    );

    // And nothing was reserved — a sweep that finds nothing must not
    // leave a grab behind that would keep the item out of later sweeps.
    let grabs = brarr_orchestrator::db::grabs::for_item(state.pool(), item.id)
        .await
        .unwrap();
    assert!(grabs.is_empty());
}

#[tokio::test]
async fn scan_now_says_so_when_a_grab_already_covers_the_item() {
    use brarr_orchestrator::db::grabs::{self, NewGrab, Protocol};
    use brarr_orchestrator::db::library::{self, MediaType, NewLibraryItem};
    use brarr_orchestrator::db::providers::{self, NewProvider};

    let (addr, state) = spawn().await;
    let item = library::upsert(
        state.pool(),
        &NewLibraryItem {
            media_type: Some(MediaType::Movie),
            tmdb_id: 603,
            title: "The Matrix".to_owned(),
            ..NewLibraryItem::default()
        },
    )
    .await
    .unwrap();
    let provider = providers::insert(
        state.pool(),
        NewProvider {
            name: "capybara",
            base_url: &url::Url::parse("https://capybarabr.com/").unwrap(),
            api_token: "tok",
            kind: "unit3d",
            plugin_path: None,
        },
    )
    .await
    .unwrap();
    grabs::reserve(
        state.pool(),
        &NewGrab {
            item_id: item.id,
            episode_id: None,
            season_number: None,
            decision_id: None,
            provider_id: provider.id,
            provider_name: "capybara",
            release_id_remote: "abc",
            release_name: "Matrix.1999.1080p",
            download_url: None,
            protocol: Protocol::Torrent,
        },
    )
    .await
    .unwrap()
    .unwrap();

    let badge = reqwest::Client::new()
        .post(format!("http://{addr}/library/{}/scan", item.id))
        .send()
        .await
        .expect("send")
        .text()
        .await
        .unwrap();
    assert!(badge.contains("já coberto"), "badge = {badge}");
    assert!(
        badge.contains("bg-success-soft"),
        "already covered is a fine outcome, not an error: {badge}"
    );
}

#[tokio::test]
async fn download_clients_index_renders_empty_state() {
    let (addr, _state) = spawn().await;
    let resp = reqwest::get(format!("http://{addr}/download-clients"))
        .await
        .expect("send");
    assert_eq!(resp.status(), 200);
    let body = resp.text().await.unwrap();
    assert!(body.contains("Nenhum cliente de download configurado"));
    assert!(body.contains("Clientes de download"));
}

#[tokio::test]
async fn create_then_delete_download_client_roundtrip() {
    let (addr, _state) = spawn().await;
    let client = reqwest::Client::new();

    let resp = client
        .post(format!("http://{addr}/download-clients"))
        .form(&[
            ("name", "qbittorrent-main"),
            ("kind", "qbittorrent"),
            ("base_url", "http://10.0.1.246:8080/"),
            ("username", "admin"),
            ("password", "hunter2"),
            ("category", "brarr"),
            ("priority", "1"),
        ])
        .send()
        .await
        .expect("send");
    assert_eq!(resp.status(), 200);
    let body = resp.text().await.unwrap();
    assert!(body.contains("qbittorrent-main"));
    assert!(body.contains("http://10.0.1.246:8080/"));
    // Protocol is derived from the kind, never submitted by the form.
    assert!(body.contains("torrent"));
    // The count rides inside the swapped partial — a header-rendered one
    // would still read zero right here.
    assert!(body.contains("1 cliente(s) configurado(s)"));
    // …and usenet has no home yet, which the operator should be told.
    assert!(body.contains("sem usenet"));
    assert!(!body.contains("sem torrent"));

    let body = reqwest::get(format!("http://{addr}/download-clients"))
        .await
        .expect("send")
        .text()
        .await
        .unwrap();
    let id = first_row_id(&body, "download-client-");

    let resp = client
        .delete(format!("http://{addr}/download-clients/{id}"))
        .send()
        .await
        .expect("send");
    assert_eq!(resp.status(), 200);
    assert!(
        resp.text()
            .await
            .unwrap()
            .contains("Nenhum cliente de download configurado")
    );
}

#[tokio::test]
async fn add_then_remove_a_root_folder() {
    let (addr, _state) = spawn().await;
    let client = reqwest::Client::new();
    let dir = std::env::temp_dir().join(format!("brarr-http-root-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();

    let resp = client
        .post(format!("http://{addr}/root-folders"))
        .form(&[("path", dir.to_str().unwrap()), ("media_type", "movie")])
        .send()
        .await
        .expect("send");
    assert_eq!(resp.status(), 200);
    let body = resp.text().await.unwrap();
    assert!(body.contains(dir.to_str().unwrap()));
    assert!(body.contains("Filmes"));
    // Free space is read from the real filesystem, so the row says
    // something concrete rather than "caminho inacessível".
    assert!(body.contains("livres de"), "body = {body}");

    let id = first_row_id(&body, "root-folder-");
    let resp = client
        .delete(format!("http://{addr}/root-folders/{id}"))
        .send()
        .await
        .expect("send");
    assert_eq!(resp.status(), 200);
    assert!(resp.text().await.unwrap().contains("Nenhuma pasta raiz"));
    assert!(
        dir.exists(),
        "removing a root folder forgets a destination; it does not delete a library"
    );
    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn a_root_folder_that_does_not_exist_is_refused_by_the_form() {
    let (addr, _state) = spawn().await;
    let resp = reqwest::Client::new()
        .post(format!("http://{addr}/root-folders"))
        .form(&[
            ("path", "/caminho/que/nao/existe/brarr"),
            ("media_type", "tv"),
        ])
        .send()
        .await
        .expect("send");
    assert_eq!(
        resp.status(),
        400,
        "a typo has to fail here, not hours later with a finished download"
    );
}

#[tokio::test]
async fn download_client_create_rejects_an_unknown_kind() {
    let (addr, _state) = spawn().await;
    let resp = reqwest::Client::new()
        .post(format!("http://{addr}/download-clients"))
        .form(&[
            ("name", "transmission"),
            ("kind", "transmission"),
            ("base_url", "http://10.0.1.246:9091/"),
        ])
        .send()
        .await
        .expect("send");
    assert_eq!(resp.status(), 400);
}

#[tokio::test]
async fn download_client_create_rejects_a_sabnzbd_without_an_api_key() {
    let (addr, _state) = spawn().await;
    let resp = reqwest::Client::new()
        .post(format!("http://{addr}/download-clients"))
        .form(&[
            ("name", "sabnzbd-main"),
            ("kind", "sabnzbd"),
            ("base_url", "http://10.0.1.246:8085/"),
        ])
        .send()
        .await
        .expect("send");
    assert_eq!(
        resp.status(),
        400,
        "a SABnzbd row with no apikey can never work — catch it at config time"
    );
}

#[tokio::test]
async fn the_edit_modal_never_echoes_a_stored_secret() {
    let (addr, _state) = spawn().await;
    let client = reqwest::Client::new();
    client
        .post(format!("http://{addr}/download-clients"))
        .form(&[
            ("name", "sabnzbd-main"),
            ("kind", "sabnzbd"),
            ("base_url", "http://10.0.1.246:8085/"),
            ("api_key", "super-secret-key"),
        ])
        .send()
        .await
        .expect("send");

    let body = reqwest::get(format!("http://{addr}/download-clients"))
        .await
        .expect("send")
        .text()
        .await
        .unwrap();
    assert!(
        !body.contains("super-secret-key"),
        "the list must not leak the credential either"
    );
    let id = first_row_id(&body, "download-client-");

    let modal = client
        .get(format!("http://{addr}/download-clients/{id}/edit"))
        .send()
        .await
        .expect("send")
        .text()
        .await
        .unwrap();
    assert!(
        !modal.contains("super-secret-key"),
        "the edit form renders credentials blank; a blank submit keeps them"
    );
    assert!(modal.contains("Há uma key salva"));

    // …and editing without re-typing the key keeps it. Renaming is the
    // realistic case, and it must not wipe the credential.
    let resp = client
        .put(format!("http://{addr}/download-clients/{id}"))
        .form(&[
            ("name", "sabnzbd-renomeado"),
            ("base_url", "http://10.0.1.246:8085/"),
            ("api_key", ""),
            ("priority", "2"),
        ])
        .send()
        .await
        .expect("send");
    assert_eq!(resp.status(), 200);
    assert!(resp.text().await.unwrap().contains("sabnzbd-renomeado"));

    let modal = client
        .get(format!("http://{addr}/download-clients/{id}/edit"))
        .send()
        .await
        .expect("send")
        .text()
        .await
        .unwrap();
    assert!(
        modal.contains("Há uma key salva"),
        "the stored apikey survived an edit that left the field blank"
    );
}

#[tokio::test]
async fn toggling_a_download_client_flips_the_action_label() {
    let (addr, _state) = spawn().await;
    let client = reqwest::Client::new();
    client
        .post(format!("http://{addr}/download-clients"))
        .form(&[
            ("name", "qb"),
            ("kind", "qbittorrent"),
            ("base_url", "http://10.0.1.246:8080/"),
        ])
        .send()
        .await
        .expect("send");
    let body = reqwest::get(format!("http://{addr}/download-clients"))
        .await
        .expect("send")
        .text()
        .await
        .unwrap();
    assert!(body.contains("desativar"));
    let id = first_row_id(&body, "download-client-");

    let body = client
        .post(format!("http://{addr}/download-clients/{id}/toggle"))
        .send()
        .await
        .expect("send")
        .text()
        .await
        .unwrap();
    assert!(body.contains(">ativar<") || body.contains("ativar"));
    assert!(body.contains("opacity-55"), "a drained row renders muted");
}

#[tokio::test]
async fn testing_a_dead_download_client_answers_a_failure_badge() {
    let (addr, _state) = spawn().await;
    let client = reqwest::Client::new();
    client
        .post(format!("http://{addr}/download-clients"))
        .form(&[
            ("name", "qb-desligado"),
            ("kind", "qbittorrent"),
            // Port 1 is unbindable by an unprivileged process.
            ("base_url", "http://127.0.0.1:1/"),
            ("username", "admin"),
            ("password", "x"),
        ])
        .send()
        .await
        .expect("send");
    let body = reqwest::get(format!("http://{addr}/download-clients"))
        .await
        .expect("send")
        .text()
        .await
        .unwrap();
    let id = first_row_id(&body, "download-client-");

    let resp = client
        .post(format!("http://{addr}/download-clients/{id}/test"))
        .send()
        .await
        .expect("send");
    assert_eq!(resp.status(), 200);
    let badge = resp.text().await.unwrap();
    assert!(badge.contains("erro"), "badge = {badge}");
    assert!(
        badge.contains("bg-danger-soft") && badge.contains("text-danger-soft-fg"),
        "the badge has to carry colours the stylesheet actually defines, badge = {badge}"
    );
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
            release_guid: None,
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

// ---------------------------------------------------------------------
// Remote path mapping
// ---------------------------------------------------------------------

/// Register a download client through the real handler, returning its id.
async fn seed_client(addr: SocketAddr, name: &str) -> String {
    let client = reqwest::Client::new();
    let resp = client
        .post(format!("http://{addr}/download-clients"))
        .form(&[
            ("name", name),
            ("kind", "qbittorrent"),
            ("base_url", "http://127.0.0.1:8080/"),
            ("username", "u"),
            ("password", "p"),
            ("category", "brarr"),
            ("priority", "1"),
        ])
        .send()
        .await
        .expect("send");
    assert_eq!(resp.status(), 200, "seeding a client must succeed");

    // The id is only in the rendered table; pull it off the edit link.
    let body = resp.text().await.unwrap();
    let marker = "/download-clients/";
    let start = body.find(marker).expect("a client row with an id") + marker.len();
    body[start..]
        .chars()
        .take_while(|c| c.is_ascii_hexdigit() || *c == '-')
        .collect()
}

#[tokio::test]
async fn the_download_clients_page_offers_path_mapping() {
    let (addr, _state) = spawn().await;
    let client = reqwest::Client::new();
    let body = client
        .get(format!("http://{addr}/download-clients"))
        .send()
        .await
        .expect("send")
        .text()
        .await
        .unwrap();

    assert!(
        body.contains("Mapeamento de caminhos"),
        "the mapping block has to be on the screen that configures clients"
    );
    assert!(
        body.contains("Nenhum mapeamento"),
        "zero state, body = {body}"
    );
}

#[tokio::test]
async fn a_path_mapping_round_trips_through_the_form() {
    let (addr, _state) = spawn().await;
    let id = seed_client(addr, "qb").await;
    let http = reqwest::Client::new();

    // The local side must exist — it is checked at registration, exactly
    // like a root folder.
    let local = std::env::temp_dir();
    let local = local.to_string_lossy();

    let resp = http
        .post(format!("http://{addr}/path-mappings"))
        .form(&[
            ("client_id", id.as_str()),
            // Deliberately noisy: trailing and doubled separators.
            ("remote_prefix", "/data//torrents/"),
            ("local_prefix", &local),
        ])
        .send()
        .await
        .expect("send");
    assert_eq!(resp.status(), 200);

    let body = resp.text().await.unwrap();
    assert!(
        body.contains("/data/torrents"),
        "the stored prefix is canonical, body = {body}"
    );
    assert!(body.contains("qb"), "the row names its client");
}

#[tokio::test]
async fn a_local_side_that_does_not_exist_is_refused_with_400_not_500() {
    let (addr, _state) = spawn().await;
    let id = seed_client(addr, "qb").await;
    let http = reqwest::Client::new();

    let resp = http
        .post(format!("http://{addr}/path-mappings"))
        .form(&[
            ("client_id", id.as_str()),
            ("remote_prefix", "/data/torrents"),
            ("local_prefix", "/isto/nao/existe/em/lugar/nenhum"),
        ])
        .send()
        .await
        .expect("send");

    assert_eq!(
        resp.status(),
        400,
        "a typo in the form is form input, not a server fault"
    );
}

#[tokio::test]
async fn a_remote_prefix_that_would_match_everything_is_refused() {
    let (addr, _state) = spawn().await;
    let id = seed_client(addr, "qb").await;
    let http = reqwest::Client::new();
    let local = std::env::temp_dir();

    let resp = http
        .post(format!("http://{addr}/path-mappings"))
        .form(&[
            ("client_id", id.as_str()),
            ("remote_prefix", "/"),
            ("local_prefix", local.to_string_lossy().as_ref()),
        ])
        .send()
        .await
        .expect("send");

    assert_eq!(resp.status(), 400, "a bare root would rewrite every path");
}

#[tokio::test]
async fn a_duplicate_mapping_is_refused_with_400_not_500() {
    let (addr, _state) = spawn().await;
    let id = seed_client(addr, "qb").await;
    let http = reqwest::Client::new();
    let local = std::env::temp_dir();
    let local = local.to_string_lossy();

    for expected in [200, 400] {
        let resp = http
            .post(format!("http://{addr}/path-mappings"))
            .form(&[
                ("client_id", id.as_str()),
                ("remote_prefix", "/data/torrents"),
                ("local_prefix", &local),
            ])
            .send()
            .await
            .expect("send");
        assert_eq!(
            resp.status(),
            expected,
            "the second insert collides on UNIQUE and must be a form error"
        );
    }
}

#[tokio::test]
async fn deleting_a_mapping_answers_the_refreshed_block() {
    let (addr, _state) = spawn().await;
    let id = seed_client(addr, "qb").await;
    let http = reqwest::Client::new();
    let local = std::env::temp_dir();
    let local = local.to_string_lossy();

    let created = http
        .post(format!("http://{addr}/path-mappings"))
        .form(&[
            ("client_id", id.as_str()),
            ("remote_prefix", "/data/torrents"),
            ("local_prefix", &local),
        ])
        .send()
        .await
        .expect("send")
        .text()
        .await
        .unwrap();

    let marker = "/path-mappings/";
    let start = created.find(marker).expect("a delete link") + marker.len();
    let mapping_id: String = created[start..]
        .chars()
        .take_while(|c| c.is_ascii_hexdigit() || *c == '-')
        .collect();

    let resp = http
        .delete(format!("http://{addr}/path-mappings/{mapping_id}"))
        .send()
        .await
        .expect("send");
    assert_eq!(resp.status(), 200);
    let body = resp.text().await.unwrap();
    assert!(
        body.contains("Nenhum mapeamento"),
        "back to the zero state, body = {body}"
    );
}

#[tokio::test]
async fn requeueing_an_unknown_grab_is_not_an_error() {
    // The button posts an id that may already have been requeued in
    // another tab. The refreshed block tells the truth either way; a 500
    // would be theatre.
    let (addr, _state) = spawn().await;
    let http = reqwest::Client::new();
    let resp = http
        .post(format!(
            "http://{addr}/grabs/{}/requeue-import",
            uuid::Uuid::new_v4()
        ))
        .send()
        .await
        .expect("send");
    assert_eq!(resp.status(), 200);
}

// ---------------------------------------------------------------------
// Add-with-options dialog
// ---------------------------------------------------------------------

#[tokio::test]
async fn the_add_screen_opens_a_dialog_instead_of_adding_blind() {
    let (addr, _state) = spawn().await;
    let client = reqwest::Client::new();
    let body = client
        .get(format!("http://{addr}/library/add"))
        .send()
        .await
        .expect("send")
        .text()
        .await
        .unwrap();

    assert!(
        !body.contains(r#"<form method="post" action="/library/add""#),
        "the bare POST form is what made every choice an invisible default"
    );
}

#[tokio::test]
async fn the_options_dialog_shows_what_will_be_used() {
    let (addr, _state) = spawn().await;
    let client = reqwest::Client::new();
    let resp = client
        .get(format!(
            "http://{addr}/library/add/options?tmdb_id=603&media_type=tv&title=The%20Matrix"
        ))
        .send()
        .await
        .expect("send");
    assert_eq!(resp.status(), 200);
    let body = resp.text().await.unwrap();

    assert!(body.contains("<dialog"), "it is a modal, body = {body}");
    assert!(body.contains("Pasta raiz"));
    assert!(body.contains("Perfil de qualidade"));
    assert!(body.contains("Monitorar"), "a series gets the scope select");
    assert!(
        body.contains("Buscar agora"),
        "searching on add is an explicit choice"
    );
    assert!(
        body.contains("corte padrão em 150"),
        "the fallback threshold is shown rather than being another invisible default"
    );
}

#[tokio::test]
async fn a_movie_dialog_has_no_season_scope() {
    let (addr, _state) = spawn().await;
    let client = reqwest::Client::new();
    let body = client
        .get(format!(
            "http://{addr}/library/add/options?tmdb_id=603&media_type=movie&title=Matrix"
        ))
        .send()
        .await
        .expect("send")
        .text()
        .await
        .unwrap();

    assert!(
        !body.contains("Só a primeira temporada"),
        "a movie has no season tree, body = {body}"
    );
    assert!(body.contains("Pasta raiz"), "but it still picks a folder");
}

#[tokio::test]
async fn an_unregistered_root_folder_is_refused_on_add() {
    // The dialog only offers registered folders, but the request is
    // anybody's and this value becomes a directory brarr writes into.
    let (addr, _state) = spawn().await;
    let client = reqwest::Client::new();
    let resp = client
        .post(format!("http://{addr}/library/add"))
        .form(&[
            ("tmdb_id", "603"),
            ("media_type", "movie"),
            ("root_folder", "/etc"),
        ])
        .send()
        .await
        .expect("send");

    assert_eq!(resp.status(), 400);
}

#[tokio::test]
async fn an_unknown_monitor_scope_is_refused() {
    let (addr, _state) = spawn().await;
    let client = reqwest::Client::new();
    let resp = client
        .post(format!("http://{addr}/library/add"))
        .form(&[
            ("tmdb_id", "603"),
            ("media_type", "tv"),
            ("monitor_scope", "latest-season"),
        ])
        .send()
        .await
        .expect("send");

    assert_eq!(
        resp.status(),
        400,
        "there is deliberately no latest-season scope — one stored value \
         cannot both monitor a new season and unmonitor the previous one"
    );
}
