//! Both dialects against a mock server.
//!
//! The Plex fixture is a real capture (see `tests/fixtures/README.md`),
//! so the section tests run against the shape a live server actually
//! emits — including the two `show` sections that make picking by media
//! type wrong.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests assert on happy paths"
)]

use brarr_media_server::plex::{PinState, PlexIdentity, PlexLogin};
use brarr_media_server::{
    LibraryUpdate, MediaServer, MediaServerConfig, MediaServerError, MediaServerKind, PlexClient,
    build,
};
use url::Url;
use wiremock::matchers::{body_json_string, header, method, path, path_regex, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

const TOKEN: &str = "a-plex-token";
const API_KEY: &str = "an-api-key";

fn fixture(name: &str) -> String {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("reading {}: {e}", path.display()))
}

fn config(server: &MockServer, kind: MediaServerKind, token: &str) -> MediaServerConfig {
    MediaServerConfig {
        name: format!("{kind} de teste"),
        kind,
        base_url: Url::parse(&server.uri()).unwrap(),
        token: Some(token.to_owned()),
    }
}

async fn mount_sections(server: &MockServer) {
    Mock::given(method("GET"))
        .and(path("/library/sections"))
        .and(header("X-Plex-Token", TOKEN))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_raw(fixture("plex_sections.json"), "application/json"),
        )
        .mount(server)
        .await;
}

/// One changed folder, described the way the notify path describes it.
fn upd(path: &str, relative: &str, root: &str) -> LibraryUpdate {
    LibraryUpdate {
        path: path.to_owned(),
        relative: relative.to_owned(),
        root_name: root.to_owned(),
    }
}

// ─── Plex ───────────────────────────────────────────────────────────

#[tokio::test]
async fn plex_reads_the_real_section_payload() {
    let server = MockServer::start().await;
    mount_sections(&server).await;
    Mock::given(method("GET"))
        .and(path("/identity"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            r#"{"MediaContainer":{"size":0,"version":"1.43.3.10861-07dfddaeb"}}"#,
            "application/json",
        ))
        .mount(&server)
        .await;

    let client = PlexClient::new(config(&server, MediaServerKind::Plex, TOKEN)).unwrap();
    let status = client.test_connection().await.expect("the token is good");

    assert_eq!(status.version, "1.43.3.10861-07dfddaeb");
    assert_eq!(status.libraries.len(), 3);
    // `key` is a string in the payload while `Location.id`, in the same
    // object, is a number. Keeping it as text is what this asserts.
    let ids: Vec<&str> = status.libraries.iter().map(|l| l.id.as_str()).collect();
    assert_eq!(ids, ["3", "1", "2"]);
    assert_eq!(status.libraries[1].locations, ["/mnt/midias/Animes"]);
}

#[tokio::test]
async fn plex_refreshes_only_the_section_that_holds_the_path() {
    let server = MockServer::start().await;
    mount_sections(&server).await;
    // Section 2 is `Series`; section 1 is `Animes`. Both are type `show`,
    // so a media-type choice would have a 50% chance of being right.
    Mock::given(method("GET"))
        .and(path("/library/sections/2/refresh"))
        .and(query_param("path", "/mnt/midias/Series/Fringe"))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&server)
        .await;

    let client = build(config(&server, MediaServerKind::Plex, TOKEN)).unwrap();
    client
        .notify_updated(&[upd("/mnt/midias/Series/Fringe", "Fringe", "Series")])
        .await
        .expect("the section covers it");

    let refreshes = server
        .received_requests()
        .await
        .unwrap()
        .into_iter()
        .filter(|r| r.url.path().contains("/refresh"))
        .count();
    assert_eq!(refreshes, 1, "one section, not a fan-out over all of them");
}

