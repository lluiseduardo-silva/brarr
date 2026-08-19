//! Against a real server, on purpose.
//!
//! Ignored by default — these need a running media server and a
//! credential, and they are the half a mock cannot defend: only the real
//! thing says whether the token brarr obtained is accepted, and whether
//! the sections it lists are the ones the operator sees.
//!
//! ```bash
//! BRARR_LIVE_PLEX_URL=http://10.0.1.248:32400 \
//! BRARR_LIVE_PLEX_TOKEN=<token> \
//!   cargo test -p brarr-media-server --test live_api -- --ignored --nocapture
//! ```
//!
//! For Jellyfin/Emby, the same with `BRARR_LIVE_JELLYFIN_URL` +
//! `BRARR_LIVE_JELLYFIN_KEY` (or `..._EMBY_...`).
//!
//! Everything here is **read-only**. There is deliberately no test that
//! fires a refresh: that is a write against the operator's server, and it
//! belongs in the end-to-end script where a human is watching, not in a
//! suite someone might run twice by reflex.
//!
//! A claim that only a skipped test defends is a claim nobody checks — so
//! every behaviour asserted here also has a mocked counterpart in
//! `client_wiremock.rs`. These exist to catch the thing mocks cannot: a
//! payload shape that changed upstream.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::print_stderr,
    clippy::print_stdout,
    clippy::panic,
    reason = "an ignored live test reports to a human"
)]

use brarr_media_server::{MediaServer, MediaServerConfig, MediaServerKind, build};
use url::Url;

fn live(url_var: &str, token_var: &str, kind: MediaServerKind) -> Option<Box<dyn MediaServer>> {
    let url = std::env::var(url_var)
        .ok()
        .filter(|v| !v.trim().is_empty())?;
    let token = std::env::var(token_var)
        .ok()
        .filter(|v| !v.trim().is_empty())?;
    let base_url = Url::parse(&url).ok()?;
    build(MediaServerConfig {
        name: format!("{kind} ao vivo"),
        kind,
        base_url,
        token: Some(token),
    })
    .ok()
}

#[tokio::test]
#[ignore = "needs BRARR_LIVE_PLEX_URL + BRARR_LIVE_PLEX_TOKEN and a network"]
async fn plex_lists_its_sections() {
    let Some(client) = live(
        "BRARR_LIVE_PLEX_URL",
        "BRARR_LIVE_PLEX_TOKEN",
        MediaServerKind::Plex,
    ) else {
        eprintln!("BRARR_LIVE_PLEX_URL/TOKEN unset — skipping");
        return;
    };

    let status = client.test_connection().await.expect("the token is good");
    println!(
        "Plex {} — {} seções:",
        status.version,
        status.libraries.len()
    );
    for library in &status.libraries {
        println!(
            "  [{}] {} → {:?}",
            library.id, library.title, library.locations
        );
    }
    assert!(!status.version.is_empty(), "the server reported a version");
    assert!(
        !status.libraries.is_empty(),
        "a server with no library is a configuration the notify path cannot serve"
    );
    assert!(
        status.libraries.iter().all(|l| !l.locations.is_empty()),
        "a section with no location can never match a path"
    );
}

#[tokio::test]
#[ignore = "needs BRARR_LIVE_PLEX_URL + BRARR_LIVE_PLEX_TOKEN and a network"]
async fn plex_refuses_a_tampered_token() {
    let Some(url) = std::env::var("BRARR_LIVE_PLEX_URL").ok() else {
        eprintln!("BRARR_LIVE_PLEX_URL unset — skipping");
        return;
    };
    let client = build(MediaServerConfig {
        name: "plex com token errado".to_owned(),
        kind: MediaServerKind::Plex,
        base_url: Url::parse(&url).unwrap(),
        token: Some("nao-vale-de-nada".to_owned()),
    })
    .unwrap();

    let err = client.test_connection().await.expect_err("401 esperado");
    println!("recusa: {err}");
    assert!(
        matches!(err, brarr_media_server::MediaServerError::Auth { .. }),
        "a wrong token has to read as a credential problem, not as a dead host: {err:?}"
    );
}

#[tokio::test]
#[ignore = "needs BRARR_LIVE_JELLYFIN_URL + BRARR_LIVE_JELLYFIN_KEY and a network"]
async fn jellyfin_lists_its_libraries() {
    let Some(client) = live(
        "BRARR_LIVE_JELLYFIN_URL",
        "BRARR_LIVE_JELLYFIN_KEY",
        MediaServerKind::Jellyfin,
    ) else {
        eprintln!("BRARR_LIVE_JELLYFIN_URL/KEY unset — skipping");
        return;
    };

    let status = client.test_connection().await.expect("the key is good");
    println!(
        "Jellyfin {} — {} bibliotecas:",
        status.version,
        status.libraries.len()
    );
    for library in &status.libraries {
        println!("  {} → {:?}", library.title, library.locations);
    }
    assert!(!status.version.is_empty());
}

#[tokio::test]
#[ignore = "needs BRARR_LIVE_EMBY_URL + BRARR_LIVE_EMBY_KEY and a network"]
async fn emby_lists_its_libraries() {
    let Some(client) = live(
        "BRARR_LIVE_EMBY_URL",
        "BRARR_LIVE_EMBY_KEY",
        MediaServerKind::Emby,
    ) else {
        eprintln!("BRARR_LIVE_EMBY_URL/KEY unset — skipping");
        return;
    };

    let status = client.test_connection().await.expect("the key is good");
    println!(
        "Emby {} — {} bibliotecas:",
        status.version,
        status.libraries.len()
    );
    assert!(!status.version.is_empty());
}
