//! The `/media-servers` screen, driven through the real router.
//!
//! Two things here can only be caught over HTTP. The first is that the
//! edit modal must never echo a credential back: it is a template
//! property, and no unit test of the db layer can see it — while the
//! whole "blank means keep" convention depends on it being true. The
//! second is the Plex sign-in, whose contract *is* the fragment: it
//! carries its own `hx-trigger`, so a response that forgets it stops the
//! login silently, and a terminal response that forgets to answer `286`
//! leaves the browser asking plex.tv forever.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::doc_markdown
)]

use std::net::SocketAddr;
use std::time::Duration;

use brarr_decision_service::Engine;
use brarr_orchestrator::db::media_servers;
use brarr_orchestrator::{AppState, db, web};

struct Harness {
    addr: SocketAddr,
    state: AppState,
}

async fn spawn() -> Harness {
    let pool = db::open_memory().await.expect("open in-memory db");
    let state = AppState::new(pool, Engine::baseline());
    let static_dir = std::env::temp_dir().join("brarr-media-servers-static");
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
    Harness { addr, state }
}

fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap()
}

#[tokio::test]
async fn the_screen_renders_and_the_nav_links_to_it() {
    let h = spawn().await;
    let body = client()
        .get(format!("http://{}/media-servers", h.addr))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();

    assert!(body.contains("Media servers"));
    assert!(
        body.contains("Nenhum media server configurado"),
        "the empty state has to explain what is missing"
    );
    assert!(
        body.contains("href=\"/media-servers\""),
        "the nav needs a way in, or the screen exists and nobody finds it"
    );
}

#[tokio::test]
async fn creating_a_server_lists_it_and_counts_the_missing_credential() {
    let h = spawn().await;
    let body = client()
        .post(format!("http://{}/media-servers", h.addr))
        .form(&[
            ("name", "plex-casa"),
            ("kind", "plex"),
            ("base_url", "http://10.0.1.248:32400"),
            ("token", ""),
        ])
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();

    assert!(body.contains("plex-casa"));
    assert!(
        body.contains("1 sem credencial"),
        "the warning lives inside the swapped region so it cannot go stale: {body}"
    );
    assert!(
        body.contains("entrar com o Plex"),
        "a Plex row offers the sign-in; a pasted token is the fallback"
    );
}

#[tokio::test]
async fn jellyfin_gets_no_plex_sign_in_button() {
    let h = spawn().await;
    let body = client()
        .post(format!("http://{}/media-servers", h.addr))
        .form(&[
            ("name", "jellyfin"),
            ("kind", "jellyfin"),
            ("base_url", "http://10.0.1.9:8096"),
            ("token", "uma-api-key"),
        ])
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();

    assert!(body.contains("jellyfin"));
    assert!(
        !body.contains("entrar com o Plex"),
        "Jellyfin authenticates with a key from its own panel"
    );
    assert!(!body.contains("sem credencial"), "it was created with one");
}

#[tokio::test]
async fn the_edit_modal_never_echoes_the_credential() {
    let h = spawn().await;
    let row = media_servers::insert(
        h.state.pool(),
        media_servers::NewMediaServer {
            name: "emby",
            kind: brarr_media_server::MediaServerKind::Emby,
            base_url: "http://10.0.1.224:8096",
            token: Some("um-segredo-muito-especifico"),
        },
    )
    .await
    .unwrap();

    let body = client()
        .get(format!("http://{}/media-servers/{}/edit", h.addr, row.id))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();

    assert!(
        !body.contains("um-segredo-muito-especifico"),
        "the whole \"blank means keep\" convention depends on this being true"
    );
    assert!(
        body.contains("Há uma credencial salva"),
        "the operator still has to be told there is one: {body}"
    );
    assert!(
        body.contains("API key") && !body.contains("Token do Plex"),
        "Emby's credential is a key from its own panel, and the label has to say so"
    );
}

#[tokio::test]
async fn the_plex_modal_points_back_at_the_sign_in() {
    let h = spawn().await;
    let row = media_servers::insert(
        h.state.pool(),
        media_servers::NewMediaServer {
            name: "plex",
            kind: brarr_media_server::MediaServerKind::Plex,
            base_url: "http://10.0.1.248:32400",
            token: None,
        },
    )
    .await
    .unwrap();

    let body = client()
        .get(format!("http://{}/media-servers/{}/edit", h.addr, row.id))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();

    assert!(body.contains("Token do Plex"));
    assert!(
        body.contains("entrar com o Plex"),
        "pasting a token by hand is the rescue, not the path — the modal has to say which is which"
    );
    assert!(body.contains("Nenhuma credencial salva"));
}