#[tokio::test]
async fn plex_re_anchors_a_path_no_section_covers() {
    let server = MockServer::start().await;
    mount_sections(&server).await;
    // brarr writes `/midias/...`, this Plex only knows `/mnt/midias/...`.
    // That used to be refused. It is exactly the configuration both *arr
    // work under with nothing configured, because `UpdateSectionPath`
    // builds `location.Path + separator + relativePath` and never sends
    // its own absolute path — so the tail is re-anchored on the shelf
    // whose folder is named after brarr's root.
    Mock::given(method("GET"))
        .and(path("/library/sections/2/refresh"))
        .and(query_param("path", "/mnt/midias/Series/Fringe"))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&server)
        .await;

    let client = build(config(&server, MediaServerKind::Plex, TOKEN)).unwrap();
    client
        .notify_updated(&[upd("/midias/Series/Fringe", "Fringe", "Series")])
        .await
        .expect("re-anchored onto the Series shelf");

    let refreshes = server
        .received_requests()
        .await
        .unwrap()
        .into_iter()
        .filter(|r| r.url.path().contains("/refresh"))
        .count();
    assert_eq!(
        refreshes, 1,
        "the root is called Series and exactly one shelf ends in Series, so no guessing"
    );
}

#[tokio::test]
async fn a_root_matching_no_shelf_name_reaches_every_shelf() {
    let server = MockServer::start().await;
    mount_sections(&server).await;
    Mock::given(method("GET"))
        .and(path_regex(r"^/library/sections/\d+/refresh$"))
        .respond_with(ResponseTemplate::new(200))
        .expect(3)
        .mount(&server)
        .await;

    let client = build(config(&server, MediaServerKind::Plex, TOKEN)).unwrap();
    client
        .notify_updated(&[upd("/midias/Cartoons/Ducktales", "Ducktales", "Cartoons")])
        .await
        .expect("falls back the way Radarr does");
    // Two of those three name a directory the shelf does not have. The
    // server ignores them, which is what makes the fallback survivable —
    // and it is strictly better than refusing to say anything.
}

#[tokio::test]
async fn plex_only_refuses_when_the_server_serves_nothing() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/library/sections"))
        .and(header("X-Plex-Token", TOKEN))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            r#"{"MediaContainer":{"size":0,"Directory":[]}}"#,
            "application/json",
        ))
        .mount(&server)
        .await;

    let client = build(config(&server, MediaServerKind::Plex, TOKEN)).unwrap();
    let err = client
        .notify_updated(&[upd("/midias/Series/Fringe", "Fringe", "Series")])
        .await
        .expect_err("there is nowhere to point");
    assert!(
        matches!(err, MediaServerError::NoMatchingLibrary { .. }),
        "got {err:?}"
    );
}

#[tokio::test]
async fn plex_reports_a_refused_token_as_auth_and_not_as_a_dead_host() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/library/sections"))
        .respond_with(ResponseTemplate::new(401))
        .mount(&server)
        .await;

    let client = PlexClient::new(config(&server, MediaServerKind::Plex, "wrong")).unwrap();
    let err = client.test_connection().await.expect_err("401");
    assert!(matches!(err, MediaServerError::Auth { .. }), "got {err:?}");
}

#[tokio::test]
async fn plex_reports_an_unreachable_host_as_transport() {
    let config = MediaServerConfig {
        name: "morto".to_owned(),
        kind: MediaServerKind::Plex,
        // Port 1 is reserved and nothing listens on it.
        base_url: Url::parse("http://127.0.0.1:1/").unwrap(),
        token: Some(TOKEN.to_owned()),
    };
    let client = PlexClient::new(config).unwrap();
    let err = client.test_connection().await.expect_err("nothing listens");
    assert!(
        matches!(err, MediaServerError::Transport { .. }),
        "a dead host is not a wrong password: {err:?}"
    );
}

#[tokio::test]
async fn plex_keeps_the_token_out_of_the_query_string() {
    let server = MockServer::start().await;
    mount_sections(&server).await;
    Mock::given(method("GET"))
        .and(path("/identity"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            r#"{"MediaContainer":{"version":"1.43.3"}}"#,
            "application/json",
        ))
        .mount(&server)
        .await;

    let client = PlexClient::new(config(&server, MediaServerKind::Plex, TOKEN)).unwrap();
    client.test_connection().await.unwrap();

    for request in server.received_requests().await.unwrap() {
        assert!(
            !request.url.as_str().contains(TOKEN),
            "the credential must not reach an access log: {}",
            request.url
        );
    }
}

#[tokio::test]
async fn a_plex_row_without_a_token_fails_before_any_request() {
    let config = MediaServerConfig {
        name: "sem token".to_owned(),
        kind: MediaServerKind::Plex,
        base_url: Url::parse("http://127.0.0.1:32400/").unwrap(),
        token: None,
    };
    // `Box<dyn MediaServer>` is not `Debug`, so the Result is unwrapped
    // by hand rather than through `expect_err`.
    let Err(err) = build(config) else {
        panic!("a row with no token must not build a client");
    };
    assert!(
        matches!(err, MediaServerError::Config { .. }),
        "got {err:?}"
    );
}

// ─── Emby / Jellyfin ────────────────────────────────────────────────

async fn mount_media_browser(server: &MockServer) {
    Mock::given(method("GET"))
        .and(path("/System/Info"))
        .and(header("X-MediaBrowser-Token", API_KEY))
        .and(header(
            "Authorization",
            format!("MediaBrowser Token=\"{API_KEY}\"").as_str(),
        ))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            r#"{"Version":"10.10.7","ServerName":"jellyfin-test"}"#,
            "application/json",
        ))
        .mount(server)
        .await;
    Mock::given(method("GET"))
        .and(path("/Library/VirtualFolders"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            r#"[{"Name":"Filmes","ItemId":"7","Locations":["/media/Filmes"],"CollectionType":"movies"},{"Name":"Series","ItemId":"8","Locations":["/media/Series"],"CollectionType":"tvshows"}]"#,
            "application/json",
        ))
        .mount(server)
        .await;
}

#[tokio::test]
async fn jellyfin_and_emby_run_the_same_code_path() {
    for kind in [MediaServerKind::Jellyfin, MediaServerKind::Emby] {
        let server = MockServer::start().await;
        mount_media_browser(&server).await;

        let client = build(config(&server, kind, API_KEY)).unwrap();
        let status = client.test_connection().await.expect("the key is good");

        assert_eq!(client.kind(), kind, "the row still knows which it is");
        assert_eq!(status.version, "10.10.7");
        assert_eq!(status.libraries[0].locations, ["/media/Filmes"]);
        assert_eq!(status.libraries.len(), 2);
    }
}

#[tokio::test]
async fn media_browser_sends_one_request_for_the_whole_pass() {
    let server = MockServer::start().await;
    mount_media_browser(&server).await;
    Mock::given(method("POST"))
        .and(path("/Library/Media/Updated"))
        // One request for the pass, and the paths are sorted because
        // they come out of a set — a payload a test can pin.
        .and(body_json_string(
            r#"{"Updates":[{"Path":"/media/Filmes/Heat (1995)","UpdateType":"Created"},{"Path":"/media/Series/Fringe","UpdateType":"Created"}]}"#,
        ))
        .respond_with(ResponseTemplate::new(204))
        .expect(1)
        .mount(&server)
        .await;

    let client = build(config(&server, MediaServerKind::Jellyfin, API_KEY)).unwrap();
    client
        .notify_updated(&[
            upd("/media/Filmes/Heat (1995)", "Heat (1995)", "Filmes"),
            upd("/media/Series/Fringe", "Fringe", "Series"),
        ])
        .await
        .expect("accepted");
}

#[tokio::test]
async fn media_browser_lists_its_libraries_so_it_can_re_anchor_too() {
    // This deliberately did not happen before, on the grounds that
    // `Library/Media/Updated` resolves a path server-side. It does — but
    // only a path the server recognises, and without a mapping brarr's
    // own spelling is not one. One GET buys the same zero-configuration
    // behaviour the *arr get.
    let server = MockServer::start().await;
    mount_media_browser(&server).await;
    Mock::given(method("POST"))
        .and(path("/Library/Media/Updated"))
        .and(body_json_string(
            r#"{"Updates":[{"Path":"/media/Filmes/Heat (1995)","UpdateType":"Created"}]}"#,
        ))
        .respond_with(ResponseTemplate::new(204))
        .expect(1)
        .mount(&server)
        .await;

    let client = build(config(&server, MediaServerKind::Emby, API_KEY)).unwrap();
    client
        // brarr's own spelling; the server's library is `/media/Filmes`.
        .notify_updated(&[upd("/midias/Filmes/Heat (1995)", "Heat (1995)", "Filmes")])
        .await
        .expect("re-anchored");
}