#[tokio::test]
async fn a_blank_credential_on_edit_keeps_the_stored_one() {
    let h = spawn().await;
    let row = media_servers::insert(
        h.state.pool(),
        media_servers::NewMediaServer {
            name: "emby",
            kind: brarr_media_server::MediaServerKind::Emby,
            base_url: "http://10.0.1.224:8096",
            token: Some("guardado"),
        },
    )
    .await
    .unwrap();

    client()
        .put(format!("http://{}/media-servers/{}", h.addr, row.id))
        .form(&[
            ("name", "emby renomeado"),
            ("base_url", "http://10.0.1.224:8096"),
            ("token", ""),
        ])
        .send()
        .await
        .unwrap();

    let after = media_servers::get_by_id(h.state.pool(), row.id)
        .await
        .unwrap();
    assert_eq!(after.name, "emby renomeado");
    assert_eq!(after.token.as_deref(), Some("guardado"));
}

#[tokio::test]
async fn the_sign_in_is_refused_for_a_server_that_does_not_use_it() {
    let h = spawn().await;
    let row = media_servers::insert(
        h.state.pool(),
        media_servers::NewMediaServer {
            name: "jellyfin",
            kind: brarr_media_server::MediaServerKind::Jellyfin,
            base_url: "http://10.0.1.9:8096",
            token: Some("k"),
        },
    )
    .await
    .unwrap();

    let res = client()
        .post(format!(
            "http://{}/media-servers/{}/plex/login",
            h.addr, row.id
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 400, "a Jellyfin row has no plex.tv sign-in");
}

#[tokio::test]
async fn a_status_poll_with_no_login_in_flight_stops_asking() {
    let h = spawn().await;
    let row = media_servers::insert(
        h.state.pool(),
        media_servers::NewMediaServer {
            name: "plex",
            kind: brarr_media_server::MediaServerKind::Plex,
            base_url: "http://10.0.1.248:32400",
            token: None,
        },
    )
    .await
    .unwrap();

    let res = client()
        .get(format!(
            "http://{}/media-servers/{}/plex/login/status",
            h.addr, row.id
        ))
        .send()
        .await
        .unwrap();

    // 286 is htmx's stop-polling status. Without it the browser keeps
    // asking about a login that no longer exists — which is exactly the
    // unbounded loop both *arr ship.
    assert_eq!(res.status(), 286);
    let body = res.text().await.unwrap();
    assert!(body.contains("expirou"), "got {body}");
    assert!(
        !body.contains("hx-trigger"),
        "a terminal answer must not reprogram the poll"
    );
}

#[tokio::test]
async fn a_path_mapping_round_trips_through_the_screen() {
    let h = spawn().await;
    let row = media_servers::insert(
        h.state.pool(),
        media_servers::NewMediaServer {
            name: "plex",
            kind: brarr_media_server::MediaServerKind::Plex,
            base_url: "http://10.0.1.248:32400",
            token: Some("t"),
        },
    )
    .await
    .unwrap();
    let local = std::env::temp_dir();

    let body = client()
        .post(format!("http://{}/media-server-mappings", h.addr))
        .form(&[
            ("server_id", row.id.to_string().as_str()),
            // `/mnt/midias` does not exist on this machine, and that is
            // the entire reason the row exists.
            ("remote_prefix", "/mnt/midias"),
            ("local_prefix", local.to_string_lossy().as_ref()),
        ])
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();

    assert!(body.contains("/mnt/midias"), "got {body}");
    assert!(
        body.contains("O servidor enxerga"),
        "the table has to say which column is whose"
    );
}

#[tokio::test]
async fn removing_a_server_takes_its_mappings_with_it() {
    let h = spawn().await;
    let row = media_servers::insert(
        h.state.pool(),
        media_servers::NewMediaServer {
            name: "plex",
            kind: brarr_media_server::MediaServerKind::Plex,
            base_url: "http://10.0.1.248:32400",
            token: Some("t"),
        },
    )
    .await
    .unwrap();
    client()
        .post(format!("http://{}/media-server-mappings", h.addr))
        .form(&[
            ("server_id", row.id.to_string().as_str()),
            ("remote_prefix", "/mnt/midias"),
            (
                "local_prefix",
                std::env::temp_dir().to_string_lossy().as_ref(),
            ),
        ])
        .send()
        .await
        .unwrap();

    client()
        .delete(format!("http://{}/media-servers/{}", h.addr, row.id))
        .send()
        .await
        .unwrap();

    assert!(
        brarr_orchestrator::db::media_server_mappings::list_all(h.state.pool())
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn the_client_identifier_is_generated_once_and_never_changes() {
    let h = spawn().await;
    let first = brarr_orchestrator::notify::plex_identity(h.state.pool())
        .await
        .unwrap();
    let second = brarr_orchestrator::notify::plex_identity(h.state.pool())
        .await
        .unwrap();
    assert_eq!(
        first.client_identifier, second.client_identifier,
        "a value that changes between creating a PIN and redeeming it orphans the token"
    );
    assert!(!first.client_identifier.trim().is_empty());
}