#[tokio::test]
async fn media_browser_reports_a_refused_key_as_auth() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/System/Info"))
        .respond_with(ResponseTemplate::new(401))
        .mount(&server)
        .await;

    let client = build(config(&server, MediaServerKind::Jellyfin, "wrong")).unwrap();
    let err = client.test_connection().await.expect_err("401");
    assert!(matches!(err, MediaServerError::Auth { .. }), "got {err:?}");
}

#[tokio::test]
async fn notifying_nothing_touches_no_network() {
    let server = MockServer::start().await;
    let client = build(config(&server, MediaServerKind::Emby, API_KEY)).unwrap();
    client.notify_updated(&[]).await.expect("no-op");
    assert!(server.received_requests().await.unwrap().is_empty());
}

// ─── The Plex sign-in ───────────────────────────────────────────────

fn login(server: &MockServer) -> PlexLogin {
    PlexLogin::new(PlexIdentity::new("11111111-2222-3333-4444-555555555555"))
        .unwrap()
        .with_base_url(&server.uri(), "https://app.plex.tv/auth")
}

#[tokio::test]
async fn a_pin_is_created_then_polled_until_the_token_lands() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/v2/pins"))
        .and(query_param("strong", "true"))
        .and(query_param(
            "X-Plex-Client-Identifier",
            "11111111-2222-3333-4444-555555555555",
        ))
        .and(query_param("X-Plex-Product", "brarr"))
        .respond_with(ResponseTemplate::new(201).set_body_raw(
            r#"{"id":9876,"code":"a-long-strong-code","authToken":null,"expiresIn":1799}"#,
            "application/json",
        ))
        .mount(&server)
        .await;
    // First poll: still waiting on the human.
    Mock::given(method("GET"))
        .and(path("/api/v2/pins/9876"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            r#"{"id":9876,"code":"a-long-strong-code","authToken":null}"#,
            "application/json",
        ))
        .up_to_n_times(1)
        .with_priority(1)
        .mount(&server)
        .await;
    // Second poll: approved.
    Mock::given(method("GET"))
        .and(path("/api/v2/pins/9876"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            r#"{"id":9876,"code":"a-long-strong-code","authToken":"the-account-token"}"#,
            "application/json",
        ))
        .with_priority(2)
        .mount(&server)
        .await;

    let login = login(&server);
    let pin = login.create_pin().await.expect("plex.tv issued a pin");
    assert_eq!(pin.id, 9876);
    assert_eq!(
        pin.expires_in_seconds,
        Some(1799),
        "30 minutes, as reported"
    );

    assert_eq!(login.poll_pin(pin.id).await.unwrap(), PinState::Pending);
    assert_eq!(
        login.poll_pin(pin.id).await.unwrap(),
        PinState::Authorized("the-account-token".to_owned())
    );

    // Every call has to carry the same identifier, or the token stops
    // being recognisably ours.
    for request in server.received_requests().await.unwrap() {
        assert!(
            request
                .url
                .as_str()
                .contains("X-Plex-Client-Identifier=11111111-2222-3333-4444-555555555555"),
            "missing identifier on {}",
            request.url
        );
    }
}

#[tokio::test]
async fn a_pin_plex_no_longer_knows_reads_as_expired_rather_than_as_an_error() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v2/pins/4242"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;

    // A lapsed or already-redeemed PIN is a 404, and the screen has
    // something specific to say about it — so it is a state, not an Err.
    assert_eq!(
        login(&server).poll_pin(4242).await.unwrap(),
        PinState::Expired
    );
}

#[tokio::test]
async fn the_ping_never_fails_the_caller() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v2/ping"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&server)
        .await;

    assert!(
        !login(&server).ping("the-account-token").await,
        "a plex.tv outage reports false, it does not propagate"
    );
}
